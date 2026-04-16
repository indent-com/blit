use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use web_transport_quinn as wt;

// ---------------------------------------------------------------------------
// Gateway connector: one per named destination.
// ---------------------------------------------------------------------------

/// How the gateway handles a named destination.
#[derive(Clone)]
enum GatewayConnector {
    /// Unix socket (or Windows named pipe) path.
    Ipc(String),
    /// Raw TCP address (host:port).
    Tcp(String),
    /// WebRTC share session — the gateway connects to the hub as a consumer
    /// and bridges the resulting stream to the browser over WebSocket/WebTransport.
    Share {
        /// Passphrase (the secret after `share:`).
        passphrase: String,
        /// Signaling hub WebSocket URL (ws:// or wss://).
        signal_url: String,
    },
    /// Embedded SSH connection via the shared pool.
    Ssh {
        pool: blit_ssh::SshPool,
        user: Option<String>,
        host: String,
        socket: Option<String>,
    },
}

type BoxedReader = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
type BoxedWriter = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;

#[cfg(unix)]
type IpcStream = tokio::net::UnixStream;
#[cfg(windows)]
type IpcStream = tokio::net::windows::named_pipe::NamedPipeClient;

async fn connect_ipc(path: &str) -> Result<IpcStream, String> {
    #[cfg(unix)]
    {
        UnixStream::connect(path)
            .await
            .map_err(|e| format!("cannot connect to {path}: {e}"))
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        ClientOptions::new()
            .open(path)
            .map_err(|e| format!("cannot connect to {path}: {e}"))
    }
}

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

const INDEX_HTML_BR: &[u8] = include_bytes!("../../../js/ui/dist/index.html.br");

static INDEX_ETAG: LazyLock<String> = LazyLock::new(|| blit_webserver::html_etag(INDEX_HTML_BR));

type DestMap = std::collections::HashMap<String, GatewayConnector>;

struct Config {
    passphrase: String,
    /// Resolved connectors for routing WebSocket/WebTransport connections.
    /// Derived from `remotes` on startup and reconciled on file changes.
    destinations: std::sync::RwLock<DestMap>,
    /// Live-reloading `blit.remotes` file — the persistent source of truth
    /// for the remote list.  The file watcher drives `destinations` updates.
    remotes: blit_webserver::config::RemotesState,
    cors_origin: Option<String>,
    wt_cert_hash: std::sync::RwLock<Option<String>>,
    config_state: blit_webserver::config::ConfigState,
    /// When `BLIT_PROXY=1`, all proxiable upstream connections are routed
    /// through this blit-proxy socket path instead of connecting directly.
    proxy_sock: Option<String>,
    /// Shared SSH connection pool for `ssh:` remotes.
    ssh_pool: blit_ssh::SshPool,
    /// Default hub URL used when a `share:` remote doesn't include `?hub=`.
    /// Read from `BLIT_HUB` at startup; falls back to `hub.blit.sh`.
    hub_url: String,
    /// When `BLIT_GATEWAY_WEBRTC=1`, the gateway connects to `share:` remotes
    /// as a WebRTC consumer and bridges them to browsers over
    /// WebSocket/WebTransport.  Without this flag, `share:` entries in
    /// blit.remotes are ignored by the gateway.
    webrtc_enabled: bool,
    /// Broadcast notification triggered on SIGINT/SIGTERM so active
    /// WebSocket/WebTransport handlers can send `S2C_QUIT` before exit.
    shutdown: Arc<tokio::sync::Notify>,
}

impl Config {
    /// Get a connector for a named destination, returning a cloned snapshot
    /// that can be used after the lock is released.
    fn connector_for(&self, name: &str) -> Option<ConnectorSnapshot> {
        let destinations = self.destinations.read().unwrap();
        let connector = destinations.get(name)?;
        Some(match connector {
            GatewayConnector::Share {
                passphrase,
                signal_url,
            } => {
                // Route through blit-proxy when enabled — the proxy pools
                // WebRTC sessions so page reloads reuse the existing session.
                if let Some(proxy) = &self.proxy_sock {
                    let proxy_uri = share_proxy_uri(passphrase, signal_url);
                    ConnectorSnapshot::Proxied(proxy.clone(), proxy_uri)
                } else {
                    ConnectorSnapshot::Share {
                        passphrase: passphrase.clone(),
                        signal_url: signal_url.clone(),
                    }
                }
            }
            GatewayConnector::Ssh {
                pool,
                user,
                host,
                socket,
            } => {
                if let Some(proxy) = &self.proxy_sock {
                    let mut uri = format!("ssh:{host}");
                    if let Some(u) = user {
                        uri = format!("ssh:{u}@{host}");
                    }
                    if let Some(s) = socket {
                        uri.push_str(&format!("/{s}"));
                    }
                    ConnectorSnapshot::Proxied(proxy.clone(), uri)
                } else {
                    ConnectorSnapshot::Ssh {
                        pool: pool.clone(),
                        user: user.clone(),
                        host: host.clone(),
                        socket: socket.clone(),
                    }
                }
            }
            // For proxiable connectors, route through blit-proxy when enabled.
            conn => {
                if let Some(proxy) = &self.proxy_sock {
                    let upstream_uri = match conn {
                        GatewayConnector::Ipc(p) => format!("socket:{p}"),
                        GatewayConnector::Tcp(a) => format!("tcp:{a}"),
                        _ => unreachable!(),
                    };
                    ConnectorSnapshot::Proxied(proxy.clone(), upstream_uri)
                } else {
                    match conn {
                        GatewayConnector::Ipc(p) => ConnectorSnapshot::Ipc(p.clone()),
                        GatewayConnector::Tcp(a) => ConnectorSnapshot::Tcp(a.clone()),
                        _ => unreachable!(),
                    }
                }
            }
        })
    }
}

/// Convert a `blit.remotes` URI entry to a `GatewayConnector`.
/// `hub_url` is the default signaling hub (from `BLIT_HUB` or the blit default).
/// `webrtc_enabled` gates whether `share:` entries are proxied; when false they
/// are skipped (returns `None`).
fn uri_to_connector(
    uri: &str,
    ssh_pool: &blit_ssh::SshPool,
    hub_url: &str,
    webrtc_enabled: bool,
) -> Option<GatewayConnector> {
    if let Some(rest) = uri.strip_prefix("ssh:") {
        let (user, host, socket) = blit_ssh::parse_ssh_uri(rest);
        return Some(GatewayConnector::Ssh {
            pool: ssh_pool.clone(),
            user,
            host,
            socket,
        });
    }
    if let Some(path) = uri.strip_prefix("socket:") {
        return Some(GatewayConnector::Ipc(path.to_string()));
    }
    if let Some(addr) = uri.strip_prefix("tcp:") {
        return Some(GatewayConnector::Tcp(addr.to_string()));
    }
    if let Some(rest) = uri.strip_prefix("share:") {
        if !webrtc_enabled {
            return None;
        }
        // Accepts:
        //   share:PASSPHRASE
        //   share:PASSPHRASE?hub=wss://custom.hub
        let (passphrase, signal_url) = if let Some(q) = rest.find('?') {
            let pass = &rest[..q];
            let params = url::form_urlencoded::parse(&rest.as_bytes()[q + 1..]);
            let hub = params
                .into_iter()
                .find(|(k, _)| k == "hub")
                .map(|(_, v)| blit_webrtc_forwarder::normalize_hub(&v))
                .unwrap_or_else(|| hub_url.to_string());
            (pass.to_string(), hub)
        } else {
            (rest.to_string(), hub_url.to_string())
        };
        return Some(GatewayConnector::Share {
            passphrase,
            signal_url,
        });
    }
    if uri == "local" {
        let path = blit_webserver::config::default_local_socket();
        return Some(GatewayConnector::Ipc(path));
    }
    None
}

/// Reconcile the live `destinations` map to match a new remotes snapshot.
fn reconcile_destinations(
    destinations: &std::sync::RwLock<DestMap>,
    entries: &[(String, String)],
    ssh_pool: &blit_ssh::SshPool,
    hub_url: &str,
    webrtc_enabled: bool,
) {
    let mut map = destinations.write().unwrap();
    // Preserve "default" (the local IPC socket set at startup as a fallback).
    map.retain(|name, _| name == "default" || entries.iter().any(|(n, _)| n == name));
    for (name, uri) in entries {
        if let Some(c) = uri_to_connector(uri, ssh_pool, hub_url, webrtc_enabled) {
            map.insert(name.clone(), c);
        }
    }
}

/// A lock-free snapshot of a connector's routing info for use after the
/// destinations lock is released.
enum ConnectorSnapshot {
    Ipc(String),
    Tcp(String),
    /// Route through blit-proxy: (proxy_sock_path, upstream_uri).
    Proxied(String, String),
    /// WebRTC share session: connect directly to the hub.
    Share {
        passphrase: String,
        signal_url: String,
    },
    /// Embedded SSH via the shared pool.
    Ssh {
        pool: blit_ssh::SshPool,
        user: Option<String>,
        host: String,
        socket: Option<String>,
    },
}

impl ConnectorSnapshot {
    async fn connect(&self) -> Result<(BoxedReader, BoxedWriter), String> {
        match self {
            ConnectorSnapshot::Ipc(path) => {
                let stream = connect_ipc(path).await?;
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w)))
            }
            ConnectorSnapshot::Tcp(addr) => {
                let stream = tokio::net::TcpStream::connect(addr.as_str())
                    .await
                    .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
                let _ = stream.set_nodelay(true);
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w)))
            }
            ConnectorSnapshot::Proxied(proxy_sock, upstream_uri) => {
                proxy_connect(proxy_sock, upstream_uri).await
            }
            ConnectorSnapshot::Share {
                passphrase,
                signal_url,
            } => {
                let stream = blit_webrtc_forwarder::client::connect(passphrase, signal_url)
                    .await
                    .map_err(|e| format!("share: {e}"))?;
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w)))
            }
            ConnectorSnapshot::Ssh {
                pool,
                user,
                host,
                socket,
            } => {
                let stream = pool
                    .connect(host, user.as_deref(), socket.as_deref())
                    .await
                    .map_err(|e| format!("ssh:{host}: {e}"))?;
                let (r, w) = tokio::io::split(stream);
                Ok((Box::new(r), Box::new(w)))
            }
        }
    }
}

/// Connect to `upstream_uri` via the blit-proxy at `proxy_sock`.
/// Performs the `target <uri>\n` / `ok\n` handshake.
///
/// If the proxy socket is unreachable, attempts to restart the proxy daemon
/// via `blit_proxy::ensure_proxy` and retries once.
#[cfg(unix)]
async fn proxy_connect(
    proxy_sock: &str,
    upstream_uri: &str,
) -> Result<(BoxedReader, BoxedWriter), String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut stream = match tokio::net::UnixStream::connect(proxy_sock).await {
        Ok(s) => s,
        Err(first_err) => {
            // Proxy socket is unreachable — attempt to restart the daemon.
            let exe = std::env::current_exe().unwrap_or_default();
            match blit_proxy::ensure_proxy(&exe, true).await {
                Ok(sock) => {
                    eprintln!("blit gateway: proxy restarted → {sock}");
                    tokio::net::UnixStream::connect(&sock).await.map_err(|e| {
                        format!("blit-proxy {sock}: {e} (after restart, original: {first_err})")
                    })?
                }
                Err(re) => {
                    return Err(format!(
                        "blit-proxy {proxy_sock}: {first_err} (restart failed: {re})"
                    ));
                }
            }
        }
    };

    let msg = format!("target {upstream_uri}\n");
    AsyncWriteExt::write_all(&mut stream, msg.as_bytes())
        .await
        .map_err(|e| format!("blit-proxy handshake write: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader
        .read_line(&mut resp)
        .await
        .map_err(|e| format!("blit-proxy handshake read: {e}"))?;
    let resp = resp.trim_end_matches('\n').trim_end_matches('\r');
    if resp == "ok" {
        let stream = reader.into_inner();
        let (r, w) = tokio::io::split(stream);
        Ok((Box::new(r), Box::new(w)))
    } else if let Some(msg) = resp.strip_prefix("error ") {
        Err(format!("blit-proxy: {msg}"))
    } else {
        Err(format!("blit-proxy: unexpected response: {resp:?}"))
    }
}

#[cfg(not(unix))]
async fn proxy_connect(
    _proxy_sock: &str,
    _upstream_uri: &str,
) -> Result<(BoxedReader, BoxedWriter), String> {
    Err("blit-proxy is not supported on this platform".into())
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

/// Run the gateway.  Reads all configuration from environment variables
/// (`BLIT_PASSPHRASE`, `BLIT_ADDR`, `BLIT_REMOTES`, …).  Does not return
/// under normal operation.
pub async fn run() {
    let passphrase = std::env::var("BLIT_PASSPHRASE").unwrap_or_else(|_| {
        eprintln!("BLIT_PASSPHRASE environment variable required");
        std::process::exit(1);
    });
    let ssh_pool = blit_ssh::SshPool::new();

    // When BLIT_GATEWAY_WEBRTC=1, proxy share: remotes via WebRTC.
    let webrtc_enabled = std::env::var("BLIT_GATEWAY_WEBRTC").ok().as_deref() == Some("1");

    // Resolve the default hub URL from BLIT_HUB (or fall back to hub.blit.sh).
    let hub_url = std::env::var("BLIT_HUB")
        .map(|h| blit_webrtc_forwarder::normalize_hub(&h))
        .unwrap_or_else(|_| {
            blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL)
        });

    // Build destinations from blit.remotes (live-reloaded, 0600).
    // BLIT_REMOTES overrides the file path (honoured by RemotesState::new()).
    let mut destinations: std::collections::HashMap<String, GatewayConnector> =
        std::collections::HashMap::new();

    let remotes = blit_webserver::config::RemotesState::new();
    let initial_remotes = blit_webserver::config::parse_remotes_str(&remotes.get());
    for (name, uri) in &initial_remotes {
        if let Some(connector) = uri_to_connector(uri, &ssh_pool, &hub_url, webrtc_enabled) {
            destinations.insert(name.clone(), connector);
        }
    }

    let addr = std::env::var("BLIT_ADDR").unwrap_or_else(|_| "0.0.0.0:3264".into());
    let quic_enabled = std::env::var("BLIT_QUIC")
        .ok()
        .map(|v| v == "1")
        .unwrap_or(false);

    let cors_origin = std::env::var("BLIT_CORS").ok();
    let config_state = blit_webserver::config::ConfigState::new();

    // Route all proxiable upstream connections through blit-proxy unless
    // explicitly disabled with BLIT_PROXY=0.  The proxy is auto-started as
    // a daemon via `blit proxy-daemon` (same binary).
    let proxy_sock: Option<String> = if std::env::var("BLIT_PROXY").ok().as_deref() == Some("0") {
        None
    } else {
        let exe = std::env::current_exe().unwrap_or_default();
        match blit_proxy::ensure_proxy(&exe, true).await {
            Ok(sock) => {
                eprintln!("blit gateway: proxy enabled → {sock}");
                Some(sock)
            }
            Err(e) => {
                eprintln!("blit gateway: proxy auto-start failed: {e}");
                None
            }
        }
    };

    let shutdown = Arc::new(tokio::sync::Notify::new());

    let state: AppState = Arc::new(Config {
        passphrase,
        destinations: std::sync::RwLock::new(destinations),
        remotes,
        cors_origin,
        wt_cert_hash: std::sync::RwLock::new(None),
        config_state,
        proxy_sock,
        ssh_pool,
        hub_url,
        webrtc_enabled,
        shutdown: shutdown.clone(),
    });

    // --- Reconcile destinations whenever blit.remotes changes ---
    {
        let recon_state = state.clone();
        let mut remotes_rx = recon_state.remotes.subscribe();
        tokio::spawn(async move {
            loop {
                let text = match remotes_rx.recv().await {
                    Ok(t) => t,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        recon_state.remotes.get()
                    }
                    Err(_) => break,
                };
                let entries = blit_webserver::config::parse_remotes_str(&text);
                reconcile_destinations(
                    &recon_state.destinations,
                    &entries,
                    &recon_state.ssh_pool,
                    &recon_state.hub_url,
                    recon_state.webrtc_enabled,
                );
            }
        });
    }

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
            eprintln!("blit gateway: cannot bind to {addr}: {e}");
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

    let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
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
        // Notify all active handlers so they can send S2C_QUIT.
        shutdown.notify_waiters();
    });
    if let Err(e) = graceful.await {
        eprintln!("blit gateway: serve error: {e}");
        std::process::exit(1);
    }
}

/// Rewrite `share:PASSPHRASE` URIs in remotes text to `share:PASSPHRASE?proxiable=true`
/// so the browser knows the gateway is proxying them via WebRTC.
fn mark_share_remotes_proxiable(remotes_text: &str) -> String {
    remotes_text
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                return line.to_string();
            }
            if let Some(eq) = line.find('=') {
                let uri = line[eq + 1..].trim();
                if uri.to_lowercase().starts_with("share:") && !uri.contains("proxiable=true") {
                    let sep = if uri.contains('?') { "&" } else { "?" };
                    let name_part = &line[..eq + 1];
                    return format!("{name_part} {uri}{sep}proxiable=true");
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build a `share:` URI suitable for handing to blit-proxy.
/// Embeds the hub URL as a query param only when it differs from the default.
fn share_proxy_uri(passphrase: &str, signal_url: &str) -> String {
    let default_hub = blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL);
    if signal_url == default_hub {
        format!("share:{passphrase}")
    } else {
        format!("share:{passphrase}?hub={signal_url}")
    }
}

fn build_app(state: AppState) -> axum::Router {
    axum::Router::new()
        .fallback(get(root_handler))
        .with_state(state)
}

/// Resolve which destination a request is for from the path.
/// `/d/{name}` or `/<prefix>/d/{name}` -> named destination.
/// Everything else -> None (default/first destination).
fn resolve_destination_name(path: &str) -> Option<String> {
    // Look for "/d/" anywhere in the path (supports base-path prefixes).
    if let Some(pos) = path.find("/d/") {
        let rest = &path[pos + 3..];
        let name = rest.split('/').next().unwrap_or(rest);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

/// Returns true when `path` ends with `/mux` (or equals `/mux`).
fn is_mux_path(path: &str) -> bool {
    path == "/mux" || path.ends_with("/mux")
}

// ---------------------------------------------------------------------------
// Multiplexed WebSocket protocol constants.
// ---------------------------------------------------------------------------

/// Reserved channel ID for control messages.
const MUX_CONTROL: u16 = 0xFFFF;

/// Client → Server: open a channel.  `[channel_id:2][name_len:2][name:N]`
const MUX_C2S_OPEN: u8 = 0x01;
/// Client → Server: close a channel. `[channel_id:2]`
const MUX_C2S_CLOSE: u8 = 0x02;

/// Server → Client: channel opened.  `[channel_id:2]`
const MUX_S2C_OPENED: u8 = 0x81;
/// Server → Client: channel closed.  `[channel_id:2]`
const MUX_S2C_CLOSED: u8 = 0x82;
/// Server → Client: channel error.   `[channel_id:2][msg_len:2][msg:N]`
const MUX_S2C_ERROR: u8 = 0x83;

/// Blit protocol: server is shutting down (single byte, no payload).
/// Injected into a channel's data stream when the upstream socket closes so
/// the browser can immediately dismiss its state instead of waiting for a
/// transport-level timeout.
const S2C_QUIT: u8 = 0x0C;

/// Build a mux control frame.
fn mux_control(opcode: u8, ch: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    buf.extend_from_slice(&MUX_CONTROL.to_le_bytes());
    buf.push(opcode);
    buf.extend_from_slice(&ch.to_le_bytes());
    buf
}

/// Build a mux error control frame.
fn mux_error(ch: u16, msg: &str) -> Vec<u8> {
    let msg_bytes = msg.as_bytes();
    let msg_len = msg_bytes.len().min(u16::MAX as usize);
    let mut buf = Vec::with_capacity(7 + msg_len);
    buf.extend_from_slice(&MUX_CONTROL.to_le_bytes());
    buf.push(MUX_S2C_ERROR);
    buf.extend_from_slice(&ch.to_le_bytes());
    buf.extend_from_slice(&(msg_len as u16).to_le_bytes());
    buf.extend_from_slice(&msg_bytes[..msg_len]);
    buf
}

async fn root_handler(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    let path = request.uri().path().to_string();

    if let Some(resp) = blit_webserver::try_font_route(&path, state.cors_origin.as_deref()) {
        return resp;
    }

    let is_ws = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if is_ws && path.ends_with("/config") {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| async move {
                let transform = state
                    .webrtc_enabled
                    .then_some(mark_share_remotes_proxiable as fn(&str) -> String);
                let mut extra_init = Vec::new();
                if let Some(hash) = state.wt_cert_hash.read().unwrap().as_ref() {
                    extra_init.push(format!("wt={hash}"));
                }
                blit_webserver::config::handle_config_ws(
                    socket,
                    &state.passphrase,
                    &state.config_state,
                    Some(&state.remotes),
                    transform,
                    &extra_init,
                )
                .await;
            }),
            Err(e) => e.into_response(),
        }
    } else if is_ws && is_mux_path(&path) {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws
                .max_message_size(MAX_FRAME_SIZE + 2) // +2 for channel ID prefix
                .on_upgrade(move |socket| handle_mux_ws(socket, state)),
            Err(e) => e.into_response(),
        }
    } else if is_ws {
        let dest_name = resolve_destination_name(&path);
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws
                .max_message_size(MAX_FRAME_SIZE)
                .on_upgrade(move |socket| handle_ws(socket, state, dest_name)),
            Err(e) => e.into_response(),
        }
    } else {
        let etag = &*INDEX_ETAG;
        let inm = request
            .headers()
            .get(axum::http::header::IF_NONE_MATCH)
            .map(|v| v.as_bytes());
        let ae = request
            .headers()
            .get(axum::http::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok());
        blit_webserver::html_response(INDEX_HTML_BR, etag, inm, ae)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        diff |= a[i] ^ b[i];
    }
    std::hint::black_box(diff) == 0
}

async fn handle_ws(mut ws: WebSocket, state: AppState, dest_name: Option<String>) {
    let authed = match tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
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
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let _ = ws.close().await;
            false
        }
    };
    if !authed {
        return;
    }

    let dest_label = match dest_name.as_deref() {
        Some(n) => n,
        None => {
            let _ = ws
                .send(Message::Text("error:no destination specified".into()))
                .await;
            let _ = ws.close().await;
            return;
        }
    };
    let connector = match state.connector_for(dest_label) {
        Some(c) => c,
        None => {
            eprintln!("unknown destination '{dest_label}'");
            let _ = ws
                .send(Message::Text(
                    format!("error:unknown destination '{dest_label}'").into(),
                ))
                .await;
            let _ = ws.close().await;
            return;
        }
    };
    eprintln!("client authenticated for '{dest_label}'");

    let (mut sock_reader, mut sock_writer) = match connector.connect().await {
        Ok(rw) => rw,
        Err(e) => {
            eprintln!("cannot connect to blit server for '{dest_label}': {e}");
            let _ = ws.send(Message::Text(format!("error:{e}").into())).await;
            let _ = ws.close().await;
            return;
        }
    };
    let _ = ws.send(Message::Text("ok".into())).await;
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

    let shutdown = state.shutdown.clone();
    let mut sock_to_ws = tokio::spawn(async move {
        loop {
            tokio::select! {
                frame = read_frame(&mut sock_reader) => {
                    match frame {
                        Some(data) => {
                            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break, // upstream EOF
                    }
                }
                _ = shutdown.notified() => break,
            }
        }
        // Inject S2C_QUIT so the browser can immediately dismiss its state
        // instead of waiting for a WebSocket close timeout.
        let _ = ws_tx.send(Message::Binary(vec![S2C_QUIT].into())).await;
    });

    tokio::select! {
        _ = &mut ws_to_sock => {}
        _ = &mut sock_to_ws => {}
    }
    ws_to_sock.abort();
    sock_to_ws.abort();

    eprintln!("client disconnected from '{dest_label}'");
}

// ---------------------------------------------------------------------------
// Multiplexed WebSocket handler.
// ---------------------------------------------------------------------------

/// State for a single multiplexed channel inside a mux session.
struct MuxChannelState {
    /// Send payloads to be written upstream.
    writer_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Upstream writer task handle.
    writer_task: JoinHandle<()>,
    /// Upstream reader task handle.
    reader_task: JoinHandle<()>,
}

impl MuxChannelState {
    fn shutdown(self) {
        // Dropping writer_tx causes the writer task to end.
        drop(self.writer_tx);
        self.writer_task.abort();
        self.reader_task.abort();
    }
}

async fn handle_mux_ws(mut ws: WebSocket, state: AppState) {
    // --- Authentication (identical to handle_ws) ---
    let authed = match tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
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
        }
    })
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let _ = ws.close().await;
            false
        }
    };
    if !authed {
        return;
    }

    // Signal mux mode (distinct from "ok" used by the legacy per-destination handler).
    let _ = ws.send(Message::Text("mux".into())).await;
    eprintln!("mux client authenticated");

    let (ws_tx, mut ws_rx) = ws.split();

    // All upstream reader tasks feed frames into this channel; the writer
    // task drains it into ws_tx.  Each frame is already prefixed with the
    // 2-byte channel ID.
    let (merge_tx, merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let mut channels: HashMap<u16, MuxChannelState> = HashMap::new();
    let shutdown = state.shutdown.clone();

    // Channel-open tasks are spawned into this JoinSet so the select loop
    // stays non-blocking while (potentially slow) upstream connections are
    // established.  Each task returns `(ch_id, Option<MuxChannelState>)`.
    let mut open_tasks: tokio::task::JoinSet<(u16, Option<MuxChannelState>)> =
        tokio::task::JoinSet::new();
    // Abort handles for pending opens — lets us cancel an in-flight connect
    // when the browser re-opens or closes the same channel ID.
    let mut pending_opens: HashMap<u16, tokio::task::AbortHandle> = HashMap::new();

    // Writer task: sends mux frames to the WebSocket.  Decoupled from the
    // main loop so that slow WebSocket writes (TCP backpressure) never block
    // processing of C2S messages — especially ACKs and client metrics that
    // the server's pacing engine depends on.
    let mut writer_task = tokio::spawn(async move {
        let mut ws_tx = ws_tx;
        let mut merge_rx = merge_rx;
        while let Some(data) = merge_rx.recv().await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            biased;

            // Completed channel-open tasks — insert the channel state so
            // that subsequent data frames can be forwarded.  Polled before
            // ws_rx so the entry is in `channels` by the time the browser's
            // first post-OPENED data frame arrives (OPENED travels through
            // merge_tx → ws_tx → network → browser, giving us plenty of
            // time).
            result = open_tasks.join_next(), if !open_tasks.is_empty() => {
                if let Some(Ok((ch_id, Some(ch_state)))) = result {
                    pending_opens.remove(&ch_id);
                    channels.insert(ch_id, ch_state);
                } else if let Some(Ok((ch_id, None))) = result {
                    pending_opens.remove(&ch_id);
                }
                // Err = task panicked or was aborted — already cleaned up.
            }

            // Browser → upstream: demux by channel ID.
            msg = ws_rx.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    _ => break,
                };
                match msg {
                    Message::Binary(data) => {
                        if data.len() < 2 { continue; }
                        let ch_id = u16::from_le_bytes([data[0], data[1]]);
                        let payload = &data[2..];

                        if ch_id == MUX_CONTROL {
                            // Control message.
                            if payload.is_empty() { continue; }
                            match payload[0] {
                                MUX_C2S_OPEN => {
                                    if payload.len() < 5 { continue; }
                                    let open_ch = u16::from_le_bytes([payload[1], payload[2]]);
                                    let name_len = u16::from_le_bytes([payload[3], payload[4]]) as usize;
                                    if payload.len() < 5 + name_len { continue; }
                                    let name = std::str::from_utf8(&payload[5..5 + name_len])
                                        .unwrap_or("")
                                        .to_string();

                                    // Cancel any in-flight open for this channel ID.
                                    if let Some(abort) = pending_opens.remove(&open_ch) {
                                        abort.abort();
                                    }
                                    // Close any previous channel with the same ID (re-open).
                                    if let Some(prev) = channels.remove(&open_ch) {
                                        prev.shutdown();
                                    }

                                    let open_state = state.clone();
                                    let open_merge_tx = merge_tx.clone();
                                    let abort = open_tasks.spawn(async move {
                                        let ch = mux_open_channel(
                                            open_ch, name, open_state, open_merge_tx,
                                        ).await;
                                        (open_ch, ch)
                                    });
                                    pending_opens.insert(open_ch, abort);
                                }
                                MUX_C2S_CLOSE => {
                                    if payload.len() < 3 { continue; }
                                    let close_ch = u16::from_le_bytes([payload[1], payload[2]]);
                                    // Cancel any in-flight open for this channel ID.
                                    if let Some(abort) = pending_opens.remove(&close_ch) {
                                        abort.abort();
                                    }
                                    if let Some(ch) = channels.remove(&close_ch) {
                                        ch.shutdown();
                                    }
                                    let _ = merge_tx.send(mux_control(MUX_S2C_CLOSED, close_ch));
                                }
                                _ => {} // Unknown control opcode — ignore.
                            }
                        } else if let Some(ch) = channels.get(&ch_id) {
                            // Data frame — forward payload to upstream writer.
                            let _ = ch.writer_tx.send(payload.to_vec());
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }

            // Writer task exited — WebSocket error or all senders dropped.
            _ = &mut writer_task => break,

            // Gateway is shutting down — send S2C_QUIT on every open channel
            // via the writer task, then exit.
            _ = shutdown.notified() => {
                for &ch_id in channels.keys() {
                    let mut quit_frame = Vec::with_capacity(3);
                    quit_frame.extend_from_slice(&ch_id.to_le_bytes());
                    quit_frame.push(S2C_QUIT);
                    let _ = merge_tx.send(quit_frame);
                }
                break;
            }
        }
    }

    // Clean up all channels and pending opens.
    open_tasks.abort_all();
    writer_task.abort();
    for (_, ch) in channels {
        ch.shutdown();
    }
    eprintln!("mux client disconnected");
}

/// Open a multiplexed channel: connect to the upstream destination and wire
/// reader/writer tasks that bridge the channel to the merge queue.
///
/// Returns the channel state on success so the caller can insert it into the
/// channel map.  On failure an error control frame is sent via `merge_tx`
/// and `None` is returned.
///
/// Accepts owned types so the caller can `tokio::spawn` this without
/// lifetime issues — this is critical for keeping the mux select-loop
/// non-blocking while potentially slow connections (SSH, WebRTC, proxy)
/// are established.
async fn mux_open_channel(
    ch_id: u16,
    name: String,
    state: AppState,
    merge_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) -> Option<MuxChannelState> {
    let connector = match state.connector_for(&name) {
        Some(c) => c,
        None => {
            eprintln!("mux: unknown destination '{name}'");
            let _ = merge_tx.send(mux_error(ch_id, &format!("unknown destination '{name}'")));
            return None;
        }
    };

    let connect_result =
        tokio::time::timeout(std::time::Duration::from_secs(30), connector.connect()).await;

    let (sock_reader, sock_writer) = match connect_result {
        Ok(Ok(rw)) => rw,
        Ok(Err(e)) => {
            eprintln!("mux: cannot connect to '{name}': {e}");
            let _ = merge_tx.send(mux_error(ch_id, &e));
            return None;
        }
        Err(_) => {
            let msg = format!("connection to '{name}' timed out");
            eprintln!("mux: {msg}");
            let _ = merge_tx.send(mux_error(ch_id, &msg));
            return None;
        }
    };

    // Writer task: drains payloads from the browser into the upstream socket.
    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(async move {
        let mut w = sock_writer;
        while let Some(payload) = writer_rx.recv().await {
            if !write_frame(&mut w, &payload).await {
                break;
            }
        }
    });

    // Send OPENED *before* starting the reader so the browser receives it
    // before any data frames from the upstream.
    let _ = merge_tx.send(mux_control(MUX_S2C_OPENED, ch_id));

    // Reader task: reads length-prefixed frames from the upstream socket,
    // prepends the channel ID, and feeds them into the merge queue.
    let reader_merge_tx = merge_tx.clone();
    let reader_task = tokio::spawn(async move {
        let mut r = sock_reader;
        while let Some(data) = read_frame(&mut r).await {
            let mut frame = Vec::with_capacity(2 + data.len());
            frame.extend_from_slice(&ch_id.to_le_bytes());
            frame.extend_from_slice(&data);
            if reader_merge_tx.send(frame).is_err() {
                break;
            }
        }
        // Upstream EOF — inject S2C_QUIT as a data frame so the browser's
        // BlitConnection can immediately clear its session state, then send
        // the mux-level CLOSED control frame.
        let mut quit_frame = Vec::with_capacity(3);
        quit_frame.extend_from_slice(&ch_id.to_le_bytes());
        quit_frame.push(S2C_QUIT);
        let _ = reader_merge_tx.send(quit_frame);
        let _ = reader_merge_tx.send(mux_control(MUX_S2C_CLOSED, ch_id));
    });

    eprintln!("mux: channel {ch_id} opened for '{name}'");

    Some(MuxChannelState {
        writer_tx,
        writer_task,
        reader_task,
    })
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

/// Authenticate a WebTransport bidirectional stream.
///
/// Protocol: client sends `[pass_len:2 LE][passphrase]`, server responds
/// with `[1]` (ok) or `[0]` (rejected).  Returns `Ok(())` on success.
async fn wt_authenticate(
    send: &mut wt::SendStream,
    recv: &mut wt::RecvStream,
    passphrase: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let auth_result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut len_buf = [0u8; 2];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("auth read len: {e}"))?;
        let pass_len = u16::from_le_bytes(len_buf) as usize;
        if pass_len > 4096 {
            return Err::<(), String>("passphrase too long".into());
        }
        let mut pass_buf = vec![0u8; pass_len];
        recv.read_exact(&mut pass_buf)
            .await
            .map_err(|e| format!("auth read pass: {e}"))?;
        let pass = std::str::from_utf8(&pass_buf).unwrap_or("");

        if !constant_time_eq(pass.trim().as_bytes(), passphrase.as_bytes()) {
            send.write_all(&[0]).await.ok();
            return Err("authentication failed".into());
        }
        Ok(())
    })
    .await;

    match auth_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err("authentication timed out".into()),
    }
    send.write_all(&[1])
        .await
        .map_err(|e| format!("auth write ok: {e}"))?;
    Ok(())
}

async fn handle_webtransport_session(
    request: wt::Request,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = request.url.path().to_string();
    let is_mux = is_mux_path(&path);
    let dest_name = resolve_destination_name(&path);
    let session = request.ok().await?;

    let (mut send, mut recv) = session.accept_bi().await?;

    wt_authenticate(&mut send, &mut recv, &state.passphrase).await?;

    if is_mux {
        return handle_mux_wt(send, recv, state).await;
    }

    let dest_label = match dest_name.as_deref() {
        Some(n) => n,
        None => {
            session.close(1, b"no destination specified");
            session.closed().await;
            return Ok(());
        }
    };
    eprintln!("webtransport client authenticated for '{dest_label}'");

    // --- Proxy to blit server ---
    let connector = match state.connector_for(dest_label) {
        Some(c) => c,
        None => {
            eprintln!("webtransport: unknown destination '{dest_label}'");
            session.close(1, format!("unknown destination '{dest_label}'").as_bytes());
            session.closed().await;
            return Ok(());
        }
    };
    let (mut sock_reader, mut sock_writer) = match connector.connect().await {
        Ok(rw) => rw,
        Err(e) => {
            eprintln!("webtransport: cannot connect to blit server for '{dest_label}': {e}");
            session.close(1, e.as_bytes());
            session.closed().await;
            return Ok(());
        }
    };

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

    eprintln!("webtransport client disconnected from '{dest_label}'");
    Ok(())
}

/// Handle the mux protocol over a WebTransport bidirectional stream.
///
/// The wire format wraps each mux frame in a length prefix:
/// `[frame_len:4 LE][mux_frame]` where `mux_frame` has the same layout as
/// a WebSocket binary message in the WS mux handler:
/// `[channel_id:2 LE][payload]` for data, `[0xFFFF][opcode][...]` for control.
async fn handle_mux_wt(
    send: wt::SendStream,
    mut recv: wt::RecvStream,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    eprintln!("mux-wt client authenticated");

    let (merge_tx, merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut channels: HashMap<u16, MuxChannelState> = HashMap::new();
    let shutdown = state.shutdown.clone();

    // Channel-open tasks (same pattern as the WS mux handler).
    let mut open_tasks: tokio::task::JoinSet<(u16, Option<MuxChannelState>)> =
        tokio::task::JoinSet::new();
    let mut pending_opens: HashMap<u16, tokio::task::AbortHandle> = HashMap::new();

    // Reader task: reads length-prefixed mux frames from the WT stream.
    let (client_frame_tx, mut client_frame_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let reader_task = tokio::spawn(async move {
        let mut len_buf = [0u8; 4];
        loop {
            if recv.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > MAX_FRAME_SIZE + 2 {
                break;
            }
            let mut buf = vec![0u8; len];
            if len > 0 && recv.read_exact(&mut buf).await.is_err() {
                break;
            }
            if client_frame_tx.send(buf).is_err() {
                break;
            }
        }
    });

    // Writer task: sends length-prefixed mux frames to the WT stream.
    // Decoupled from the main loop so that slow QUIC writes (flow control,
    // congestion) never block processing of C2S messages — especially ACKs
    // and client metrics that the server's pacing engine depends on.
    let mut writer_task = tokio::spawn(async move {
        let mut send = send;
        let mut merge_rx = merge_rx;
        while let Some(data) = merge_rx.recv().await {
            let mut frame = Vec::with_capacity(4 + data.len());
            frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
            frame.extend_from_slice(&data);
            if send.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            biased;

            // Completed channel-open tasks (same as WS mux handler).
            result = open_tasks.join_next(), if !open_tasks.is_empty() => {
                if let Some(Ok((ch_id, Some(ch_state)))) = result {
                    pending_opens.remove(&ch_id);
                    channels.insert(ch_id, ch_state);
                } else if let Some(Ok((ch_id, None))) = result {
                    pending_opens.remove(&ch_id);
                }
            }

            // Client → upstream: demux by channel ID.
            msg = client_frame_rx.recv() => {
                let data = match msg {
                    Some(d) => d,
                    None => break,
                };
                if data.len() < 2 { continue; }
                let ch_id = u16::from_le_bytes([data[0], data[1]]);
                let payload = &data[2..];

                if ch_id == MUX_CONTROL {
                    if payload.is_empty() { continue; }
                    match payload[0] {
                        MUX_C2S_OPEN => {
                            if payload.len() < 5 { continue; }
                            let open_ch = u16::from_le_bytes([payload[1], payload[2]]);
                            let name_len = u16::from_le_bytes([payload[3], payload[4]]) as usize;
                            if payload.len() < 5 + name_len { continue; }
                            let name = std::str::from_utf8(&payload[5..5 + name_len])
                                .unwrap_or("")
                                .to_string();

                            if let Some(abort) = pending_opens.remove(&open_ch) {
                                abort.abort();
                            }
                            if let Some(prev) = channels.remove(&open_ch) {
                                prev.shutdown();
                            }

                            let open_state = state.clone();
                            let open_merge_tx = merge_tx.clone();
                            let abort = open_tasks.spawn(async move {
                                let ch = mux_open_channel(
                                    open_ch, name, open_state, open_merge_tx,
                                ).await;
                                (open_ch, ch)
                            });
                            pending_opens.insert(open_ch, abort);
                        }
                        MUX_C2S_CLOSE => {
                            if payload.len() < 3 { continue; }
                            let close_ch = u16::from_le_bytes([payload[1], payload[2]]);
                            if let Some(abort) = pending_opens.remove(&close_ch) {
                                abort.abort();
                            }
                            if let Some(ch) = channels.remove(&close_ch) {
                                ch.shutdown();
                            }
                            let _ = merge_tx.send(mux_control(MUX_S2C_CLOSED, close_ch));
                        }
                        _ => {}
                    }
                } else if let Some(ch) = channels.get(&ch_id) {
                    let _ = ch.writer_tx.send(payload.to_vec());
                }
            }

            // Writer task exited — QUIC stream error or all senders dropped.
            _ = &mut writer_task => break,

            // Gateway is shutting down — send S2C_QUIT on every open channel
            // via the writer task, then exit.
            _ = shutdown.notified() => {
                for &ch_id in channels.keys() {
                    let mut quit_frame = Vec::with_capacity(3);
                    quit_frame.extend_from_slice(&ch_id.to_le_bytes());
                    quit_frame.push(S2C_QUIT);
                    let _ = merge_tx.send(quit_frame);
                }
                break;
            }
        }
    }

    open_tasks.abort_all();
    reader_task.abort();
    writer_task.abort();
    for (_, ch) in channels {
        ch.shutdown();
    }
    eprintln!("mux-wt client disconnected");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn make_test_state(destinations: DestMap, cors_origin: Option<String>) -> AppState {
        Arc::new(Config {
            passphrase: "test".into(),
            destinations: std::sync::RwLock::new(destinations),
            remotes: blit_webserver::config::RemotesState::ephemeral(String::new()),
            cors_origin,
            wt_cert_hash: std::sync::RwLock::new(None),
            config_state: blit_webserver::config::ConfigState::new(),
            proxy_sock: None,
            ssh_pool: blit_ssh::SshPool::new(),
            hub_url: blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL),
            webrtc_enabled: false,
            shutdown: Arc::new(tokio::sync::Notify::new()),
        })
    }

    fn test_app() -> axum::Router {
        let mut destinations = std::collections::HashMap::new();
        destinations.insert(
            "default".into(),
            GatewayConnector::Ipc("/nonexistent.sock".into()),
        );
        build_app(make_test_state(destinations, None))
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

    #[tokio::test]
    async fn etag_304_on_matching_if_none_match() {
        let app = test_app();
        let resp = app
            .clone()
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let etag = resp
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();

        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/")
                    .header("if-none-match", &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            304,
            "expected 304 Not Modified with matching ETag"
        );
    }

    #[tokio::test]
    async fn etag_200_on_mismatched_if_none_match() {
        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/")
                    .header("if-none-match", "\"wrong-etag\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    fn test_app_with_cors(origin: &str) -> axum::Router {
        let mut destinations = std::collections::HashMap::new();
        destinations.insert(
            "default".into(),
            GatewayConnector::Ipc("/nonexistent.sock".into()),
        );
        build_app(make_test_state(destinations, Some(origin.into())))
    }

    #[tokio::test]
    async fn cors_header_present_on_font_route() {
        let app = test_app_with_cors("*");
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
        let cors = resp
            .headers()
            .get("access-control-allow-origin")
            .expect("expected CORS header");
        assert_eq!(cors.to_str().unwrap(), "*");
    }

    #[tokio::test]
    async fn no_cors_header_when_none() {
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
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "CORS header should not be present when cors_origin is None"
        );
    }

    // /config is WebSocket-only now — a plain GET falls through to the SPA.
    #[tokio::test]
    async fn config_get_returns_index_html() {
        let app = test_app();
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/config")
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
            ct.contains("text/html"),
            "expected index.html fallthrough, got {ct}"
        );
    }

    // RemotesState round-trips through parse_remotes_str / serialize_remotes.
    #[test]
    fn remotes_parse_roundtrip() {
        let input = "rabbit = ssh:rabbit\nfox = ssh:fox\n";
        let entries = blit_webserver::config::parse_remotes_str(input);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("rabbit".into(), "ssh:rabbit".into()));
        assert_eq!(entries[1], ("fox".into(), "ssh:fox".into()));
    }

    #[test]
    fn remotes_parse_comments_and_blanks() {
        let input = "# header\nrabbit = ssh:rabbit\n\n# ignored\nfox = ssh:fox\n";
        let entries = blit_webserver::config::parse_remotes_str(input);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn remotes_ephemeral_get() {
        let r = blit_webserver::config::RemotesState::ephemeral("rabbit = ssh:rabbit\n".into());
        assert_eq!(r.get(), "rabbit = ssh:rabbit\n");
    }

    #[test]
    fn share_uri_ignored_when_webrtc_disabled() {
        let c = uri_to_connector(
            "share:mysecret",
            &blit_ssh::SshPool::new(),
            "wss://hub.blit.sh",
            false,
        );
        assert!(
            c.is_none(),
            "share: should be ignored when webrtc_enabled=false"
        );
    }

    #[test]
    fn share_uri_parses_passphrase_only() {
        let c = uri_to_connector(
            "share:mysecret",
            &blit_ssh::SshPool::new(),
            "wss://hub.blit.sh",
            true,
        );
        match c {
            Some(GatewayConnector::Share {
                passphrase,
                signal_url,
                ..
            }) => {
                assert_eq!(passphrase, "mysecret");
                assert_eq!(signal_url, "wss://hub.blit.sh");
            }
            _ => panic!("expected Share connector"),
        }
    }

    #[test]
    fn share_uri_parses_custom_hub() {
        let c = uri_to_connector(
            "share:mysecret?hub=wss://custom.hub",
            &blit_ssh::SshPool::new(),
            "wss://hub.blit.sh",
            true,
        );
        match c {
            Some(GatewayConnector::Share {
                passphrase,
                signal_url,
                ..
            }) => {
                assert_eq!(passphrase, "mysecret");
                assert_eq!(signal_url, "wss://custom.hub");
            }
            _ => panic!("expected Share connector"),
        }
    }
}
