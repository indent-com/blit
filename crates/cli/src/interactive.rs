use tokio::io::AsyncWriteExt;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, WebSocketUpgrade};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::task::JoinHandle;

use crate::transport::{self, Transport, make_frame, read_frame, write_frame};

/// Blit protocol: server is shutting down (single byte, no payload).
const S2C_QUIT: u8 = 0x0C;

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        diff |= a[i] ^ b[i];
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
    #[cfg(unix)]
    if crate::transport::proxy_enabled() {
        if uri.starts_with("ssh:")
            || uri.starts_with("tcp:")
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
    if let Some(rest) = uri.strip_prefix("ssh:") {
        let (user, host, socket) = blit_ssh::parse_ssh_uri(rest);
        return Some(BrowserConnector::Ssh {
            pool: ssh_pool.clone(),
            user,
            host,
            socket,
        });
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
                    .map_err(|e| format!("share: {e}"))?;
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
    /// Broadcast notification triggered on SIGINT so active WebSocket
    /// handlers can send `S2C_QUIT` before the process exits.
    shutdown: Arc<tokio::sync::Notify>,
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

    // Build initial destinations from blit.remotes.  When the file does
    // not exist, `read_remotes()` (called by `RemotesState::new`) auto-
    // provisions it with `local = local`, so there is always at least one
    // entry.  The "local" URI is resolved to an IPC connector by
    // `uri_to_browser_connector`.
    let mut destinations = std::collections::HashMap::new();
    let initial_remotes = blit_webserver::config::parse_remotes_str(&remotes.get());
    for (name, uri) in &initial_remotes {
        if let Some(connector) = uri_to_browser_connector(uri, hub, &ssh_pool) {
            destinations.insert(name.clone(), DestinationInfo { connector });
        }
    }

    let shutdown = Arc::new(tokio::sync::Notify::new());

    let state = Arc::new(BrowserState {
        token: token.clone(),
        destinations: std::sync::RwLock::new(destinations),
        config: blit_webserver::config::ConfigState::new(),
        remotes,
        hub: hub.to_string(),
        ssh_pool,
        shutdown: shutdown.clone(),
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
                // Remove destinations no longer in the list.
                map.retain(|name, _| entries.iter().any(|(n, _)| n == name));
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
                                    &[],
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

    let graceful = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = tokio::signal::ctrl_c().await;
        // Notify all active handlers so they can send S2C_QUIT.
        shutdown.notify_waiters();
    });
    if let Err(e) = graceful.await {
        eprintln!("blit: serve error: {e}");
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

    if is_ws && (path == "/mux" || path.ends_with("/mux")) {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| browser_handle_mux_ws(socket, state)),
            Err(e) => e.into_response(),
        }
    } else if is_ws {
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

    let shutdown = state.shutdown.clone();
    let mut transport_to_ws = tokio::spawn(async move {
        let mut frames = 0u64;
        loop {
            tokio::select! {
                frame = read_frame(&mut transport_reader) => {
                    match frame {
                        Some(data) => {
                            frames += 1;
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
        if frames == 0 {
            let _ = ws_tx
                .send(Message::Text(
                    "error:blit-server not reachable (is it running on the remote host?)".into(),
                ))
                .await;
        } else {
            let _ = ws_tx.send(Message::Binary(vec![S2C_QUIT].into())).await;
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

// ---------------------------------------------------------------------------
// Multiplexed WebSocket handler.
// ---------------------------------------------------------------------------

const MUX_CONTROL: u16 = 0xFFFF;

const MUX_C2S_OPEN: u8 = 0x01;
const MUX_C2S_CLOSE: u8 = 0x02;

const MUX_S2C_OPENED: u8 = 0x81;
const MUX_S2C_CLOSED: u8 = 0x82;
const MUX_S2C_ERROR: u8 = 0x83;

fn mux_control(opcode: u8, ch: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    buf.extend_from_slice(&MUX_CONTROL.to_le_bytes());
    buf.push(opcode);
    buf.extend_from_slice(&ch.to_le_bytes());
    buf
}

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

struct MuxChannelState {
    writer_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    writer_task: JoinHandle<()>,
    reader_task: JoinHandle<()>,
}

impl MuxChannelState {
    fn shutdown(self) {
        drop(self.writer_tx);
        self.writer_task.abort();
        self.reader_task.abort();
    }
}

async fn browser_handle_mux_ws(mut ws: WebSocket, state: Arc<BrowserState>) {
    // --- Authentication ---
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

    let _ = ws.send(Message::Text("mux".into())).await;
    eprintln!("blit: mux client authenticated");

    let (mut ws_tx, mut ws_rx) = ws.split();
    let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let mut channels: HashMap<u16, MuxChannelState> = HashMap::new();
    let shutdown = state.shutdown.clone();

    loop {
        tokio::select! {
            biased;

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
                            if payload.is_empty() { continue; }
                            match payload[0] {
                                MUX_C2S_OPEN => {
                                    if payload.len() < 5 { continue; }
                                    let open_ch = u16::from_le_bytes([payload[1], payload[2]]);
                                    let name_len = u16::from_le_bytes([payload[3], payload[4]]) as usize;
                                    if payload.len() < 5 + name_len { continue; }
                                    let name = std::str::from_utf8(&payload[5..5 + name_len])
                                        .unwrap_or("");

                                    if let Some(prev) = channels.remove(&open_ch) {
                                        prev.shutdown();
                                    }

                                    cli_mux_open_channel(
                                        open_ch,
                                        name,
                                        &state,
                                        &merge_tx,
                                        &mut channels,
                                    )
                                    .await;
                                }
                                MUX_C2S_CLOSE => {
                                    if payload.len() < 3 { continue; }
                                    let close_ch = u16::from_le_bytes([payload[1], payload[2]]);
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
                    Message::Close(_) => break,
                    _ => {}
                }
            }

            frame = merge_rx.recv() => {
                match frame {
                    Some(data) => {
                        if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }

            // Server is shutting down — send S2C_QUIT on every open channel.
            _ = shutdown.notified() => {
                for &ch_id in channels.keys() {
                    let mut quit_frame = Vec::with_capacity(3);
                    quit_frame.extend_from_slice(&ch_id.to_le_bytes());
                    quit_frame.push(S2C_QUIT);
                    if ws_tx.send(Message::Binary(quit_frame.into())).await.is_err() {
                        break;
                    }
                }
                break;
            }
        }
    }

    for (_, ch) in channels {
        ch.shutdown();
    }
    eprintln!("blit: mux client disconnected");
}

async fn cli_mux_open_channel(
    ch_id: u16,
    name: &str,
    state: &Arc<BrowserState>,
    merge_tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    channels: &mut HashMap<u16, MuxChannelState>,
) {
    let connector = {
        let dests = state.destinations.read().unwrap();
        match dests.get(name) {
            Some(info) => info.connector.clone(),
            None => {
                eprintln!("blit: mux: unknown destination '{name}'");
                let _ = merge_tx.send(mux_error(ch_id, &format!("unknown destination '{name}'")));
                return;
            }
        }
    };

    let transport = match connector.connect().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("blit: mux: cannot connect to '{name}': {e}");
            let _ = merge_tx.send(mux_error(ch_id, &e));
            return;
        }
    };

    let (mut sock_reader, mut sock_writer) = transport.split();

    let (writer_tx, mut writer_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(async move {
        while let Some(payload) = writer_rx.recv().await {
            if !write_frame(&mut sock_writer, &payload).await {
                break;
            }
        }
    });

    let _ = merge_tx.send(mux_control(MUX_S2C_OPENED, ch_id));

    let reader_merge_tx = merge_tx.clone();
    let reader_task = tokio::spawn(async move {
        while let Some(data) = read_frame(&mut sock_reader).await {
            let mut frame = Vec::with_capacity(2 + data.len());
            frame.extend_from_slice(&ch_id.to_le_bytes());
            frame.extend_from_slice(&data);
            if reader_merge_tx.send(frame).is_err() {
                break;
            }
        }
        let _ = reader_merge_tx.send(mux_control(MUX_S2C_CLOSED, ch_id));
    });

    channels.insert(
        ch_id,
        MuxChannelState {
            writer_tx,
            writer_task,
            reader_task,
        },
    );

    eprintln!("blit: mux: channel {ch_id} opened for '{name}'");
}
