use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, FromRequest, State, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
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
    /// upsidedown relay attach (`uplink:<jwt>[?control=<url>]`), resolved
    /// by blit-proxy.  Holds the full URI; the token inside is a credential.
    Uplink(String),
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

const INDEX_HTML_BR: &[u8] = include_bytes!("../../../js/ui/dist/index.html.br");
/// The preview service worker (docs/design/net.md § Client: service worker).
/// A separate asset because a worker cannot be inlined into the single-file
/// app bundle: it needs its own URL and a JavaScript MIME type.
const SW_JS_BR: &[u8] = include_bytes!("../../../js/ui/dist/sw.js.br");

/// Workers the UI spawns. Each is a separate asset because a worker cannot be
/// inlined into the single-file build, and each therefore needs a route: an
/// unserved worker fails only in production, where a dev server is not there
/// to hand the file over.
const MUX_WORKER_BR: &[u8] = include_bytes!("../../../js/ui/dist/mux-worker.js.br");
const BUFFER_RECYCLER_WORKER_BR: &[u8] =
    include_bytes!("../../../js/ui/dist/buffer-recycler-worker.js.br");

static INDEX_ETAG: LazyLock<String> = LazyLock::new(|| blit_webserver::html_etag(INDEX_HTML_BR));
static SW_ETAG: LazyLock<String> = LazyLock::new(|| blit_webserver::html_etag(SW_JS_BR));
static MUX_WORKER_ETAG: LazyLock<String> =
    LazyLock::new(|| blit_webserver::html_etag(MUX_WORKER_BR));
static BUFFER_RECYCLER_ETAG: LazyLock<String> =
    LazyLock::new(|| blit_webserver::html_etag(BUFFER_RECYCLER_WORKER_BR));

type DestMap = std::collections::HashMap<String, GatewayConnector>;

#[derive(Debug, PartialEq, Eq)]
struct WebTransportPublicAddr {
    /// `None` means the browser reuses the page hostname.
    host: Option<String>,
    port: u16,
}

impl std::fmt::Display for WebTransportPublicAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.host.as_deref() {
            Some(host) if host.starts_with('[') && host.ends_with(']') => {
                write!(f, "{host}:{}", self.port)
            }
            Some(host) if host.contains(':') => write!(f, "[{host}]:{}", self.port),
            Some(host) => write!(f, "{host}:{}", self.port),
            None => write!(f, ":{}", self.port),
        }
    }
}

struct Config {
    passphrase: blit_webserver::config::AuthPassphrase,
    /// Resolved connectors for routing WebSocket/WebTransport connections.
    /// Derived from `remotes` on startup and reconciled on file changes.
    destinations: std::sync::RwLock<DestMap>,
    /// Live-reloading `blit.remotes` file — the persistent source of truth
    /// for the remote list.  The file watcher drives `destinations` updates.
    remotes: blit_webserver::config::RemotesState,
    /// Live-reloading `blit.roots` file — the IDE workspace roots served to
    /// browsers over `/config`. Does not affect routing.
    roots: blit_webserver::config::RootsState,
    cors_origin: Option<String>,
    wt_cert_hash: std::sync::RwLock<Option<String>>,
    /// Browser-facing `host:port` (or `:port`) advertised to authenticated
    /// clients. The browser location supplies an omitted hostname.
    wt_public_addr: Option<WebTransportPublicAddr>,
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
    /// Shared auth throttle for config and gateway transports.
    auth_throttle: blit_webserver::config::AuthThrottle,
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
            GatewayConnector::Uplink(uri) => {
                if let Some(proxy) = &self.proxy_sock {
                    ConnectorSnapshot::Proxied(proxy.clone(), uri.clone())
                } else {
                    ConnectorSnapshot::Uplink(uri.clone())
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
    if uri.starts_with("uplink:") {
        return Some(GatewayConnector::Uplink(uri.to_string()));
    }
    if uri == "local" {
        let path = blit_webserver::config::default_local_socket();
        return Some(GatewayConnector::Ipc(path));
    }
    if let Some(name) = uri.strip_prefix("local:")
        && blit_webserver::config::valid_server_name(name)
    {
        return Some(GatewayConnector::Ipc(
            blit_webserver::config::local_socket_for_name(name),
        ));
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
    /// upsidedown relay attach, resolved in-process via blit-proxy's library.
    Uplink(String),
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
            ConnectorSnapshot::Uplink(uri) => blit_proxy::connect_uplink_split(uri).await,
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
            let exe = blit_proxy::blit_exe();
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

/// AF41 maps interactive video onto the Wi-Fi Multimedia video access class
/// on networks that honor DSCP, instead of competing as best-effort bulk TCP.
const INTERACTIVE_TOS: u32 = 34 << 2;
/// Keep at most roughly one large encoded frame waiting unsent in the kernel.
/// The default effectively lets the whole autotuned send buffer fill, hiding
/// congestion from the async writer and turning a Wi-Fi loss into a long
/// stale-video queue before application backpressure can react.
#[cfg(target_os = "linux")]
const TCP_NOTSENT_LOWAT: u32 = 64 * 1024;

fn configure_browser_tcp(stream: &tokio::net::TcpStream) {
    let _ = stream.set_nodelay(true);
    let socket = socket2::SockRef::from(stream);
    let _ = socket.set_tos_v4(INTERACTIVE_TOS);
    #[cfg(unix)]
    let _ = socket.set_tclass_v6(INTERACTIVE_TOS);
    #[cfg(target_os = "linux")]
    {
        // Linux priority 4 is the 802.1D video class. This also selects the
        // corresponding hardware queue when the egress device exposes one.
        let _ = socket.set_priority(4);
        let _ = socket.set_tcp_notsent_lowat(TCP_NOTSENT_LOWAT);
    }
}

/// Upstream data frames the mux writer may hold for one browser before the
/// upstream readers stall. Keep only one waiting behind the frame currently
/// being written: at 240 fps an eight-frame application queue could hide over
/// 33 ms of congestion from the server before TCP backpressure reached it.
/// The socket buffers and server ACK window still carry the link BDP.
const MUX_DATA_QUEUE_FRAMES: usize = 1;

/// Largest `/config` message. That endpoint speaks short text control lines
/// (`set k v`, `remotes-add …`), so 64 KiB is already generous; the point is
/// not to inherit a 64 MiB default on the one path that reads before it
/// authenticates.
const CONFIG_MAX_MESSAGE_SIZE: usize = 64 * 1024;

/// Client mux frames the WebTransport reader may hold before it stops
/// draining the QUIC stream.  Matches `MUX_DATA_QUEUE_FRAMES` in the other
/// direction; the WebSocket path needs no equivalent because its select loop
/// reads the socket directly.
const MUX_CLIENT_QUEUE_FRAMES: usize = 8;

/// Audio frames held for one browser, across every channel.
///
/// Audio has its own lane because it is the one stream where arriving late is
/// indistinguishable from not arriving: a video frame that is 40 ms behind is
/// 40 ms of staleness, an audio frame that is 40 ms behind is a gap the
/// listener hears. Deep enough to ride out a burst, and far shallower than
/// the point where the frames would be stale on arrival anyway — the server
/// already discards its own backlog past 500 ms.
const MUX_AUDIO_QUEUE_FRAMES: usize = 64;

/// Mux control frames (OPENED / CLOSED / errors) queued for one browser.
/// Every one is a handful of bytes and they are only produced in response to
/// client actions, so this is generous — it exists because a client can spam
/// `MUX_C2S_CLOSE`, each of which used to push an ack onto an unbounded queue.
const MUX_CONTROL_QUEUE_FRAMES: usize = 256;

/// Frames queued for one upstream socket before the channel is considered
/// stuck.  Client input is keystrokes, ACKs and resizes — tiny and drained at
/// upstream speed — so a backlog this deep means the upstream is not moving,
/// not that it is slow.
const MUX_WRITER_QUEUE_FRAMES: usize = 64;

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
    let passphrase_raw = std::env::var("BLIT_PASSPHRASE").unwrap_or_else(|_| {
        eprintln!("BLIT_PASSPHRASE environment variable required");
        std::process::exit(1);
    });
    let passphrase = blit_webserver::config::AuthPassphrase::from_env_value(passphrase_raw);
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
    let roots = blit_webserver::config::RootsState::new();
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
    let wt_public_addr = if quic_enabled {
        let configured = std::env::var("BLIT_QUIC_PUBLIC_ADDR")
            .ok()
            .filter(|raw| !raw.trim().is_empty());
        configured.map_or_else(
            || {
                let port = addr
                    .parse::<SocketAddr>()
                    .unwrap_or_else(|e| {
                        eprintln!("blit gateway: invalid BLIT_ADDR '{addr}': {e}");
                        std::process::exit(1);
                    })
                    .port();
                Some(WebTransportPublicAddr { host: None, port })
            },
            |raw| {
                Some(parse_webtransport_public_addr(&raw).unwrap_or_else(|| {
                    eprintln!(
                        "blit gateway: invalid BLIT_QUIC_PUBLIC_ADDR '{raw}' \
                         (expected hostname:port or :port)"
                    );
                    std::process::exit(1);
                }))
            },
        )
    } else {
        None
    };

    let cors_origin = std::env::var("BLIT_CORS").ok();
    let config_state = blit_webserver::config::ConfigState::new();

    // Route all proxiable upstream connections through blit-proxy unless
    // explicitly disabled with BLIT_PROXY=0.  The proxy is auto-started as
    // a daemon via `blit proxy-daemon` (same binary).
    let proxy_sock: Option<String> = if std::env::var("BLIT_PROXY").ok().as_deref() == Some("0") {
        None
    } else {
        let exe = blit_proxy::blit_exe();
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
        roots,
        cors_origin,
        wt_cert_hash: std::sync::RwLock::new(None),
        wt_public_addr,
        config_state,
        proxy_sock,
        ssh_pool,
        hub_url,
        webrtc_enabled,
        shutdown: shutdown.clone(),
        auth_throttle: blit_webserver::config::AuthThrottle::new(),
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
    let listener = tcp.tap_io(|stream| {
        configure_browser_tcp(stream);
    });
    eprintln!(
        "listening on {addr} (WebSocket{}){}",
        if quic_enabled { " + WebTransport" } else { "" },
        if quic_enabled {
            ""
        } else {
            " — set BLIT_QUIC=1 to enable WebTransport"
        },
    );

    blit_sd_notify::notify_ready(false);

    let graceful = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
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

/// Blit protocol: one piece of a split logical message.
const S2C_FRAGMENT: u8 = 0x2B;
const FRAGMENT_FLAG_LAST: u8 = 1 << 0;

/// Blit protocol: an encoded audio frame.
const S2C_AUDIO_FRAME: u8 = 0x30;

/// How the last hop we can see treats audio, under `BLIT_AUDIO_DEBUG`.
///
/// The server reports the same three numbers for its own writer, and they
/// come out clean — 50 writes a second, worst gap 21 ms against a 20 ms
/// cadence, no measurable write time. That only proves audio reaches the
/// gateway on time. This is the socket to the browser, and it is the last
/// place a gap can be manufactured where anyone can still watch it happen:
/// past here the evidence is the listener.
///
/// `gap` is between successive audio writes, so it should track the 20 ms
/// cadence. `write` is how long the socket took to accept the frame — a
/// backed-up link shows up here and nowhere else.
struct AudioTrace {
    enabled: bool,
    window_start: std::time::Instant,
    last_write_at: std::time::Instant,
    writes: u32,
    max_gap_ms: u32,
    max_write_ms: u32,
    behind_bulk_ms: u32,
}

impl AudioTrace {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            enabled: std::env::var_os("BLIT_AUDIO_DEBUG").is_some(),
            window_start: now,
            last_write_at: now,
            writes: 0,
            max_gap_ms: 0,
            max_write_ms: 0,
            behind_bulk_ms: 0,
        }
    }

    /// A bulk frame held the socket for `elapsed` — the delay audio would
    /// have paid had it arrived at the wrong moment.
    fn saw_bulk(&mut self, elapsed: std::time::Duration) {
        if !self.enabled {
            return;
        }
        self.behind_bulk_ms = self.behind_bulk_ms.max(elapsed.as_millis() as u32);
    }

    fn saw_audio(&mut self, started: std::time::Instant, finished: std::time::Instant) {
        if !self.enabled {
            return;
        }
        self.writes += 1;
        self.max_gap_ms = self
            .max_gap_ms
            .max(started.duration_since(self.last_write_at).as_millis() as u32);
        self.max_write_ms = self
            .max_write_ms
            .max(finished.duration_since(started).as_millis() as u32);
        self.last_write_at = finished;
        if finished.duration_since(self.window_start) >= std::time::Duration::from_secs(1) {
            eprintln!(
                "[gateway audio] writes={} max_gap={}ms max_write={}ms max_bulk_hold={}ms",
                self.writes, self.max_gap_ms, self.max_write_ms, self.behind_bulk_ms,
            );
            self.window_start = finished;
            self.writes = 0;
            self.max_gap_ms = 0;
            self.max_write_ms = 0;
            self.behind_bulk_ms = 0;
        }
    }
}

/// Splitting bulk frames so audio is not stuck behind them.
///
/// This used to live in the server, sized from how long its `write` took —
/// but the server writes to a unix socket, so it measured the kernel
/// accepting bytes rather than the link carrying them, and on a fast local
/// socket it never triggered. The gateway holds the socket to the browser,
/// which is the link that actually delays audio, so the same decision is
/// correct here and meaningless there.
///
/// What this buys is bounded: it caps how long audio waits *behind* bytes
/// already queued. It does nothing for a packet lost in flight, because a
/// reliable ordered stream will not deliver past a gap however it is framed.
/// That case needs a delivery mode without retransmission, not smaller
/// pieces.
mod bulk {
    use std::time::Duration;

    /// Chunk size once the socket has shown real backpressure. 4 KiB bounds
    /// audio head-of-line time to roughly 4 ms on a 1 MB/s link.
    ///
    /// Splitting every video frame unconditionally is not free: a 240 Hz
    /// 40-70 KiB stream becomes thousands of messages a second, and browsers
    /// process those in batches, so complete frames arrive with visible gaps
    /// even when the byte count is exact. Hence a trigger rather than always.
    pub const CHUNK_BYTES: usize = 4 * 1024;

    /// Payload size split on sight, before any backpressure is measured.
    ///
    /// The first big frame arrives before there is anything to measure, and
    /// half a megabyte is tens of milliseconds of link time with audio behind
    /// it. Well above the 40-70 KiB of an ordinary video frame, so the steady
    /// stream still pays no fragmentation cost.
    pub const ALWAYS_BYTES: usize = 128 * 1024;

    /// Most pieces one frame may be split into.
    ///
    /// The receiver aborts a reassembly past `MAX_FRAGMENT_COUNT` (16,384)
    /// pieces, and a logical message can already reach the gateway as several
    /// frames — the server still splits at the 16 MiB frame ceiling. Capping
    /// per frame well under the receiver's limit keeps the total safe without
    /// the gateway having to track logical messages.
    pub const MAX_CHUNKS_PER_FRAME: usize = 2048;

    const TRIGGER: Duration = Duration::from_millis(5);
    const SLOW_CONFIRMATIONS: u8 = 2;
    const RECOVERY: Duration = Duration::from_millis(2);
    const RECOVERY_WRITES: u8 = 32;

    #[derive(Default)]
    pub struct Fragmentation {
        active: bool,
        slow_writes: u8,
        fast_writes: u8,
    }

    impl Fragmentation {
        /// Chunk size for a payload of `bytes`, or `None` to write it whole.
        pub fn chunk_bytes(&self, bytes: usize) -> Option<usize> {
            if !self.active && bytes < ALWAYS_BYTES {
                return None;
            }
            Some(CHUNK_BYTES.max(bytes.div_ceil(MAX_CHUNKS_PER_FRAME)))
        }

        /// Feed back how long a write of `bytes` took.
        pub fn observe(&mut self, bytes: usize, elapsed: Duration) {
            if bytes <= CHUNK_BYTES {
                return;
            }
            if self.active {
                if elapsed <= RECOVERY {
                    self.fast_writes = self.fast_writes.saturating_add(1);
                    if self.fast_writes >= RECOVERY_WRITES {
                        *self = Self::default();
                    }
                } else {
                    self.fast_writes = 0;
                }
                return;
            }
            if elapsed >= TRIGGER {
                self.slow_writes = self.slow_writes.saturating_add(1);
                if self.slow_writes >= SLOW_CONFIRMATIONS {
                    self.active = true;
                    self.slow_writes = 0;
                }
            } else {
                self.slow_writes = 0;
            }
        }
    }
}

/// Split one channel's payload into `S2C_FRAGMENT` payloads.
///
/// A payload that is already a fragment is split further rather than nested:
/// its header is peeled off and `FRAGMENT_FLAG_LAST` is carried onto the last
/// piece only. Fragments concatenate in order, so this is transparent to the
/// receiver — which is what lets the gateway re-split what the server already
/// split at the frame ceiling.
fn fragment_payload(payload: &[u8], chunk_bytes: usize) -> Vec<Vec<u8>> {
    let (body, ends_message) = if payload.first() == Some(&S2C_FRAGMENT) && payload.len() >= 2 {
        (&payload[2..], payload[1] & FRAGMENT_FLAG_LAST != 0)
    } else {
        (payload, true)
    };
    let mut out = Vec::with_capacity(body.len().div_ceil(chunk_bytes).max(1));
    let mut offset = 0;
    while offset < body.len() {
        let end = offset.saturating_add(chunk_bytes).min(body.len());
        let is_last = end == body.len();
        let mut frag = Vec::with_capacity(2 + (end - offset));
        frag.push(S2C_FRAGMENT);
        frag.push(if is_last && ends_message {
            FRAGMENT_FLAG_LAST
        } else {
            0
        });
        frag.extend_from_slice(&body[offset..end]);
        out.push(frag);
        offset = end;
    }
    out
}

/// Split a mux data frame into mux frames, or `None` to write it whole.
///
/// Takes the whole `[channel_id:2][payload]` frame and returns frames of the
/// same shape, so the caller does not have to know where the payload starts.
/// Control frames reach the data queue too — a channel's CLOSED rides behind
/// its own last bytes deliberately — and those are never split.
fn fragment_bulk_frame(frame: &[u8], state: &bulk::Fragmentation) -> Option<Vec<Vec<u8>>> {
    if frame.len() < 3 {
        return None;
    }
    let ch = u16::from_le_bytes([frame[0], frame[1]]);
    if ch == MUX_CONTROL {
        return None;
    }
    let payload = &frame[2..];
    let chunk_bytes = state.chunk_bytes(payload.len())?;
    let pieces = fragment_payload(payload, chunk_bytes);
    // One piece is the frame itself with two bytes of header added, so
    // sending it as a fragment is pure overhead.
    if pieces.len() <= 1 {
        return None;
    }
    Some(
        pieces
            .into_iter()
            .map(|piece| {
                let mut out = Vec::with_capacity(2 + piece.len());
                out.extend_from_slice(&frame[0..2]);
                out.extend_from_slice(&piece);
                out
            })
            .collect(),
    )
}

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

fn parse_webtransport_public_addr(raw: &str) -> Option<WebTransportPublicAddr> {
    let raw = raw.trim();
    if let Some(port) = raw.strip_prefix(':') {
        let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
        return Some(WebTransportPublicAddr { host: None, port });
    }

    let url = url::Url::parse(&format!("https://{raw}")).ok()?;
    if url.username() != ""
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let host = url.host_str()?.to_string();
    let port = raw
        .rsplit_once(':')?
        .1
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    Some(WebTransportPublicAddr {
        host: Some(host),
        port,
    })
}

async fn root_handler(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    let auth_peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let path = request.uri().path().to_string();

    if let Some(resp) = blit_webserver::try_font_route(
        &path,
        state.cors_origin.as_deref(),
        request
            .headers()
            .get(axum::http::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok()),
    ) {
        return resp;
    }

    // The preview service worker and the path it claims (docs/design/net.md
    // § Client: service worker). Both are checked before the WebSocket
    // upgrade and the SPA fallback: `/x/…` reaching the fallback would render
    // the blit UI inside a preview frame, which is unreadable as a failure.
    {
        let inm = request
            .headers()
            .get(axum::http::header::IF_NONE_MATCH)
            .map(|v| v.as_bytes());
        let ae = request
            .headers()
            .get(axum::http::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok());
        if let Some(resp) = blit_webserver::try_ui_route(&path, SW_JS_BR, &SW_ETAG, inm, ae) {
            return resp;
        }
        // Checked here for the same reason as the service worker: reaching the
        // SPA fallback would answer a worker request with index.html, and a
        // worker fed HTML fails in a way that reads as nothing at all.
        if let Some(resp) = blit_webserver::try_worker_route(
            &path,
            &[
                ("/mux-worker.js", MUX_WORKER_BR, &MUX_WORKER_ETAG),
                (
                    "/buffer-recycler-worker.js",
                    BUFFER_RECYCLER_WORKER_BR,
                    &BUFFER_RECYCLER_ETAG,
                ),
            ],
            inm,
            ae,
        ) {
            return resp;
        }
    }

    let is_ws = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if is_ws && path.ends_with("/config") {
        match WebSocketUpgrade::from_request(request, &state).await {
            // `/config` carries short text control lines and nothing else, but
            // it inherited axum's 64 MiB default — four times what the two
            // upgrades below allow, on the one endpoint that reads from the
            // socket *before* authenticating. Tungstenite reassembles
            // fragmented frames into a growing buffer before rejecting one, so
            // an unauthenticated peer could park 64 MiB per connection.
            Ok(ws) => {
                ws.max_message_size(CONFIG_MAX_MESSAGE_SIZE)
                    .on_upgrade(move |socket| async move {
                        let transform = state
                            .webrtc_enabled
                            .then_some(mark_share_remotes_proxiable as fn(&str) -> String);
                        let mut extra_init = Vec::new();
                        if let Some(addr) = state.wt_public_addr.as_ref() {
                            extra_init.push(format!("wt-addr={addr}"));
                        }
                        if let Some(hash) = state.wt_cert_hash.read().unwrap().as_ref() {
                            extra_init.push(format!("wt={hash}"));
                        }
                        blit_webserver::config::handle_config_ws(
                            socket,
                            &state.passphrase,
                            &state.config_state,
                            Some(&state.remotes),
                            transform,
                            Some(&state.roots),
                            &extra_init,
                            blit_webserver::config::AuthContext {
                                throttle: &state.auth_throttle,
                                peer: &auth_peer,
                            },
                        )
                        .await;
                    })
            }
            Err(e) => e.into_response(),
        }
    } else if is_ws && is_mux_path(&path) {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws
                .max_message_size(MAX_FRAME_SIZE + 2) // +2 for channel ID prefix
                .on_upgrade(move |socket| handle_mux_ws(socket, state, auth_peer)),
            Err(e) => e.into_response(),
        }
    } else if is_ws {
        let dest_name = resolve_destination_name(&path);
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws
                .max_message_size(MAX_FRAME_SIZE)
                .on_upgrade(move |socket| handle_ws(socket, state, dest_name, auth_peer)),
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

async fn handle_ws(
    mut ws: WebSocket,
    state: AppState,
    dest_name: Option<String>,
    auth_peer: String,
) {
    if !blit_webserver::config::authenticate_text_ws(
        &mut ws,
        &state.passphrase,
        &state.auth_throttle,
        &auth_peer,
        None,
    )
    .await
    {
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
    writer_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
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

async fn handle_mux_ws(mut ws: WebSocket, state: AppState, auth_peer: String) {
    // --- Authentication (identical to handle_ws) ---
    if !blit_webserver::config::authenticate_text_ws(
        &mut ws,
        &state.passphrase,
        &state.auth_throttle,
        &auth_peer,
        None,
    )
    .await
    {
        return;
    }

    // Signal mux mode (distinct from "ok" used by the legacy per-destination handler).
    let _ = ws.send(Message::Text("mux".into())).await;
    eprintln!("mux client authenticated");

    let (ws_tx, mut ws_rx) = ws.split();

    // Mux control frames (OPENED / CLOSED / errors) keep their own channel so
    // they are not queued behind bulk data — the writer task polls this one
    // first.  Bounded like the data channel, but the two senders treat a full
    // queue differently: `mux_open_channel` awaits, because OPENED has to
    // reach the browser before the channel's first data frame, while the
    // select loop uses `try_send` and lets a close ack or shutdown QUIT go,
    // because blocking there would stall every other channel on the session.
    let (merge_tx, merge_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_CONTROL_QUEUE_FRAMES);
    // Upstream data frames: a browser that cannot keep up must stop the
    // upstream reader, which fills the upstream socket and lets the blit
    // server see its own writes block.  With an unbounded queue here the
    // server's outbox always looks empty, its only congestion signal never
    // fires, and the backlog grows in this process instead.
    let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_DATA_QUEUE_FRAMES);
    // Audio, kept out of the data queue entirely: that queue is one frame
    // deep on purpose, so a single video frame in it is enough to hold audio
    // behind a write that can take tens of milliseconds on a real link.
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_AUDIO_QUEUE_FRAMES);

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
        let mut data_rx = data_rx;
        let mut audio_rx = audio_rx;
        let mut fragmentation = bulk::Fragmentation::default();
        let mut trace = AudioTrace::new();
        loop {
            let (frame, is_bulk, is_audio) = tokio::select! {
                biased;
                ctrl = merge_rx.recv() => (ctrl, false, false),
                audio = audio_rx.recv() => (audio, false, true),
                data = data_rx.recv() => (data, true, false),
            };
            let Some(frame) = frame else { break };
            if !is_bulk {
                let started = std::time::Instant::now();
                if ws_tx.send(Message::Binary(frame.into())).await.is_err() {
                    break;
                }
                if is_audio {
                    trace.saw_audio(started, std::time::Instant::now());
                }
                continue;
            }
            let bytes = frame.len();
            let started = std::time::Instant::now();
            let mut ok = true;
            match fragment_bulk_frame(&frame, &fragmentation) {
                None => ok = ws_tx.send(Message::Binary(frame.into())).await.is_ok(),
                Some(pieces) => {
                    for piece in pieces {
                        // Audio first, between every piece: splitting the
                        // frame is only useful if something overtakes it.
                        while let Ok(audio) = audio_rx.try_recv() {
                            let at = std::time::Instant::now();
                            if ws_tx.send(Message::Binary(audio.into())).await.is_err() {
                                ok = false;
                                break;
                            }
                            trace.saw_audio(at, std::time::Instant::now());
                        }
                        if !ok || ws_tx.send(Message::Binary(piece.into())).await.is_err() {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if !ok {
                break;
            }
            let elapsed = started.elapsed();
            trace.saw_bulk(elapsed);
            fragmentation.observe(bytes, elapsed);
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
                                    let open_data_tx = data_tx.clone();
                                    let open_audio_tx = audio_tx.clone();
                                    let abort = open_tasks.spawn(async move {
                                        let ch = mux_open_channel(
                                            open_ch, name, open_state, open_merge_tx, open_data_tx,
                                            open_audio_tx,
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
                                    let _ = merge_tx.try_send(mux_control(MUX_S2C_CLOSED, close_ch));
                                }
                                _ => {} // Unknown control opcode — ignore.
                            }
                        } else if let Some(ch) = channels.get(&ch_id) {
                            // Data frame — forward payload to upstream writer.
                            //
                            // `try_send`, not `send().await`: awaiting here
                            // would let one stuck upstream head-of-line-block
                            // every other channel on this session, plus the
                            // close and shutdown arms. A full queue means that
                            // upstream is not draining, so drop the channel and
                            // let the client's per-channel reconnect handle it
                            // — the stream is ordered, so skipping a frame and
                            // carrying on would silently corrupt it.
                            if ch.writer_tx.try_send(payload.to_vec()).is_err() {
                                if let Some(ch) = channels.remove(&ch_id) {
                                    ch.shutdown();
                                }
                                let _ = merge_tx.try_send(mux_control(MUX_S2C_CLOSED, ch_id));
                            }
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
                    let _ = merge_tx.try_send(quit_frame);
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
    merge_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    audio_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> Option<MuxChannelState> {
    let connector = match state.connector_for(&name) {
        Some(c) => c,
        None => {
            eprintln!("mux: unknown destination '{name}'");
            let _ = merge_tx
                .send(mux_error(ch_id, &format!("unknown destination '{name}'")))
                .await;
            return None;
        }
    };

    let connect_result =
        tokio::time::timeout(std::time::Duration::from_secs(30), connector.connect()).await;

    let (sock_reader, sock_writer) = match connect_result {
        Ok(Ok(rw)) => rw,
        Ok(Err(e)) => {
            eprintln!("mux: cannot connect to '{name}': {e}");
            let _ = merge_tx.send(mux_error(ch_id, &e)).await;
            return None;
        }
        Err(_) => {
            let msg = format!("connection to '{name}' timed out");
            eprintln!("mux: {msg}");
            let _ = merge_tx.send(mux_error(ch_id, &msg)).await;
            return None;
        }
    };

    // Writer task: drains payloads from the browser into the upstream socket.
    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_WRITER_QUEUE_FRAMES);
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
    let _ = merge_tx.send(mux_control(MUX_S2C_OPENED, ch_id)).await;

    // Reader task: reads length-prefixed frames from the upstream socket,
    // prepends the channel ID, and feeds them into the bounded data queue.
    // `send().await` here is the whole point: when the browser stops
    // draining, this stops reading and the upstream socket applies real
    // backpressure to the blit server.
    let reader_name = name.to_string();
    let reader_task = tokio::spawn(async move {
        let mut r = sock_reader;
        // Where a gap gets made on the way in, under BLIT_AUDIO_DEBUG.
        //
        // This task is the only reader of the upstream socket, so a video
        // frame that blocks on the data queue also stops audio being read
        // behind it — the priority lane downstream cannot help with a frame
        // this task has not picked up yet. `max_gap` is between audio frames
        // arriving off the socket; `max_block` is the worst time spent handing
        // a bulk frame on. If the gap is large and the block is not, the delay
        // was made upstream of this process.
        let debug = std::env::var_os("BLIT_AUDIO_DEBUG").is_some();
        let mut window_start = std::time::Instant::now();
        let mut last_audio_at = window_start;
        let (mut reads, mut max_gap_ms, mut max_block_ms) = (0u32, 0u32, 0u32);
        // What sat between two audio frames on the socket. If a gap is bulk
        // waiting to be read, these are large; if the gap is silence on the
        // wire, they are zero and the delay was made before the write.
        let (mut bulk_bytes, mut bulk_frames) = (0usize, 0u32);
        let (mut worst_bulk_bytes, mut worst_bulk_frames) = (0usize, 0u32);
        let mut before_read = std::time::Instant::now();
        let mut wait_ms = 0u32;
        let mut worst_wait_ms = 0u32;
        while let Some(data) = read_frame(&mut r).await {
            // How much of the gap was spent with nothing to read, summed
            // across every read in it — attributing only the last one hides
            // time spent waiting on a bulk frame that arrived in between. If
            // a long gap is nearly all wait, the frames were not written yet
            // and the delay is upstream; if it is not, this process made it.
            if debug {
                wait_ms += before_read.elapsed().as_millis() as u32;
            }
            let mut frame = Vec::with_capacity(2 + data.len());
            frame.extend_from_slice(&ch_id.to_le_bytes());
            frame.extend_from_slice(&data);
            // Audio takes the priority lane. Sent rather than try_sent so a
            // browser that stops draining still stalls this reader and lets
            // the upstream socket back up — the same backpressure the data
            // queue provides, which is what stops the server's congestion
            // signal from being hidden in this process.
            let is_audio = data.first() == Some(&S2C_AUDIO_FRAME);
            let at = std::time::Instant::now();
            let queued = if is_audio {
                audio_tx.send(frame).await
            } else {
                data_tx.send(frame).await
            };
            if queued.is_err() {
                break;
            }
            if debug {
                if is_audio {
                    reads += 1;
                    let gap = at.duration_since(last_audio_at).as_millis() as u32;
                    if gap > max_gap_ms {
                        max_gap_ms = gap;
                        worst_bulk_bytes = bulk_bytes;
                        worst_bulk_frames = bulk_frames;
                        worst_wait_ms = wait_ms;
                    }
                    bulk_bytes = 0;
                    bulk_frames = 0;
                    wait_ms = 0;
                    last_audio_at = at;
                    if at.duration_since(window_start) >= std::time::Duration::from_secs(1) {
                        eprintln!(
                            "[gateway reader {reader_name}] audio={reads} \
                             max_gap={max_gap_ms}ms wait={worst_wait_ms}ms max_block={max_block_ms}ms \
                             in_gap={worst_bulk_frames}frames/{worst_bulk_bytes}B",
                        );
                        window_start = at;
                        reads = 0;
                        max_gap_ms = 0;
                        max_block_ms = 0;
                        worst_bulk_bytes = 0;
                        worst_bulk_frames = 0;
                        worst_wait_ms = 0;
                    }
                } else {
                    bulk_bytes += data.len();
                    bulk_frames += 1;
                    max_block_ms = max_block_ms.max(at.elapsed().as_millis() as u32);
                }
            }
            if debug {
                before_read = std::time::Instant::now();
            }
        }
        // Upstream EOF — inject S2C_QUIT as a data frame so the browser's
        // BlitConnection can immediately clear its session state, then send
        // the mux-level CLOSED control frame.
        //
        // Both go through the data queue, not the control one: they are the
        // tail of this channel's byte stream, and the writer serves control
        // first.  Sent out of band they would overtake whatever payload is
        // still queued behind a backpressured browser, and the browser would
        // tear the channel down on top of its own last frames.
        let mut quit_frame = Vec::with_capacity(3);
        quit_frame.extend_from_slice(&ch_id.to_le_bytes());
        quit_frame.push(S2C_QUIT);
        if data_tx.send(quit_frame).await.is_ok() {
            let _ = data_tx.send(mux_control(MUX_S2C_CLOSED, ch_id)).await;
        }
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
    let mut server_config = wt::quinn::ServerConfig::with_crypto(Arc::new(quic_config));

    // Transport-level keepalive: browsers throttle JS timers in background
    // tabs beyond quinn's 30s default idle timeout, so the application-level
    // pings alone can't keep an idle session alive.  Server-initiated QUIC
    // PINGs reset the idle timers on both ends independently of the page.
    let mut transport = wt::quinn::TransportConfig::default();
    // A ten-packet initial window is smaller than a typical surface frame and
    // makes a fresh high-refresh stream stop for ACKs mid-frame. Keep enough
    // initial flight for a local-Wi-Fi BDP, but use CUBIC rather than Quinn's
    // experimental BBR implementation: BBR's 1.25/0.75 ProbeBW gain cycle
    // advances once per RTT and showed up as a deterministic video cadence.
    let mut cubic = wt::quinn::congestion::CubicConfig::default();
    cubic.initial_window(256 * 1024);
    transport.congestion_controller_factory(Arc::new(cubic));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    transport.max_idle_timeout(Some(std::time::Duration::from_secs(60).try_into()?));
    server_config.transport_config(Arc::new(transport));
    Ok(server_config)
}

fn bind_v6only_udp(addr: std::net::SocketAddr) -> std::io::Result<std::net::UdpSocket> {
    let sock = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, None)?;
    sock.set_only_v6(true)?;
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

fn bind_webtransport_endpoint(
    config: wt::quinn::ServerConfig,
    bind_addr: std::net::SocketAddr,
) -> std::io::Result<wt::quinn::Endpoint> {
    if bind_addr.is_ipv6() {
        let sock = bind_v6only_udp(bind_addr)?;
        wt::quinn::Endpoint::new(
            wt::quinn::EndpointConfig::default(),
            Some(config),
            sock,
            wt::quinn::default_runtime().unwrap(),
        )
    } else {
        wt::quinn::Endpoint::server(config, bind_addr)
    }
}

/// `0.0.0.0` means the gateway should be reachable on every local address,
/// including IPv6. Exact IPv4 addresses and all explicit IPv6 addresses keep
/// their requested scope.
fn webtransport_secondary_bind_addr(
    bind_addr: std::net::SocketAddr,
) -> Option<std::net::SocketAddr> {
    if bind_addr.ip().is_ipv4() && bind_addr.ip().is_unspecified() {
        Some(([0, 0, 0, 0, 0, 0, 0, 0], bind_addr.port()).into())
    } else {
        None
    }
}

/// Run the WebTransport server on the configured address.
/// For self-signed certs, regenerates every 13 days.
async fn run_webtransport_loop(state: AppState, addr: &str, has_explicit_cert: bool) {
    let bind_addr: std::net::SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("webtransport: invalid address: {e}");
            return;
        }
    };
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

        let config = match build_quinn_server_config(certs, key) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("webtransport: TLS config error: {e}");
                return;
            }
        };

        // Match the TCP listener's scope exactly. In particular,
        // BLIT_ADDR=127.0.0.1 must not expose QUIC on every interface.
        let endpoint = match bind_webtransport_endpoint(config.clone(), bind_addr) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                eprintln!("webtransport: bind failed on {bind_addr}: {e}");
                *state.wt_cert_hash.write().unwrap() = None;
                return;
            }
        };
        let mut server = wt::Server::new(endpoint);
        let secondary_addr = webtransport_secondary_bind_addr(bind_addr);
        let mut secondary_server =
            secondary_addr.and_then(|addr| match bind_webtransport_endpoint(config, addr) {
                Ok(endpoint) => Some(wt::Server::new(endpoint)),
                Err(e) => {
                    eprintln!(
                        "webtransport: secondary bind failed on {addr} (continuing IPv4-only): {e}"
                    );
                    None
                }
            });

        eprintln!("webtransport cert SHA-256: {hash_hex}");
        *state.wt_cert_hash.write().unwrap() = Some(hash_hex);
        eprintln!("webtransport: listening on {bind_addr} (QUIC)");
        if let Some(addr) = secondary_addr.filter(|_| secondary_server.is_some()) {
            eprintln!("webtransport: listening on {addr} (QUIC)");
        }

        run_wt_accept_loop(
            &state,
            &mut server,
            secondary_server.as_mut(),
            has_explicit_cert,
        )
        .await;
        if has_explicit_cert {
            return;
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

async fn run_wt_accept_loop(
    state: &AppState,
    server: &mut wt::Server,
    secondary: Option<&mut wt::Server>,
    permanent: bool,
) {
    if let Some(secondary) = secondary {
        if permanent {
            loop {
                tokio::select! {
                    req = server.accept() => dispatch_wt_request(req, state),
                    req = secondary.accept() => dispatch_wt_request(req, state),
                }
            }
        }

        let rotate_after = tokio::time::sleep(std::time::Duration::from_secs(13 * 24 * 3600));
        tokio::pin!(rotate_after);
        loop {
            tokio::select! {
                req = server.accept() => dispatch_wt_request(req, state),
                req = secondary.accept() => dispatch_wt_request(req, state),
                _ = &mut rotate_after => {
                    eprintln!("webtransport: rotating self-signed certificate");
                    break;
                }
            }
        }
        return;
    }

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

/// How long each stage of the WebTransport handshake may take.
///
/// Deliberately short. The client opens its stream and writes the passphrase
/// immediately after `wt.ready` resolves, and abandons the whole attempt after
/// a few seconds (`wtConnectTimeoutMs` in js/core/src/transports/mux.ts), so a
/// generous server-side budget buys nothing: it only keeps a slot from
/// `AUTH_MAX_UNAUTHENTICATED` — a *global* limit — reserved for a client that
/// has already left. Enough simultaneously-abandoned probes would then make the
/// throttle answer every other client, including authenticated ones
/// reconnecting, with `AUTH_BUSY`.
const WT_AUTH_STAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Authenticate a WebTransport bidirectional stream.
///
/// Protocol: client sends `[pass_len:2 LE][passphrase]`, server responds
/// with `[1]` (ok) or `[0]` (rejected).  Returns `Ok(())` on success.
async fn wt_authenticate(
    send: &mut wt::SendStream,
    recv: &mut wt::RecvStream,
    passphrase: &blit_webserver::config::AuthPassphrase,
    guard: blit_webserver::config::AuthAttemptGuard,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let auth_result = tokio::time::timeout(WT_AUTH_STAGE_TIMEOUT, async {
        let mut len_buf = [0u8; 2];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("auth read len: {e}"))?;
        let pass_len = u16::from_le_bytes(len_buf) as usize;
        if pass_len > 4096 {
            return Err::<bool, String>("passphrase too long".into());
        }
        let mut pass_buf = vec![0u8; pass_len];
        recv.read_exact(&mut pass_buf)
            .await
            .map_err(|e| format!("auth read pass: {e}"))?;
        let pass = std::str::from_utf8(&pass_buf).unwrap_or("");

        if !passphrase.verify(pass.trim()) {
            send.write_all(&[0]).await.ok();
            return Ok(false);
        }
        Ok(true)
    })
    .await;

    match auth_result {
        Ok(Ok(true)) => guard.record_success(),
        // Only a passphrase that was presented and did not match counts
        // against the peer's failure budget.
        Ok(Ok(false)) => {
            guard.record_failure();
            return Err("authentication failed".into());
        }
        // The stream died before yielding a passphrase — the client abandoning
        // its WebTransport probe to fall back to WebSocket lands here, and it
        // must not push the peer towards a lockout.
        Ok(Err(e)) => {
            drop(guard);
            return Err(e.into());
        }
        Err(_) => {
            drop(guard);
            return Err("authentication timed out".into());
        }
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
    let auth_peer = request.conn().remote_address().ip().to_string();
    let is_mux = is_mux_path(&path);
    let dest_name = resolve_destination_name(&path);
    let Some(auth_guard) = state.auth_throttle.begin(auth_peer.clone()) else {
        request
            .reject(axum::http::StatusCode::TOO_MANY_REQUESTS)
            .await?;
        return Ok(());
    };
    let session = request.ok().await?;

    let (mut send, mut recv) =
        match tokio::time::timeout(WT_AUTH_STAGE_TIMEOUT, session.accept_bi()).await {
            Ok(Ok(streams)) => streams,
            // No passphrase was ever offered, so nothing failed to verify —
            // release the handshake slot without charging the peer.
            Ok(Err(e)) => {
                drop(auth_guard);
                return Err(e.into());
            }
            Err(_) => {
                drop(auth_guard);
                session.close(1, b"authentication timed out");
                return Err("authentication timed out".into());
            }
        };

    wt_authenticate(&mut send, &mut recv, &state.passphrase, auth_guard).await?;

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

    // Control and data channels, same split and same full-queue handling as
    // the WebSocket mux handler above.
    let (merge_tx, merge_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_CONTROL_QUEUE_FRAMES);
    let (data_tx, data_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_DATA_QUEUE_FRAMES);
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(MUX_AUDIO_QUEUE_FRAMES);
    let mut channels: HashMap<u16, MuxChannelState> = HashMap::new();
    let shutdown = state.shutdown.clone();

    // Channel-open tasks (same pattern as the WS mux handler).
    let mut open_tasks: tokio::task::JoinSet<(u16, Option<MuxChannelState>)> =
        tokio::task::JoinSet::new();
    let mut pending_opens: HashMap<u16, tokio::task::AbortHandle> = HashMap::new();

    // Reader task: reads length-prefixed mux frames from the WT stream.
    //
    // Bounded, and awaited rather than dropped: blocking here stops
    // `read_exact` and lets QUIC flow control push back on the client, which
    // is the whole point. The frames are a reliable ordered stream, so
    // dropping one is not an option. `len` is only checked against
    // MAX_FRAME_SIZE, and `vec![0u8; len]` is allocated from the length header
    // before the body arrives, so an unbounded queue here is 16 MiB per
    // queued frame of attacker-chosen depth.
    //
    // The WebSocket mux needs no equivalent: its select loop polls the socket
    // directly, so it is backpressured by construction.
    let (client_frame_tx, mut client_frame_rx) =
        tokio::sync::mpsc::channel::<Vec<u8>>(MUX_CLIENT_QUEUE_FRAMES);
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
            if client_frame_tx.send(buf).await.is_err() {
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
        let mut data_rx = data_rx;
        let mut audio_rx = audio_rx;
        let mut fragmentation = bulk::Fragmentation::default();
        // Length-prefixed on the way out; the reassembly is the browser's.
        async fn write_one(send: &mut wt::SendStream, data: &[u8]) -> bool {
            let mut frame = Vec::with_capacity(4 + data.len());
            frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
            frame.extend_from_slice(data);
            send.write_all(&frame).await.is_ok()
        }
        let mut trace = AudioTrace::new();
        loop {
            let (frame, is_bulk, is_audio) = tokio::select! {
                biased;
                ctrl = merge_rx.recv() => (ctrl, false, false),
                audio = audio_rx.recv() => (audio, false, true),
                data = data_rx.recv() => (data, true, false),
            };
            let Some(frame) = frame else { break };
            if !is_bulk {
                let started = std::time::Instant::now();
                if !write_one(&mut send, &frame).await {
                    break;
                }
                if is_audio {
                    trace.saw_audio(started, std::time::Instant::now());
                }
                continue;
            }
            let bytes = frame.len();
            let started = std::time::Instant::now();
            let mut ok = true;
            match fragment_bulk_frame(&frame, &fragmentation) {
                None => ok = write_one(&mut send, &frame).await,
                Some(pieces) => {
                    for piece in pieces {
                        while let Ok(audio) = audio_rx.try_recv() {
                            let at = std::time::Instant::now();
                            if !write_one(&mut send, &audio).await {
                                ok = false;
                                break;
                            }
                            trace.saw_audio(at, std::time::Instant::now());
                        }
                        if !ok || !write_one(&mut send, &piece).await {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if !ok {
                break;
            }
            let elapsed = started.elapsed();
            trace.saw_bulk(elapsed);
            fragmentation.observe(bytes, elapsed);
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
                            let open_data_tx = data_tx.clone();
                            let open_audio_tx = audio_tx.clone();
                            let abort = open_tasks.spawn(async move {
                                let ch = mux_open_channel(
                                    open_ch, name, open_state, open_merge_tx, open_data_tx,
                                    open_audio_tx,
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
                            let _ = merge_tx.try_send(mux_control(MUX_S2C_CLOSED, close_ch));
                        }
                        _ => {}
                    }
                } else if let Some(ch) = channels.get(&ch_id) {
                    // See the WebSocket path: a full queue means this upstream
                    // is stuck, and awaiting would block every other channel.
                    if ch.writer_tx.try_send(payload.to_vec()).is_err() {
                        if let Some(ch) = channels.remove(&ch_id) {
                            ch.shutdown();
                        }
                        let _ = merge_tx.try_send(mux_control(MUX_S2C_CLOSED, ch_id));
                    }
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
                    let _ = merge_tx.try_send(quit_frame);
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
mod bulk_fragmentation {
    use super::*;
    use std::time::Duration;

    fn data_frame(ch: u16, payload: &[u8]) -> Vec<u8> {
        let mut f = ch.to_le_bytes().to_vec();
        f.extend_from_slice(payload);
        f
    }

    /// Reassemble the way the browser does, so the test asserts on the bytes
    /// the client ends up with rather than on the framing that carried them.
    fn reassemble(pieces: &[Vec<u8>]) -> (u16, Vec<u8>, bool) {
        let ch = u16::from_le_bytes([pieces[0][0], pieces[0][1]]);
        let mut out = Vec::new();
        let mut done = false;
        for piece in pieces {
            assert_eq!(u16::from_le_bytes([piece[0], piece[1]]), ch);
            assert_eq!(piece[2], S2C_FRAGMENT);
            out.extend_from_slice(&piece[4..]);
            done = piece[3] & FRAGMENT_FLAG_LAST != 0;
        }
        (ch, out, done)
    }

    #[test]
    fn an_idle_link_does_not_split_ordinary_video_frames() {
        // The cost of splitting is real — thousands of messages a second that
        // browsers batch — so a link that has never blocked pays nothing.
        let state = bulk::Fragmentation::default();
        let frame = data_frame(3, &vec![7u8; 70 * 1024]);
        assert!(fragment_bulk_frame(&frame, &state).is_none());
    }

    #[test]
    fn a_huge_frame_splits_before_any_backpressure_is_measured() {
        // Nothing has been measured yet when the first one arrives, and half
        // a megabyte is tens of milliseconds of link time on its own.
        let state = bulk::Fragmentation::default();
        let payload = (0..bulk::ALWAYS_BYTES).map(|i| i as u8).collect::<Vec<_>>();
        let pieces = fragment_bulk_frame(&data_frame(3, &payload), &state).expect("split");
        assert!(pieces.len() > 1);
        let (ch, body, done) = reassemble(&pieces);
        assert_eq!(ch, 3);
        assert_eq!(body, payload);
        assert!(done);
    }

    #[test]
    fn splitting_starts_after_writes_are_seen_to_block() {
        let mut state = bulk::Fragmentation::default();
        let payload = vec![1u8; 64 * 1024];
        let frame = data_frame(1, &payload);
        assert!(fragment_bulk_frame(&frame, &state).is_none());

        state.observe(payload.len(), Duration::from_millis(6));
        state.observe(payload.len(), Duration::from_millis(6));

        let pieces = fragment_bulk_frame(&frame, &state).expect("split once blocked");
        assert_eq!(reassemble(&pieces).1, payload);
    }

    /// The server still splits at the 16 MiB frame ceiling, so the gateway
    /// receives fragments as well as whole messages. Splitting one further
    /// must not nest: only the piece that ends the logical message may carry
    /// LAST, or the browser dispatches a half-received message.
    #[test]
    fn re_splitting_a_fragment_does_not_nest() {
        let mut state = bulk::Fragmentation::default();
        state.observe(bulk::ALWAYS_BYTES, Duration::from_millis(6));
        state.observe(bulk::ALWAYS_BYTES, Duration::from_millis(6));

        let body = vec![9u8; 32 * 1024];
        for (flag, ends) in [(FRAGMENT_FLAG_LAST, true), (0, false)] {
            let mut payload = vec![S2C_FRAGMENT, flag];
            payload.extend_from_slice(&body);
            let pieces = fragment_bulk_frame(&data_frame(2, &payload), &state).expect("split");
            let (_, out, done) = reassemble(&pieces);
            assert_eq!(out, body, "payload survives re-splitting");
            assert_eq!(done, ends, "only a terminal fragment stays terminal");
            assert!(
                pieces[..pieces.len() - 1]
                    .iter()
                    .all(|p| p[3] & FRAGMENT_FLAG_LAST == 0),
                "no interior piece claims to end the message",
            );
        }
    }

    #[test]
    fn control_frames_are_never_split() {
        // A channel's CLOSED rides the data queue behind its own last bytes;
        // rewriting it as a fragment would make it a payload for the channel
        // it is closing.
        let mut state = bulk::Fragmentation::default();
        state.observe(bulk::ALWAYS_BYTES, Duration::from_millis(6));
        state.observe(bulk::ALWAYS_BYTES, Duration::from_millis(6));
        let frame = data_frame(MUX_CONTROL, &vec![0u8; 256 * 1024]);
        assert!(fragment_bulk_frame(&frame, &state).is_none());
    }

    #[test]
    fn a_frame_is_never_split_past_what_the_receiver_will_reassemble() {
        // The browser aborts a reassembly past MAX_FRAGMENT_COUNT pieces, and
        // one logical message can arrive here as several frames.
        let mut state = bulk::Fragmentation::default();
        state.observe(bulk::ALWAYS_BYTES, Duration::from_millis(6));
        state.observe(bulk::ALWAYS_BYTES, Duration::from_millis(6));
        let frame = data_frame(1, &vec![0u8; 16 * 1024 * 1024]);
        let pieces = fragment_bulk_frame(&frame, &state).expect("split");
        assert!(pieces.len() <= bulk::MAX_CHUNKS_PER_FRAME);
    }

    #[test]
    fn a_quiet_link_stops_splitting_again() {
        let mut state = bulk::Fragmentation::default();
        let payload = vec![1u8; 64 * 1024];
        state.observe(payload.len(), Duration::from_millis(6));
        state.observe(payload.len(), Duration::from_millis(6));
        assert!(fragment_bulk_frame(&data_frame(1, &payload), &state).is_some());
        for _ in 0..64 {
            state.observe(payload.len(), Duration::from_micros(100));
        }
        assert!(fragment_bulk_frame(&data_frame(1, &payload), &state).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn make_test_state(destinations: DestMap, cors_origin: Option<String>) -> AppState {
        Arc::new(Config {
            passphrase: blit_webserver::config::AuthPassphrase::plaintext("test"),
            destinations: std::sync::RwLock::new(destinations),
            remotes: blit_webserver::config::RemotesState::ephemeral(String::new()),
            roots: blit_webserver::config::RootsState::ephemeral(String::new()),
            cors_origin,
            wt_cert_hash: std::sync::RwLock::new(None),
            wt_public_addr: None,
            config_state: blit_webserver::config::ConfigState::new(),
            proxy_sock: None,
            ssh_pool: blit_ssh::SshPool::new(),
            hub_url: blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL),
            webrtc_enabled: false,
            shutdown: Arc::new(tokio::sync::Notify::new()),
            auth_throttle: blit_webserver::config::AuthThrottle::new(),
        })
    }

    #[test]
    fn webtransport_public_addr_supports_hostname_or_port_only_override() {
        assert_eq!(
            parse_webtransport_public_addr(":10001")
                .unwrap()
                .to_string(),
            ":10001"
        );
        assert_eq!(
            parse_webtransport_public_addr("hound.local:443")
                .unwrap()
                .to_string(),
            "hound.local:443"
        );
        assert_eq!(
            parse_webtransport_public_addr("[::1]:4443")
                .unwrap()
                .to_string(),
            "[::1]:4443"
        );
    }

    #[tokio::test]
    async fn webtransport_endpoint_preserves_loopback_bind_scope() {
        let (certs, key, _) = generate_self_signed_cert();
        let config = build_quinn_server_config(certs, key).unwrap();
        let requested: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

        let endpoint = bind_webtransport_endpoint(config, requested).unwrap();

        assert_eq!(endpoint.local_addr().unwrap().ip(), requested.ip());
    }

    #[test]
    fn webtransport_ipv4_wildcard_adds_ipv6_wildcard() {
        let v4: std::net::SocketAddr = "0.0.0.0:10001".parse().unwrap();
        let loopback: std::net::SocketAddr = "127.0.0.1:10001".parse().unwrap();

        assert_eq!(
            webtransport_secondary_bind_addr(v4),
            Some("[::]:10001".parse().unwrap())
        );
        assert_eq!(webtransport_secondary_bind_addr(loopback), None);
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
    fn named_local_remote_resolves_to_its_instance_socket() {
        let connector = uri_to_connector(
            "local:work",
            &blit_ssh::SshPool::new(),
            "wss://hub.blit.sh",
            false,
        );
        match connector {
            Some(GatewayConnector::Ipc(path)) => {
                assert_eq!(path, blit_webserver::config::local_socket_for_name("work"));
            }
            _ => panic!("expected named local IPC connector"),
        }
        assert!(
            uri_to_connector(
                "local:../../elsewhere",
                &blit_ssh::SshPool::new(),
                "wss://hub.blit.sh",
                false,
            )
            .is_none()
        );
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
