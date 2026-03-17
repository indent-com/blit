use tokio::io::AsyncWriteExt;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

use crate::transport::{self, Transport, make_frame, read_frame};

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

const WEB_INDEX_HTML_BR: &[u8] = include_bytes!("../../../js/ui/dist/index.html.br");

#[derive(Clone)]
enum BrowserConnector {
    /// Local Unix socket / named pipe.
    Ipc(String),
    /// Route every connection through blit-proxy with the given upstream URI.
    #[cfg(unix)]
    Proxied(String),
    /// Direct TCP connection.
    Tcp(String),
    /// WebRTC share session — browser connects directly to the hub.
    WebRtc { passphrase: String, hub: String },
    /// Embedded SSH connection via the shared pool.
    Ssh {
        pool: blit_ssh::SshPool,
        user: Option<String>,
        host: String,
        socket: Option<String>,
    },
}

/// Convert a remotes URI to a `BrowserConnector`.
fn uri_to_browser_connector(
    uri: &str,
    hub: &str,
    ssh_pool: &blit_ssh::SshPool,
) -> Option<BrowserConnector> {
    if let Some(rest) = uri.strip_prefix("ssh:") {
        let (user, host, socket) = blit_ssh::parse_ssh_uri(rest);
        return Some(BrowserConnector::Ssh {
            pool: ssh_pool.clone(),
            user,
            host,
            socket,
        });
    }
    #[cfg(unix)]
    if crate::transport::proxy_enabled() {
        if uri.starts_with("tcp:")
            || uri.starts_with("ws://")
            || uri.starts_with("wss://")
            || uri.starts_with("wt://")
        {
            return Some(BrowserConnector::Proxied(uri.to_string()));
        }
        if let Some(passphrase) = uri.strip_prefix("share:") {
            let proxy_uri = crate::transport::share_proxy_uri(passphrase, hub);
            return Some(BrowserConnector::Proxied(proxy_uri));
        }
    }
    if let Some(path) = uri.strip_prefix("socket:") {
        return Some(BrowserConnector::Ipc(path.to_string()));
    }
    if let Some(addr) = uri.strip_prefix("tcp:") {
        return Some(BrowserConnector::Tcp(addr.to_string()));
    }
    if let Some(passphrase) = uri.strip_prefix("share:") {
        let passphrase = passphrase.split('?').next().unwrap_or(passphrase);
        return Some(BrowserConnector::WebRtc {
            passphrase: passphrase.to_string(),
            hub: blit_webrtc_forwarder::normalize_hub(hub),
        });
    }
    if uri == "local" {
        return Some(BrowserConnector::Ipc(
            crate::transport::default_local_socket(),
        ));
    }
    None
}

impl BrowserConnector {
    async fn connect(&self) -> Result<Transport, String> {
        match self {
            Self::Ipc(p) => transport::connect_ipc(p).await,
            #[cfg(unix)]
            Self::Proxied(uri) => transport::connect_via_proxy(uri).await,
            Self::Tcp(addr) => {
                let s = tokio::net::TcpStream::connect(addr.as_str())
                    .await
                    .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
                let _ = s.set_nodelay(true);
                Ok(Transport::Tcp(s))
            }
            Self::WebRtc { passphrase, hub } => {
                let stream = blit_webrtc_forwarder::client::connect(passphrase, hub)
                    .await
                    .map_err(|e| format!("share:{passphrase}: {e}"))?;
                Ok(Transport::Duplex(stream))
            }
            Self::Ssh {
                pool,
                user,
                host,
                socket,
            } => {
                let stream = pool
                    .connect(host, user.as_deref(), socket.as_deref())
                    .await
                    .map_err(|e| format!("ssh:{host}: {e}"))?;
                Ok(Transport::Duplex(stream))
            }
        }
    }
}

struct DestinationInfo {
    connector: BrowserConnector,
}

struct BrowserState {
    token: String,
    destinations: std::sync::RwLock<std::collections::HashMap<String, DestinationInfo>>,
    config: blit_webserver::config::ConfigState,
    remotes: blit_webserver::config::RemotesState,
    /// Hub URL for converting `share:` remotes entries to connectors.
    hub: String,
    /// Shared SSH connection pool.
    ssh_pool: blit_ssh::SshPool,
}

pub async fn run_browser(port: Option<u16>, hub: &str) {
    let token: String = {
        use rand::RngExt as _;
        rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(32)
            .map(|b| b as char)
            .collect()
    };

    let bind_port: u16 = port.unwrap_or(0);
    let ssh_pool = blit_ssh::SshPool::new();

    // Use persistent RemotesState backed by ~/.config/blit/blit.remotes.
    // This watches the file for external changes and broadcasts updates.
    let remotes = blit_webserver::config::RemotesState::new();

    // Ensure local blit-server is running.
    let local_path = transport::default_local_socket();
    if let Err(e) = transport::ensure_local_server(&local_path).await {
        eprintln!("blit: {e}");
        std::process::exit(1);
    }

    // Build initial destinations from blit.remotes + local.
    let mut destinations = std::collections::HashMap::new();
    destinations.insert(
        "local".to_string(),
        DestinationInfo {
            connector: BrowserConnector::Ipc(local_path),
        },
    );
    let initial_remotes = blit_webserver::config::parse_remotes_str(&remotes.get());
    for (name, uri) in &initial_remotes {
        if name == "local" {
            continue; // Already added above
        }
        if let Some(connector) = uri_to_browser_connector(uri, hub, &ssh_pool) {
            destinations.insert(name.clone(), DestinationInfo { connector });
        }
    }

    let state = Arc::new(BrowserState {
        token: token.clone(),
        destinations: std::sync::RwLock::new(destinations),
        config: blit_webserver::config::ConfigState::new(),
        remotes,
        hub: hub.to_string(),
        ssh_pool,
    });

    // Reconcile destinations whenever blit.remotes changes (from the
    // Remotes dialog, `blit remote add` in another terminal, etc.).
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
                let mut map = recon_state.destinations.write().unwrap();
                // Remove destinations no longer in the list (except "local").
                map.retain(|name, _| name == "local" || entries.iter().any(|(n, _)| n == name));
                // Add new entries.
                for (name, uri) in &entries {
                    if !map.contains_key(name)
                        && let Some(connector) =
                            uri_to_browser_connector(uri, &recon_state.hub, &recon_state.ssh_pool)
                    {
                        map.insert(name.clone(), DestinationInfo { connector });
                    }
                }
            }
        });
    }

    let html_etag: &'static str =
        Box::leak(blit_webserver::html_etag(WEB_INDEX_HTML_BR).into_boxed_str());

    let app = axum::Router::new()
        .route(
            "/config",
            get(
                move |axum::extract::State(state): axum::extract::State<Arc<BrowserState>>,
                      request: axum::extract::Request| async move {
                    match WebSocketUpgrade::from_request(request, &state).await {
                        Ok(ws) => ws
                            .on_upgrade(move |socket| async move {
                                blit_webserver::config::handle_config_ws(
                                    socket,
                                    &state.token,
                                    &state.config,
                                    Some(&state.remotes),
                                    None,
                                )
                                .await;
                            })
                            .into_response(),
                        Err(e) => e.into_response(),
                    }
                },
            ),
        )
        .fallback(get(move |state, request| {
            browser_root_handler(state, request, html_etag)
        }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{bind_port}"))
        .await
        .unwrap_or_else(|e| {
            eprintln!("blit: cannot bind to port {bind_port}: {e}");
            std::process::exit(1);
        });
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/#{token}");
    eprintln!("blit: serving browser UI at {url}");

    open_browser(&url);

    tokio::select! {
        r = axum::serve(listener, app) => { if let Err(e) = r { eprintln!("blit: serve error: {e}"); } }
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    eprintln!("blit: open {url} in your browser");
}

/// Resolve which destination a request is for.
/// `/d/{name}` -> named destination.
fn resolve_destination_name(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix("/d/") {
        let name = rest.split('/').next().unwrap_or(rest);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

async fn browser_root_handler(
    axum::extract::State(state): axum::extract::State<Arc<BrowserState>>,
    request: axum::extract::Request,
    etag: &'static str,
) -> Response {
    let path = request.uri().path().to_string();

    if let Some(resp) = blit_webserver::try_font_route(&path, None) {
        return resp;
    }

    let is_ws = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);

    if is_ws {
        let dest_name = resolve_destination_name(&path);
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| browser_handle_ws(socket, state, dest_name)),
            Err(e) => e.into_response(),
        }
    } else {
        let inm = request
            .headers()
            .get(axum::http::header::IF_NONE_MATCH)
            .map(|v| v.as_bytes());
        let ae = request
            .headers()
            .get(axum::http::header::ACCEPT_ENCODING)
            .and_then(|v| v.to_str().ok());
        blit_webserver::html_response(WEB_INDEX_HTML_BR, etag, inm, ae)
    }
}

async fn browser_handle_ws(mut ws: WebSocket, state: Arc<BrowserState>, dest_name: Option<String>) {
    // Authenticate.
    let authed = loop {
        match ws.recv().await {
            Some(Ok(Message::Text(pass))) => {
                if constant_time_eq(pass.trim().as_bytes(), state.token.as_bytes()) {
                    break true;
                } else {
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

    // Resolve the connector.
    let connector_result: Result<BrowserConnector, String> = {
        let dests = state.destinations.read().unwrap();
        match dest_name.as_deref() {
            Some(name) => match dests.get(name) {
                Some(info) => Ok(info.connector.clone()),
                None => Err(format!("error:unknown destination '{name}'")),
            },
            None => Err("error:no destination specified".to_string()),
        }
    };
    let connector = match connector_result {
        Ok(c) => c,
        Err(msg) => {
            let _ = ws.send(Message::Text(msg.into())).await;
            let _ = ws.close().await;
            return;
        }
    };

    let transport = match connector.connect().await {
        Ok(t) => t,
        Err(e) => {
            let dest_label = dest_name.as_deref().unwrap_or("default");
            eprintln!("blit: transport connect failed for '{dest_label}': {e}");
            let _ = ws.send(Message::Text(format!("error:{e}").into())).await;
            let _ = ws.close().await;
            return;
        }
    };
    let dest_label = dest_name.as_deref().unwrap_or("default");
    let _ = ws.send(Message::Text("ok".into())).await;
    eprintln!("blit: browser client connected to '{dest_label}'");

    let (mut transport_reader, mut transport_writer) = transport.split();
    let (mut ws_tx, mut ws_rx) = ws.split();

    let mut transport_to_ws = tokio::spawn(async move {
        let mut frames = 0u64;
        while let Some(data) = read_frame(&mut transport_reader).await {
            frames += 1;
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
        if frames == 0 {
            let _ = ws_tx
                .send(Message::Text(
                    "error:blit-server not reachable (is it running on the remote host?)".into(),
                ))
                .await;
        }
    });

    let mut ws_to_transport = tokio::spawn(async move {
        while let Some(msg_result) = ws_rx.next().await {
            match msg_result {
                Ok(Message::Binary(d)) => {
                    let frame = make_frame(&d);
                    if transport_writer.write_all(&frame).await.is_err() {
                        eprintln!("blit: ws->transport: write failed");
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    eprintln!("blit: ws->transport: ws error: {e}");
                    break;
                }
                _ => continue,
            }
        }
    });

    tokio::select! {
        _ = &mut transport_to_ws => {}
        _ = &mut ws_to_transport => {}
    }
    transport_to_ws.abort();
    ws_to_transport.abort();

    eprintln!("blit: browser client disconnected from '{dest_label}'");
}
