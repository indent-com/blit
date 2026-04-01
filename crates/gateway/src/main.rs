use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::sync::{Arc, LazyLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use web_transport_quinn as wt;

/// Wraps TcpListener to set TCP_NODELAY on every accepted connection,
/// disabling Nagle's algorithm for low-latency frame delivery.
struct NoDelayListener(tokio::net::TcpListener);

impl axum::serve::Listener for NoDelayListener {
    type Io = tokio::net::TcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        {
            loop {
                match self.0.accept().await {
                    Ok((stream, addr)) => {
                        let _ = stream.set_nodelay(true);
                        return (stream, addr);
                    }
                    Err(e) => {
                        eprintln!("accept error: {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.0.local_addr()
    }
}

const INDEX_HTML: &str = include_str!("../../../js/web-app/dist/index.html");

static INDEX_ETAG: LazyLock<String> = LazyLock::new(|| blit_webserver::html_etag(INDEX_HTML));

struct Config {
    passphrase: String,
    sock_path: String,
    cors_origin: Option<String>,
    wt_cert_hash: std::sync::RwLock<Option<String>>,
    config_state: Option<blit_webserver::config::ConfigState>,
}

type AppState = Arc<Config>;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    if len > MAX_FRAME_SIZE {
        return None;
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> bool {
    if payload.len() > u32::MAX as usize {
        return false;
    }
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    writer.write_all(&buf).await.is_ok()
}

#[tokio::main]
async fn main() {
    for arg in std::env::args().skip(1) {
        if arg == "--help" || arg == "-h" {
            println!(
                "blit-gateway {} — terminal streaming WebSocket gateway",
                env!("CARGO_PKG_VERSION")
            );
            println!();
            println!("All configuration is via environment variables:");
            println!("  BLIT_PASS       Browser passphrase (required)");
            println!("  BLIT_ADDR       Listen address (default: 0.0.0.0:3264)");
            println!("  BLIT_SOCK       Upstream server socket");
            println!("  BLIT_FONT_DIRS  Colon-separated extra font directories");
            println!("  BLIT_CORS       CORS origin for font routes (* or specific origin)");
            println!("  BLIT_QUIC       Set to 1 to enable WebTransport (QUIC/HTTP3)");
            println!("  BLIT_TLS_CERT   PEM certificate file (for WebTransport)");
            println!("  BLIT_TLS_KEY    PEM private key file (for WebTransport)");
            println!("  BLIT_STORE_CONFIG  Set to 1 to sync browser settings to ~/.config/blit/blit.conf");
            std::process::exit(0);
        }
        if arg == "--version" || arg == "-V" {
            println!("blit-gateway {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
    }
    let passphrase = std::env::var("BLIT_PASS").unwrap_or_else(|_| {
        eprintln!("BLIT_PASS environment variable required");
        std::process::exit(1);
    });
    let sock_path = std::env::var("BLIT_SOCK").unwrap_or_else(|_| {
        if let Ok(dir) = std::env::var("TMPDIR") {
            return format!("{dir}/blit.sock");
        }
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            return format!("{dir}/blit.sock");
        }
        if let Ok(user) = std::env::var("USER") {
            return format!("/tmp/blit-{user}.sock");
        }
        "/tmp/blit.sock".into()
    });
    let addr = std::env::var("BLIT_ADDR").unwrap_or_else(|_| "0.0.0.0:3264".into());
    let quic_enabled = std::env::var("BLIT_QUIC")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false);

    let cors_origin = std::env::var("BLIT_CORS").ok();

    let config_state = if std::env::var("BLIT_STORE_CONFIG")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        eprintln!("config sync enabled (BLIT_STORE_CONFIG=1)");
        Some(blit_webserver::config::ConfigState::new())
    } else {
        None
    };

    let state: AppState = Arc::new(Config {
        passphrase,
        sock_path,
        cors_origin,
        wt_cert_hash: std::sync::RwLock::new(None),
        config_state,
    });

    // --- WebTransport (QUIC/HTTP3) — opt-in via BLIT_QUIC=1 ---
    if quic_enabled {
        let has_explicit_cert = std::env::var("BLIT_TLS_CERT").is_ok();
        let wt_state = state.clone();
        let wt_addr = addr.clone();
        tokio::spawn(async move {
            run_webtransport_loop(wt_state, &wt_addr, has_explicit_cert).await;
        });
    }

    let app = build_app(state.clone());

    let tcp = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| {
            eprintln!("blit-gateway: cannot bind to {addr}: {e}");
            std::process::exit(1);
        });
    let listener = NoDelayListener(tcp);
    eprintln!(
        "listening on {addr} (WebSocket{}){}",
        if quic_enabled { " + WebTransport" } else { "" },
        if quic_enabled {
            ""
        } else {
            " — set BLIT_QUIC=1 to enable WebTransport"
        },
    );

    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("blit-gateway: serve error: {e}");
        std::process::exit(1);
    }
}

fn build_app(state: AppState) -> axum::Router {
    axum::Router::new()
        .fallback(get(root_handler))
        .with_state(state)
}

async fn root_handler(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    let path = request.uri().path();

    if let Some(resp) = blit_webserver::try_font_route(path, state.cors_origin.as_deref()) {
        return resp;
    }

    let is_ws = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if is_ws && path.ends_with("/config") && state.config_state.is_some() {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| async move {
                if let Some(ref cs) = state.config_state {
                    blit_webserver::config::handle_config_ws(socket, &state.passphrase, cs).await;
                }
            }),
            Err(e) => e.into_response(),
        }
    } else if is_ws {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| handle_ws(socket, state)),
            Err(e) => e.into_response(),
        }
    } else {
        let etag = &*INDEX_ETAG;
        let inm = request.headers().get(axum::http::header::IF_NONE_MATCH);
        if inm.map(|v| v.as_bytes()) == Some(etag.as_bytes()) {
            return (
                axum::http::StatusCode::NOT_MODIFIED,
                [(axum::http::header::ETAG, etag.as_str())],
            )
                .into_response();
        }
        // Inject WebTransport cert hash for self-signed certs
        let wt_hash = state.wt_cert_hash.read().unwrap().clone();
        if let Some(hash) = &wt_hash {
            let html = INDEX_HTML.replacen(
                "<script",
                &format!("<script>window.__blitCertHash='{hash}'</script>\n<script"),
                1,
            );
            (
                [(axum::http::header::ETAG, etag.as_str())],
                axum::response::Html(html),
            )
                .into_response()
        } else {
            blit_webserver::html_response(INDEX_HTML, etag, None)
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

async fn handle_ws(mut ws: WebSocket, state: AppState) {
    let authed = loop {
        match ws.recv().await {
            Some(Ok(Message::Text(pass))) => {
                if constant_time_eq(pass.trim().as_bytes(), state.passphrase.as_bytes()) {
                    break true;
                } else {
                    let _ = ws.send(Message::Text("auth".into())).await;
                    let _ = ws.close().await;
                    break false;
                }
            }
            Some(Ok(Message::Ping(d))) => {
                let _ = ws.send(Message::Pong(d)).await;
            }
            _ => break false,
        }
    };
    if !authed {
        return;
    }
    eprintln!("client authenticated");

    let stream = match UnixStream::connect(&state.sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot connect to blit-server: {e}");
            let _ = ws.send(Message::Text(format!("error:{e}").into())).await;
            let _ = ws.close().await;
            return;
        }
    };
    let _ = ws.send(Message::Text("ok".into())).await;
    let (mut sock_reader, mut sock_writer) = stream.into_split();
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut ws_to_sock = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(d) => {
                    if !write_frame(&mut sock_writer, &d).await {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => continue,
            }
        }
    });

    let mut sock_to_ws = tokio::spawn(async move {
        while let Some(data) = read_frame(&mut sock_reader).await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut ws_to_sock => {}
        _ = &mut sock_to_ws => {}
    }
    ws_to_sock.abort();
    sock_to_ws.abort();

    eprintln!("client disconnected");
}

// ---------------------------------------------------------------------------
// WebTransport (QUIC / HTTP3)
// ---------------------------------------------------------------------------

/// Generate a self-signed certificate valid for 14 days.
/// Returns (DER cert chain, DER private key, SHA-256 hash of the leaf cert).
fn generate_self_signed_cert() -> (
    Vec<rustls_pki_types::CertificateDer<'static>>,
    rustls_pki_types::PrivateKeyDer<'static>,
    Vec<u8>,
) {
    use rcgen::{CertificateParams, KeyPair};
    use ring::digest;

    let mut params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    // WebTransport with serverCertificateHashes requires:
    //   notAfter - notBefore ≤ 14 days (exactly, not one second more)
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(14);
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = rustls_pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls_pki_types::PrivateKeyDer::try_from(key_pair.serialize_der()).unwrap();
    let hash = digest::digest(&digest::SHA256, cert_der.as_ref());
    (vec![cert_der], key_der, hash.as_ref().to_vec())
}

/// Load TLS cert/key from files (PEM).
type TlsCertMaterial = (
    Vec<rustls_pki_types::CertificateDer<'static>>,
    rustls_pki_types::PrivateKeyDer<'static>,
    Vec<u8>,
);

fn load_tls_cert(
    cert_path: &str,
    key_path: &str,
) -> Result<TlsCertMaterial, Box<dyn std::error::Error>> {
    use ring::digest;

    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;

    let certs: Vec<_> = rustls_pemfile::certs(&mut &cert_pem[..]).collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut &key_pem[..])?
        .ok_or("no private key found in PEM file")?;

    let hash = if let Some(cert) = certs.first() {
        digest::digest(&digest::SHA256, cert.as_ref())
            .as_ref()
            .to_vec()
    } else {
        vec![]
    };
    Ok((certs, key, hash))
}

/// Build a quinn ServerConfig from cert + key with the WebTransport ALPN.
fn build_quinn_server_config(
    certs: Vec<rustls_pki_types::CertificateDer<'static>>,
    key: rustls_pki_types::PrivateKeyDer<'static>,
) -> Result<wt::quinn::ServerConfig, Box<dyn std::error::Error>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    tls.alpn_protocols = vec![wt::ALPN.as_bytes().to_vec()];
    let quic_config: wt::quinn::crypto::rustls::QuicServerConfig = tls.try_into().unwrap();
    Ok(wt::quinn::ServerConfig::with_crypto(Arc::new(quic_config)))
}

fn bind_v6only_udp(addr: std::net::SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let sock = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, None)?;
    sock.set_only_v6(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// Run the WebTransport server on both IPv4 and IPv6.
/// For self-signed certs, regenerates every 13 days.
async fn run_webtransport_loop(state: AppState, addr: &str, has_explicit_cert: bool) {
    let bind_addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("webtransport: invalid address: {e}");
            return;
        }
    };
    let port = bind_addr.port();

    loop {
        let (certs, key, cert_hash) = if has_explicit_cert {
            match load_tls_cert(
                &std::env::var("BLIT_TLS_CERT").unwrap(),
                &std::env::var("BLIT_TLS_KEY").unwrap(),
            ) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("webtransport: failed to load TLS cert: {e}");
                    return;
                }
            }
        } else {
            generate_self_signed_cert()
        };

        let hash_hex: String = cert_hash.iter().map(|b| format!("{b:02x}")).collect();
        eprintln!("webtransport cert SHA-256: {hash_hex}");
        *state.wt_cert_hash.write().unwrap() = Some(hash_hex);

        let config = match build_quinn_server_config(certs, key) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("webtransport: TLS config error: {e}");
                return;
            }
        };

        // Bind both IPv4 and IPv6 so localhost (::1) and 127.0.0.1 both work.
        let v4_addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
        let v6_addr: std::net::SocketAddr = ([0, 0, 0, 0, 0, 0, 0, 0], port).into();

        let mut server4 = match wt::quinn::Endpoint::server(config.clone(), v4_addr) {
            Ok(ep) => {
                eprintln!("webtransport: listening on {v4_addr} (IPv4/QUIC)");
                wt::Server::new(ep)
            }
            Err(e) => {
                eprintln!("webtransport: IPv4 bind failed: {e}");
                return;
            }
        };
        let mut server6 = match bind_v6only_udp(v6_addr) {
            Ok(sock) => match wt::quinn::Endpoint::new(
                wt::quinn::EndpointConfig::default(),
                Some(config),
                sock,
                wt::quinn::default_runtime().unwrap(),
            ) {
                Ok(ep) => {
                    eprintln!("webtransport: listening on [{v6_addr}] (IPv6/QUIC)");
                    wt::Server::new(ep)
                }
                Err(e) => {
                    eprintln!("webtransport: IPv6 endpoint failed (continuing IPv4-only): {e}");
                    run_wt_accept_loop(&state, &mut server4, has_explicit_cert).await;
                    if has_explicit_cert {
                        return;
                    }
                    continue;
                }
            },
            Err(e) => {
                eprintln!("webtransport: IPv6 bind failed (continuing IPv4-only): {e}");
                run_wt_accept_loop(&state, &mut server4, has_explicit_cert).await;
                if has_explicit_cert {
                    return;
                }
                continue;
            }
        };

        if has_explicit_cert {
            // Production cert: accept from both forever.
            loop {
                tokio::select! {
                    req = server4.accept() => dispatch_wt_request(req, &state),
                    req = server6.accept() => dispatch_wt_request(req, &state),
                }
            }
        }

        // Self-signed cert: accept for 13 days, then regenerate.
        let rotate_after = tokio::time::sleep(std::time::Duration::from_secs(13 * 24 * 3600));
        tokio::pin!(rotate_after);
        loop {
            tokio::select! {
                req = server4.accept() => dispatch_wt_request(req, &state),
                req = server6.accept() => dispatch_wt_request(req, &state),
                _ = &mut rotate_after => {
                    eprintln!("webtransport: rotating self-signed certificate");
                    break;
                }
            }
        }
    }
}

fn dispatch_wt_request(request: Option<wt::Request>, state: &AppState) {
    if let Some(req) = request {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_webtransport_session(req, state).await {
                eprintln!("webtransport session error: {e}");
            }
        });
    }
}

async fn run_wt_accept_loop(state: &AppState, server: &mut wt::Server, permanent: bool) {
    if permanent {
        while let Some(request) = server.accept().await {
            dispatch_wt_request(Some(request), state);
        }
    } else {
        let rotate_after = tokio::time::sleep(std::time::Duration::from_secs(13 * 24 * 3600));
        tokio::pin!(rotate_after);
        loop {
            tokio::select! {
                req = server.accept() => dispatch_wt_request(req, state),
                _ = &mut rotate_after => {
                    eprintln!("webtransport: rotating self-signed certificate");
                    break;
                }
            }
        }
    }
}

async fn handle_webtransport_session(
    request: wt::Request,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session = request.ok().await?;

    // Accept a bidirectional stream for the blit protocol
    let (mut send, mut recv) = session.accept_bi().await?;

    // --- Authentication ---
    // Read passphrase: 2-byte LE length + UTF-8 string
    let mut len_buf = [0u8; 2];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("auth read len: {e}"))?;
    let pass_len = u16::from_le_bytes(len_buf) as usize;
    if pass_len > 4096 {
        return Err("passphrase too long".into());
    }
    let mut pass_buf = vec![0u8; pass_len];
    recv.read_exact(&mut pass_buf)
        .await
        .map_err(|e| format!("auth read pass: {e}"))?;
    let pass = std::str::from_utf8(&pass_buf).unwrap_or("");

    if !constant_time_eq(pass.trim().as_bytes(), state.passphrase.as_bytes()) {
        send.write_all(&[0]).await.ok(); // 0 = rejected
        return Err("authentication failed".into());
    }
    send.write_all(&[1])
        .await
        .map_err(|e| format!("auth write ok: {e}"))?; // 1 = ok
    eprintln!("webtransport client authenticated");

    // --- Proxy to blit-server ---
    let stream = UnixStream::connect(&state.sock_path).await?;
    let (mut sock_reader, mut sock_writer) = stream.into_split();

    // Client → server: read length-prefixed frames from WebTransport, forward to Unix socket
    let mut client_to_sock = tokio::spawn(async move {
        loop {
            let mut len_buf = [0u8; 4];
            if recv.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > MAX_FRAME_SIZE {
                break;
            }
            let mut buf = vec![0u8; len];
            if len > 0 && recv.read_exact(&mut buf).await.is_err() {
                break;
            }
            if !write_frame(&mut sock_writer, &buf).await {
                break;
            }
        }
    });

    // Server → client: read length-prefixed frames from Unix socket, forward to WebTransport
    let mut sock_to_client = tokio::spawn(async move {
        while let Some(data) = read_frame(&mut sock_reader).await {
            let len = (data.len() as u32).to_le_bytes();
            if send.write_all(&len).await.is_err() {
                break;
            }
            if !data.is_empty() && send.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut client_to_sock => {}
        _ = &mut sock_to_client => {}
    }
    client_to_sock.abort();
    sock_to_client.abort();

    eprintln!("webtransport client disconnected");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_app() -> axum::Router {
        let state: AppState = Arc::new(Config {
            passphrase: "test".into(),
            sock_path: "/nonexistent.sock".into(),
            cors_origin: None,
            wt_cert_hash: std::sync::RwLock::new(None),
            config_state: None,
        });
        build_app(state)
    }

    // --- HTTP integration tests ---

    #[tokio::test]
    async fn get_root_returns_index_html() {
        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"), "expected text/html, got {ct}");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert!(body.len() > 100);
    }

    #[tokio::test]
    async fn get_subpath_returns_index_html() {
        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/vt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // /vt has no matching static asset filename "vt", so falls through to index.html
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"), "expected text/html, got {ct}");
    }

    #[tokio::test]
    async fn any_path_returns_index_html() {
        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/vt/nonexistent_file.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/html"));
    }

    #[tokio::test]
    async fn prefixed_fonts_returns_json() {
        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/vt/fonts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/json"),
            "expected application/json, got {ct}"
        );
    }
}
