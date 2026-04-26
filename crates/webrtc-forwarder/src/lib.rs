macro_rules! verbose {
    ($($arg:tt)*) => {
        if $crate::VERBOSE.load(::std::sync::atomic::Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}

pub mod client;
pub mod ice;
mod peer;
pub mod signaling;
pub mod turn;

use ed25519_dalek::SigningKey;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha2::Sha256;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};

pub static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Enable verbose WebRTC logging if `BLIT_WEBRTC_VERBOSE=1` is set.
/// Safe to call multiple times; subsequent calls are no-ops if already enabled.
pub fn init_verbose() {
    if std::env::var("BLIT_WEBRTC_VERBOSE")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        VERBOSE.store(true, Ordering::Relaxed);
    }
}

pub const DEFAULT_HUB_URL: &str = "hub.blit.sh";

/// Resolve all local IPs the OS would route outbound traffic from.
/// Probes both IPv4 and IPv6; silently skips families that aren't available.
/// No packets are sent.
pub fn default_local_ips() -> Vec<std::net::IpAddr> {
    let mut ips = Vec::new();
    // IPv4: route toward 192.0.2.1 (TEST-NET, never routed)
    if let Ok(s) = std::net::UdpSocket::bind("0.0.0.0:0")
        && s.connect("192.0.2.1:80").is_ok()
        && let Ok(a) = s.local_addr()
    {
        ips.push(a.ip());
    }
    // IPv6: route toward 2001:4860:4860::8888 (Google DNS, globally routable)
    if let Ok(s) = std::net::UdpSocket::bind("[::]:0")
        && s.connect("[2001:4860:4860::8888]:80").is_ok()
        && let Ok(a) = s.local_addr()
    {
        let ip = a.ip();
        // Skip loopback and link-local
        if !ip.is_loopback() {
            ips.push(ip);
        }
    }
    ips
}

/// Convenience: first IPv4 (or any) local IP. Kept for callers that only need one.
pub fn default_local_ip() -> Option<std::net::IpAddr> {
    default_local_ips().into_iter().next()
}
const DEFAULT_MESSAGE_TEMPLATE: &str =
    "Terminals at https://blit.sh/s#{secret}\nRead-only: https://blit.sh/s#{ro_secret}";

pub fn normalize_hub(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        return trimmed.to_string();
    }
    if trimmed.starts_with("https://") {
        return trimmed.replacen("https://", "wss://", 1);
    }
    if trimmed.starts_with("http://") {
        return trimmed.replacen("http://", "ws://", 1);
    }
    // Check if the host portion (before any path/port) is a localhost address.
    // Use exact hostname matching to avoid false positives on hostnames like
    // "notlocalhost.evil.com" or "127.0.0.1.evil.com".
    let host = trimmed.split('/').next().unwrap_or(trimmed);
    let host = host.split(':').next().unwrap_or(host);
    if host == "localhost" || host == "127.0.0.1" || host == "[::1]" {
        return format!("ws://{trimmed}");
    }
    format!("wss://{trimmed}")
}

/// Callback to ensure the blit-proxy daemon is running.
/// Called when a proxy connection fails; should restart the proxy if needed
/// and return the socket path on success.
pub type ProxyEnsureFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync>;

pub struct Config {
    pub sock_path: String,
    pub signal_url: String,
    pub passphrase: String,
    pub message_override: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
    /// When set, per-peer IPC connections to blit-server are routed through
    /// the blit-proxy daemon at this socket path instead of connecting
    /// directly.  This lets the proxy pool local connections.
    pub proxy_sock: Option<String>,
    /// When set, called to restart the blit-proxy daemon when a proxy
    /// connection fails.  Should ensure the proxy is running and return
    /// the socket path on success.
    pub proxy_ensure: Option<ProxyEnsureFn>,
}

// ── Key derivation ──────────────────────────────────────────────────────
//
// Two-level derivation from a passphrase.  See DESIGN.md.
//
// Level 1 (from passphrase):
//   passphrase → Ed25519 signing key  (channel ID)
//   passphrase → RW consumer X25519 sk
//
// Level 2 (from Ed25519 signing key — the "RO root"):
//   ed25519_sk → Producer X25519 sk
//   ed25519_sk → RO consumer X25519 sk
//
// The RO URL contains the Ed25519 signing key (base64url).  From it the RO
// consumer can derive the producer pk and its own X25519 key, but CANNOT
// reverse PBKDF2 to recover the passphrase or the RW consumer key.
// This is a real cryptographic boundary.

const PBKDF2_ROUNDS: u32 = 100_000;
const SALT_SIGNING: &[u8] = b"https://blit.sh";
const SALT_CONSUMER_RW: &[u8] = b"blit-consumer-rw-x25519";
const SALT_PRODUCER: &[u8] = b"blit-producer-x25519";
const SALT_CONSUMER_RO: &[u8] = b"blit-consumer-ro-x25519";

fn pbkdf2_derive_str(input: &str, salt: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::<Hmac<Sha256>>(input.as_bytes(), salt, PBKDF2_ROUNDS, &mut out)
        .expect("HMAC can be initialized with any key length");
    out
}

fn pbkdf2_derive_bytes(input: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2::<Hmac<Sha256>>(input, salt, PBKDF2_ROUNDS, &mut out)
        .expect("HMAC can be initialized with any key length");
    out
}

/// Derive the Ed25519 signing key from a passphrase (level 1).
pub fn derive_signing_key(passphrase: &str) -> SigningKey {
    SigningKey::from_bytes(&pbkdf2_derive_str(passphrase, SALT_SIGNING))
}

/// Derive producer X25519 sk from the Ed25519 signing key (level 2).
fn derive_producer_x25519(ed25519_sk: &SigningKey) -> crypto_box::SecretKey {
    crypto_box::SecretKey::from(pbkdf2_derive_bytes(ed25519_sk.as_bytes(), SALT_PRODUCER))
}

/// Derive RW consumer X25519 sk from the passphrase (level 1).
fn derive_consumer_rw_x25519(passphrase: &str) -> crypto_box::SecretKey {
    crypto_box::SecretKey::from(pbkdf2_derive_str(passphrase, SALT_CONSUMER_RW))
}

/// Derive RO consumer X25519 sk from the Ed25519 signing key (level 2).
fn derive_consumer_ro_x25519(ed25519_sk: &SigningKey) -> crypto_box::SecretKey {
    crypto_box::SecretKey::from(pbkdf2_derive_bytes(ed25519_sk.as_bytes(), SALT_CONSUMER_RO))
}

/// Whether a consumer has full access or read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
}

/// All keys a producer needs.
#[derive(Clone)]
pub struct ProducerKeys {
    pub signing: SigningKey,
    pub our_secret: crypto_box::SecretKey,
    pub consumer_rw_pk: crypto_box::PublicKey,
    pub consumer_ro_pk: crypto_box::PublicKey,
}

impl ProducerKeys {
    pub fn derive(passphrase: &str) -> Self {
        let signing = derive_signing_key(passphrase);
        let our_secret = derive_producer_x25519(&signing);
        let consumer_rw_pk = derive_consumer_rw_x25519(passphrase).public_key();
        let consumer_ro_pk = derive_consumer_ro_x25519(&signing).public_key();
        Self {
            signing,
            our_secret,
            consumer_rw_pk,
            consumer_ro_pk,
        }
    }

    /// The read-only token: the Ed25519 signing key encoded as base64url.
    pub fn ro_token(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.signing.as_bytes())
    }

    /// Try to open a crypto_box sealed message from either consumer key.
    /// Returns the decrypted JSON and the consumer's access level.
    pub fn open_sealed(&self, data: &serde_json::Value) -> Option<(serde_json::Value, Access)> {
        let box_rw = BoxKeys {
            our_secret: self.our_secret.clone(),
            their_public: self.consumer_rw_pk.clone(),
        };
        if let Some(v) = signaling::open_sealed_data(data, &box_rw) {
            return Some((v, Access::ReadWrite));
        }
        let box_ro = BoxKeys {
            our_secret: self.our_secret.clone(),
            their_public: self.consumer_ro_pk.clone(),
        };
        if let Some(v) = signaling::open_sealed_data(data, &box_ro) {
            return Some((v, Access::ReadOnly));
        }
        None
    }

    pub fn box_keys_for(&self, access: Access) -> BoxKeys {
        let pk = match access {
            Access::ReadWrite => self.consumer_rw_pk.clone(),
            Access::ReadOnly => self.consumer_ro_pk.clone(),
        };
        BoxKeys {
            our_secret: self.our_secret.clone(),
            their_public: pk,
        }
    }
}

/// All keys a consumer needs.
#[derive(Clone)]
pub struct ConsumerKeys {
    pub signing: SigningKey,
    pub our_secret: crypto_box::SecretKey,
    pub producer_pk: crypto_box::PublicKey,
    pub access: Access,
}

impl ConsumerKeys {
    /// Derive RW consumer keys from the passphrase.
    pub fn derive_rw(passphrase: &str) -> Self {
        let signing = derive_signing_key(passphrase);
        let our_secret = derive_consumer_rw_x25519(passphrase);
        let producer_pk = derive_producer_x25519(&signing).public_key();
        Self {
            signing,
            our_secret,
            producer_pk,
            access: Access::ReadWrite,
        }
    }

    /// Derive RO consumer keys from the Ed25519 signing key bytes.
    /// This is what the RO token decodes to.
    pub fn derive_ro(ed25519_sk_bytes: &[u8; 32]) -> Self {
        let signing = SigningKey::from_bytes(ed25519_sk_bytes);
        let our_secret = derive_consumer_ro_x25519(&signing);
        let producer_pk = derive_producer_x25519(&signing).public_key();
        Self {
            signing,
            our_secret,
            producer_pk,
            access: Access::ReadOnly,
        }
    }

    pub fn box_keys(&self) -> BoxKeys {
        BoxKeys {
            our_secret: self.our_secret.clone(),
            their_public: self.producer_pk.clone(),
        }
    }
}

/// Holds the X25519 keys needed for a single crypto_box direction.
#[derive(Clone)]
pub struct BoxKeys {
    pub our_secret: crypto_box::SecretKey,
    pub their_public: crypto_box::PublicKey,
}

/// Parse a secret string.  If it ends with `.ro`, the prefix is a base64url-
/// encoded Ed25519 signing key (the read-only token).  Otherwise it's a
/// passphrase granting full access.
pub fn parse_consumer_secret(secret: &str) -> Result<ConsumerKeys, String> {
    if let Some(token) = secret.strip_suffix(".ro") {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|e| format!("invalid RO token: {e}"))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "RO token must decode to 32 bytes".to_string())?;
        Ok(ConsumerKeys::derive_ro(&arr))
    } else {
        Ok(ConsumerKeys::derive_rw(secret))
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct PeerState {
    handle: tokio::task::JoinHandle<()>,
    signal_tx: mpsc::UnboundedSender<serde_json::Value>,
    established: Arc<AtomicBool>,
}

struct Message {
    template: String,
    fatal: bool,
}

async fn fetch_message(signal_url_base: &str) -> Option<Message> {
    let base = signal_url_base
        .trim_end_matches('/')
        .replace("wss://", "https://")
        .replace("ws://", "http://");
    let url = format!("{base}/message");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", format!("blit/{}", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let template = body.get("template")?.as_str()?.to_string();
    let fatal = body.get("fatal").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(Message { template, fatal })
}

pub async fn run(config: Config) {
    VERBOSE.store(config.verbose, Ordering::Relaxed);
    init_verbose();
    let keys = ProducerKeys::derive(&config.passphrase);
    let public_key_hex = hex_encode(keys.signing.verifying_key().as_bytes());

    let ro_secret = format!("{}.ro", keys.ro_token());

    let (template, fatal) = match &config.message_override {
        Some(t) => (t.clone(), false),
        None => match fetch_message(&config.signal_url).await {
            Some(msg) => (msg.template, msg.fatal),
            None => (DEFAULT_MESSAGE_TEMPLATE.to_string(), false),
        },
    };
    if fatal {
        let rendered = template
            .replace("{secret}", &config.passphrase)
            .replace("{ro_secret}", &ro_secret);
        eprintln!("{rendered}");
        std::process::exit(1);
    }
    if !config.quiet {
        let rendered = template
            .replace("{secret}", &config.passphrase)
            .replace("{ro_secret}", &ro_secret);
        println!("{rendered}");
    }

    let ice_config = match ice::fetch_ice_config(&config.signal_url).await {
        Ok(cfg) => {
            verbose!("fetched ICE config ({} servers)", cfg.ice_servers.len());
            Some(cfg)
        }
        Err(e) => {
            verbose!("failed to fetch ICE config: {e}");
            None
        }
    };

    let (sig_event_tx, mut sig_event_rx) = mpsc::unbounded_channel::<signaling::Event>();
    let (sig_send_tx, sig_send_rx) = mpsc::unbounded_channel::<String>();
    let signal_url = format!(
        "{}/channel/{}/producer",
        config.signal_url.trim_end_matches('/'),
        public_key_hex,
    );

    // Don't decrypt in the signaling transport layer — the peer handler does
    // it via ProducerKeys::open_sealed so it can identify RW vs RO consumers.
    tokio::spawn(signaling::connect(
        signal_url,
        keys.signing.clone(),
        None,
        sig_event_tx,
        sig_send_rx,
    ));

    let shutdown = Arc::new(Notify::new());

    // Broadcast shutdown on SIGTERM / SIGINT so peers can send S2C_QUIT to
    // connected browsers before the process exits.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigterm = signal(SignalKind::terminate()).expect("signal handler");
                let mut sigint = signal(SignalKind::interrupt()).expect("signal handler");
                tokio::select! {
                    _ = sigterm.recv() => {}
                    _ = sigint.recv() => {}
                }
            }
            #[cfg(not(unix))]
            {
                let _ = tokio::signal::ctrl_c().await;
            }
            shutdown.notify_waiters();
        });
    }

    let mut peers: HashMap<String, PeerState> = HashMap::new();

    loop {
        let event = tokio::select! {
            ev = sig_event_rx.recv() => match ev {
                Some(e) => e,
                None => break,
            },
            _ = shutdown.notified() => break,
        };
        match event {
            signaling::Event::Registered { session_id } => {
                verbose!("registered with signaling server (session {session_id})");
                // The share URL printed at startup is only actually
                // consumable once the hub has registered us — this is the
                // earliest point at which a Type=notify systemd unit can
                // truthfully advertise readiness. Idempotent on reconnects:
                // sd_notify treats repeated READY=1 as a no-op and we also
                // don't care if the env var is unset.
                blit_sd_notify::notify_ready(false);
                // Do NOT abort unestablished peers here.  The hub will
                // re-send peer_joined for every consumer that is still
                // connected; the PeerJoined handler below replaces the peer
                // task when that happens.  Aborting here would kill a peer
                // that is mid-ICE-gathering (up to 4 s) just because the
                // signaling WS briefly dropped — exactly the race that makes
                // one forwarder connect and another not.
            }
            signaling::Event::PeerJoined { session_id } => {
                if let Some(existing) = peers.get(&session_id) {
                    if existing.established.load(Ordering::Relaxed) {
                        verbose!(
                            "ignoring duplicate peer_joined for established peer: {session_id}"
                        );
                        continue;
                    }
                    if let Some(old) = peers.remove(&session_id) {
                        old.handle.abort();
                    }
                }
                verbose!("consumer joined: {session_id}");
                let (peer_sig_tx, peer_sig_rx) = mpsc::unbounded_channel();
                let established = Arc::new(AtomicBool::new(false));
                let peer_id = session_id.clone();
                let sock = config.sock_path.clone();
                let out_tx = sig_send_tx.clone();
                let pk = keys.clone();
                let est = established.clone();
                let ice = ice_config.clone();
                let sd = shutdown.clone();
                let proxy = config.proxy_sock.clone();
                let proxy_ensure = config.proxy_ensure.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = peer::handle_peer(
                        peer_id.clone(),
                        sock,
                        peer_sig_rx,
                        out_tx,
                        pk,
                        est,
                        ice,
                        sd,
                        proxy,
                        proxy_ensure,
                    )
                    .await
                    {
                        verbose!("peer {peer_id} error: {e}");
                    }
                });
                peers.insert(
                    session_id,
                    PeerState {
                        handle,
                        signal_tx: peer_sig_tx,
                        established,
                    },
                );
            }
            signaling::Event::PeerLeft { session_id } => {
                verbose!("consumer left: {session_id}");
                if let Some(state) = peers.get(&session_id) {
                    if state.established.load(Ordering::Relaxed) {
                        // The signaling WebSocket dropped but the WebRTC
                        // data path (ICE/DTLS/SCTP) is independent and may
                        // still be alive.  Don't abort the peer handler —
                        // it has its own liveness detection via
                        // PEER_IDLE_TIMEOUT.  Just drop the signaling relay
                        // so no more SDP messages can be forwarded.
                        verbose!(
                            "peer {session_id} is established, \
                             keeping WebRTC session alive"
                        );
                        // Close the signal_tx so the peer handler's
                        // signal_rx returns None (harmless — signaling is
                        // only needed during ICE setup).
                        drop(peers.remove(&session_id));
                    } else {
                        // Not yet established — the consumer disconnected
                        // during ICE setup; tear down immediately.
                        if let Some(state) = peers.remove(&session_id) {
                            state.handle.abort();
                        }
                    }
                }
            }
            signaling::Event::Signal { from, data } => {
                if let Some(state) = peers.get(&from) {
                    let _ = state.signal_tx.send(data);
                } else {
                    verbose!("signal from unknown peer {from}, ignoring");
                }
            }
            signaling::Event::Error { message } => {
                verbose!("signaling error: {message}");
            }
        }
    }

    // On shutdown, notify all peers so they send S2C_QUIT, then give them a
    // brief window to flush the SCTP frame before the process exits.
    shutdown.notify_waiters();
    if !peers.is_empty() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
