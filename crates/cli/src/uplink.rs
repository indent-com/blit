//! `blit uplink` — expose the local blit server through a relay.
//!
//! Authenticates to an HTTPS control endpoint with `BLIT_UPLINK_TOKEN`,
//! receives a pool of WebTransport relays,
//! establish a session with one, and bridge each relay-initiated
//! bidirectional stream to the local blit server socket.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use web_transport_quinn as wt;

/// Stream application error codes.
const CODE_LOCAL_UNAVAILABLE: u32 = 1;
const CODE_SHUTDOWN: u32 = 2;
const CODE_LOCAL_ERROR: u32 = 3;

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

struct Relay {
    /// Connection URL as minted by the control plane (fragment stripped).
    /// The URL is the session credential — never log it; use `label`.
    url: url::Url,
    /// `host:port` for log messages.
    label: String,
    /// SHA-256 of the relay's certificate (DER), from the URL's `#sha256=`
    /// fragment; pins TLS verification instead of using system trust roots.
    cert_hash: Option<Vec<u8>>,
}

pub async fn cmd_uplink(url: String) -> Result<(), String> {
    let token = std::env::var("BLIT_UPLINK_TOKEN").unwrap_or_default();
    if token.is_empty() {
        return Err("BLIT_UPLINK_TOKEN is not set".into());
    }

    // The active session, shared with the ctrl-c arm so shutdown can close
    // it with CODE_SHUTDOWN instead of letting it idle out on the relay.
    let current: Arc<Mutex<Option<wt::Session>>> = Arc::new(Mutex::new(None));
    let current2 = current.clone();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            if let Some(session) = current2.lock().unwrap().take() {
                session.close(CODE_SHUTDOWN, b"uplink shutting down");
            }
            Ok(())
        }
        result = run_loop(&url, &token, current) => result,
    }
}

async fn run_loop(
    url: &str,
    token: &str,
    current: Arc<Mutex<Option<wt::Session>>>,
) -> Result<(), String> {
    let http = reqwest::Client::new();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        let mut pool = match fetch_pool(&http, url, token).await? {
            FetchOutcome::Pool(pool) => pool,
            FetchOutcome::Retry { after, reason } => {
                let delay = after.unwrap_or(backoff);
                eprintln!("[uplink] {reason}; retrying in {}s", delay.as_secs());
                tokio::time::sleep(jittered(delay)).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        {
            use rand::seq::SliceRandom;
            pool.shuffle(&mut rand::rng());
        }

        // Try relays in (shuffled) order; a session that was actually
        // established ends with a fresh control-plane query, per
        // docs/uplink.md.
        let mut established = false;
        for relay in &pool {
            match run_session(relay, &current).await {
                SessionEnd::Ended(reason) => {
                    eprintln!("[uplink] session ended: {reason}");
                    established = true;
                    break;
                }
                SessionEnd::NeverConnected(e) => {
                    eprintln!("[uplink] relay {}: {e}", relay.label);
                }
            }
        }

        if established {
            backoff = INITIAL_BACKOFF;
        } else {
            eprintln!(
                "[uplink] relay pool exhausted; re-querying in {}s",
                backoff.as_secs()
            );
            tokio::time::sleep(jittered(backoff)).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }
}

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

enum FetchOutcome {
    Pool(Vec<Relay>),
    Retry {
        after: Option<Duration>,
        reason: String,
    },
}

/// Query the control endpoint. `Err` is fatal (bad token); every other
/// failure is a retryable `FetchOutcome::Retry`.
async fn fetch_pool(
    http: &reqwest::Client,
    url: &str,
    token: &str,
) -> Result<FetchOutcome, String> {
    let retry = |after, reason| Ok(FetchOutcome::Retry { after, reason });

    let resp = match http
        .get(url)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/json")
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => return retry(None, format!("control endpoint unreachable: {e}")),
    };

    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(format!(
            "control endpoint rejected BLIT_UPLINK_TOKEN ({status})"
        ));
    }
    if !status.is_success() {
        let after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        return retry(after, format!("control endpoint returned {status}"));
    }

    let body = match resp.text().await {
        Ok(body) => body,
        Err(e) => return retry(None, format!("error reading relay pool: {e}")),
    };
    match parse_pool(&body) {
        Ok(pool) => Ok(FetchOutcome::Pool(pool)),
        Err(e) => retry(None, format!("bad relay pool: {e}")),
    }
}

fn parse_pool(body: &str) -> Result<Vec<Relay>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("invalid JSON: {e}"))?;
    let relays = v
        .get("relays")
        .and_then(|r| r.as_array())
        .ok_or("missing \"relays\" array")?;

    let mut pool = Vec::new();
    for relay in relays {
        let s = relay.as_str().ok_or("relay entries must be URL strings")?;
        pool.push(parse_relay(s)?);
    }
    if pool.is_empty() {
        return Err("empty \"relays\" array".into());
    }
    Ok(pool)
}

fn parse_relay(s: &str) -> Result<Relay, String> {
    let mut url = url::Url::parse(s).map_err(|e| format!("bad relay URL: {e}"))?;
    if url.scheme() != "https" {
        return Err(format!(
            "relay URL scheme must be https, got {}",
            url.scheme()
        ));
    }
    let host = url.host_str().ok_or("relay URL has no host")?.to_string();
    let label = format!("{}:{}", host, url.port().unwrap_or(443));

    // A malformed pin must be an error, never a silent fall-back to system
    // roots — that would defeat the pinning.
    let cert_hash = match url.fragment() {
        None | Some("") => None,
        Some(frag) => {
            let hash = frag
                .strip_prefix("sha256=")
                .ok_or("relay URL fragment must be sha256=<base64url hash>")?;
            let bytes =
                base64url_decode(hash).ok_or("relay certificate pin is not valid base64url")?;
            if bytes.len() != 32 {
                return Err("relay certificate pin must be a SHA-256 (32 bytes)".into());
            }
            Some(bytes)
        }
    };
    // Fragments are client-side only; strip before connecting.
    url.set_fragment(None);

    Ok(Relay {
        url,
        label,
        cert_hash,
    })
}

fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Leftover bits must be padding zeros of a valid encoding.
    if bits > 0 && (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Relay session
// ---------------------------------------------------------------------------

enum SessionEnd {
    /// Handshake or CONNECT failed — try the next relay in the pool.
    NeverConnected(String),
    /// The session was established and later died — re-query the control
    /// plane for a fresh pool.
    Ended(String),
}

async fn run_session(relay: &Relay, current: &Arc<Mutex<Option<wt::Session>>>) -> SessionEnd {
    let client = match build_client(relay.cert_hash.as_deref()) {
        Ok(client) => client,
        Err(e) => return SessionEnd::NeverConnected(e),
    };
    // Careful: the URL is the credential — log `relay.label` only.
    let session = match client.connect(relay.url.clone()).await {
        Ok(session) => session,
        Err(e) => return SessionEnd::NeverConnected(format!("connect failed: {e}")),
    };
    eprintln!("[uplink] connected to relay {}", relay.label);
    *current.lock().unwrap() = Some(session.clone());

    let reason = loop {
        match session.accept_bi().await {
            Ok((send, recv)) => {
                tokio::spawn(bridge(send, recv));
            }
            Err(e) => break format!("{e}"),
        }
    };
    current.lock().unwrap().take();
    SessionEnd::Ended(reason)
}

/// Bridge one relay-initiated stream to one local blit server connection.
/// Stream payload is the blit wire protocol, unparsed.
async fn bridge(mut send: wt::SendStream, mut recv: wt::RecvStream) {
    let path = crate::transport::default_local_socket();
    let transport = match crate::transport::connect_ipc(&path).await {
        Ok(transport) => transport,
        Err(e) => {
            eprintln!("[uplink] local blit server unavailable at {path}: {e}");
            let _ = send.reset(CODE_LOCAL_UNAVAILABLE);
            let _ = recv.stop(CODE_LOCAL_UNAVAILABLE);
            return;
        }
    };
    let (mut sock_read, mut sock_write) = transport.split();

    let down = async move {
        use tokio::io::AsyncWriteExt;
        match tokio::io::copy(&mut recv, &mut sock_write).await {
            // Peer FIN → shut down the socket's write side.
            Ok(_) => {
                let _ = sock_write.shutdown().await;
            }
            Err(_) => {
                let _ = recv.stop(CODE_LOCAL_ERROR);
            }
        }
    };
    let up = async move {
        match tokio::io::copy(&mut sock_read, &mut send).await {
            // Socket EOF → stream FIN.
            Ok(_) => {
                let _ = send.finish();
            }
            Err(_) => {
                let _ = send.reset(CODE_LOCAL_ERROR);
            }
        }
    };
    tokio::join!(down, up);
}

/// Build a WebTransport client with the liveness settings from
/// docs/uplink.md (10s keepalive, 30s idle timeout) and either
/// system-root or pinned TLS verification.  `wt::ClientBuilder` doesn't expose the quinn transport
/// config, so this mirrors its setup by hand.
fn build_client(cert_hash: Option<&[u8]>) -> Result<wt::Client, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| format!("TLS config: {e}"))?;

    let mut crypto = match cert_hash {
        Some(hash) => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCert {
                hash: hash.to_vec(),
                provider,
            }))
            .with_no_client_auth(),
        None => {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
    };
    crypto.alpn_protocols = vec![wt::ALPN.as_bytes().to_vec()];

    let quic_crypto = wt::quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .map_err(|e| format!("QUIC TLS config: {e}"))?;
    let mut config = wt::quinn::ClientConfig::new(Arc::new(quic_crypto));

    let mut transport = wt::quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    transport.max_idle_timeout(Some(
        wt::quinn::IdleTimeout::try_from(Duration::from_secs(30))
            .expect("30s fits in an idle timeout"),
    ));
    config.transport_config(Arc::new(transport));

    let endpoint = wt::quinn::Endpoint::client("[::]:0".parse().unwrap())
        .or_else(|_| wt::quinn::Endpoint::client("0.0.0.0:0".parse().unwrap()))
        .map_err(|e| format!("UDP socket: {e}"))?;
    Ok(wt::Client::new(endpoint, config))
}

/// Pins the relay's end-entity certificate to a SHA-256 hash from the pool
/// (the gateway's `serverCertificateHashes` flow, client side).  Chain and
/// expiry are deliberately not checked — the hash is the trust anchor.
#[derive(Debug)]
struct PinnedCert {
    hash: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
        if digest.as_ref() == self.hash.as_slice() {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::PeerIncompatible(
            rustls::PeerIncompatible::Tls12NotOffered,
        ))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn jittered(base: Duration) -> Duration {
    use rand::RngExt as _;
    let ms = base.as_millis().max(1) as u64;
    // 0.75x–1.25x
    Duration::from_millis(rand::rng().random_range(ms * 3 / 4..=ms * 5 / 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pool_accepts_plain_and_pinned_relay_urls() {
        let pool = parse_pool(
            r#"{"relays":[
                "https://relay-1.indent.com:4443/t/kfV3aB",
                "https://[2001:db8::7]/session?key=x#sha256=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            ],"ttl":60}"#,
        )
        .unwrap();
        assert_eq!(pool.len(), 2);
        assert_eq!(
            pool[0].url.as_str(),
            "https://relay-1.indent.com:4443/t/kfV3aB"
        );
        assert_eq!(pool[0].label, "relay-1.indent.com:4443");
        assert!(pool[0].cert_hash.is_none());
        assert_eq!(pool[1].label, "[2001:db8::7]:443");
        assert_eq!(pool[1].cert_hash.as_ref().unwrap().len(), 32);
        // The pin must be stripped before the URL is used to connect.
        assert_eq!(pool[1].url.fragment(), None);
        assert_eq!(pool[1].url.query(), Some("key=x"));
    }

    #[test]
    fn parse_pool_rejects_bad_input() {
        assert!(parse_pool("not json").is_err());
        assert!(parse_pool(r#"{"relays":[]}"#).is_err());
        assert!(parse_pool(r#"{"relays":[{"host":"h"}]}"#).is_err());
        assert!(
            parse_relay("http://relay.example/t/x").is_err(),
            "non-https scheme must be rejected"
        );
        assert!(
            parse_relay("https://relay.example/t/x#sha256=AAAA").is_err(),
            "a pin of the wrong length must be rejected, not ignored"
        );
        assert!(
            parse_relay("https://relay.example/t/x#pin=abc").is_err(),
            "an unrecognized fragment must be rejected, not ignored"
        );
    }

    #[test]
    fn base64url_decodes() {
        assert_eq!(base64url_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64url_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64url_decode("_-8").unwrap(), vec![0xff, 0xef]);
        assert!(base64url_decode("a+b").is_none());
        assert_eq!(base64url_decode("").unwrap(), Vec::<u8>::new());
    }
}
