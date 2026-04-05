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

enum BrowserConnector {
    Ipc(String),
    #[cfg(unix)]
    SshForward {
        local_sock: String,
        _ssh_child: tokio::process::Child,
    },
    Tcp(String),
}

impl BrowserConnector {
    async fn connect(&self) -> Result<Transport, String> {
        match self {
            #[cfg(unix)]
            Self::SshForward { local_sock: p, .. } => transport::connect_ipc(p).await,
            Self::Ipc(p) => transport::connect_ipc(p).await,
            Self::Tcp(addr) => {
                let s = tokio::net::TcpStream::connect(addr.as_str())
                    .await
                    .map_err(|e| format!("cannot connect to {addr}: {e}"))?;
                let _ = s.set_nodelay(true);
                Ok(Transport::Tcp(s))
            }
        }
    }
}

struct DestinationInfo {
    connector: BrowserConnector,
    label: String,
}

struct BrowserState {
    token: String,
    destinations: std::collections::HashMap<String, DestinationInfo>,
    config: blit_webserver::config::ConfigState,
}

pub async fn run_browser(dests: Vec<crate::NamedDestination>, port: Option<u16>) {
    use crate::{Destination, NamedDestination};

    let token: String = {
        use rand::RngExt as _;
        rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(32)
            .map(|b| b as char)
            .collect()
    };

    let bind_port: u16 = port.unwrap_or(0);

    // Build one BrowserConnector per destination.
    let mut destinations = std::collections::HashMap::new();
    for NamedDestination { name, dest } in &dests {
        let (connector, label) = match dest {
            Destination::Ssh { user, host, socket } => {
                #[cfg(unix)]
                {
                    let ssh_target = match user {
                        Some(u) => format!("{u}@{host}"),
                        None => host.clone(),
                    };
                    let connector =
                        setup_ssh_forward(std::slice::from_ref(&ssh_target), socket.as_deref())
                            .await;
                    (connector, host.clone())
                }
                #[cfg(not(unix))]
                {
                    let _ = (user, host, socket);
                    eprintln!("blit: SSH destinations are not supported on this platform");
                    std::process::exit(1);
                }
            }
            Destination::Tcp(addr) => {
                let label = addr.split(':').next().unwrap_or(addr).to_string();
                (BrowserConnector::Tcp(addr.clone()), label)
            }
            Destination::Socket(path) => (BrowserConnector::Ipc(path.clone()), name.clone()),
            Destination::Local => {
                let path = transport::default_local_socket();
                if let Err(e) = transport::ensure_local_server(&path).await {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
                (BrowserConnector::Ipc(path), "local".into())
            }
        };
        destinations.insert(name.clone(), DestinationInfo { connector, label });
    }

    // Test each connector (up to 30 attempts per destination).
    for (name, info) in &destinations {
        let mut attempts = 0;
        loop {
            match info.connector.connect().await {
                Ok(_) => break,
                Err(e) if attempts < 30 => {
                    attempts += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if attempts == 30 {
                        eprintln!("blit: destination '{name}': {e}");
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("blit: destination '{name}': {e}");
                    std::process::exit(1);
                }
            }
        }
    }

    let state = Arc::new(BrowserState {
        token: token.clone(),
        destinations,
        config: blit_webserver::config::ConfigState::new(),
    });

    let html_etag: &'static str =
        Box::leak(blit_webserver::html_etag(WEB_INDEX_HTML_BR).into_boxed_str());

    let app = axum::Router::new()
        .route(
            "/config",
            get(
                move |axum::extract::State(state): axum::extract::State<Arc<BrowserState>>,
                      request: axum::extract::Request| async move {
                    let is_ws = request
                        .headers()
                        .get("upgrade")
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.eq_ignore_ascii_case("websocket"))
                        .unwrap_or(false);
                    if is_ws {
                        match WebSocketUpgrade::from_request(request, &state).await {
                            Ok(ws) => ws
                                .on_upgrade(move |socket| async move {
                                    blit_webserver::config::handle_config_ws(
                                        socket,
                                        &state.token,
                                        &state.config,
                                    )
                                    .await;
                                })
                                .into_response(),
                            Err(e) => e.into_response(),
                        }
                    } else {
                        config_json_response(&state)
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

/// Build the JSON /config response advertising all destinations.
fn config_json_response(state: &BrowserState) -> Response {
    let mut json = String::from("{\"gateway\":true,\"destinations\":[");
    let mut first = true;
    // Iterate in deterministic order for stable config responses.
    let mut names: Vec<&String> = state.destinations.keys().collect();
    names.sort();
    for name in names {
        let info = &state.destinations[name];
        if !first {
            json.push(',');
        }
        first = false;
        json.push_str("{\"id\":\"");
        json.push_str(&json_escape(name));
        json.push_str("\",\"type\":\"gateway\",\"label\":\"");
        json.push_str(&json_escape(&info.label));
        json.push_str("\"}");
    }
    json.push_str("]}");
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json,
    )
        .into_response()
}

/// Minimal JSON string escaping for destination names.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(unix)]
async fn setup_ssh_forward(ssh_args: &[String], remote_socket: Option<&str>) -> BrowserConnector {
    if ssh_args.is_empty() {
        eprintln!("blit: ssh requires a host argument");
        std::process::exit(1);
    }

    let remote_sock = if let Some(path) = remote_socket {
        path.to_owned()
    } else {
        let resolve_script = format!("sh -c '{}; echo \"$S\"'", crate::transport::SSH_SOCK_SEARCH);
        let resolve = tokio::process::Command::new("ssh")
            .arg("-T")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
            .arg("-o")
            .arg("ControlPersist=300")
            .args(ssh_args)
            .arg("--")
            .arg(resolve_script)
            .output()
            .await
            .unwrap_or_else(|e| {
                eprintln!("blit: ssh: {e}");
                std::process::exit(1);
            });

        if !resolve.status.success() {
            let stderr = String::from_utf8_lossy(&resolve.stderr);
            eprintln!(
                "blit: ssh failed to resolve remote socket path: {}",
                stderr.trim()
            );
            std::process::exit(1);
        }

        let path = String::from_utf8_lossy(&resolve.stdout).trim().to_owned();
        if path.is_empty() {
            eprintln!("blit: could not determine remote blit socket path");
            std::process::exit(1);
        }
        path
    };

    let escaped_sock = crate::transport::shell_escape(&remote_sock);
    let autostart_script = format!(
        "sh -c 'S={escaped_sock}; {}; echo ok'",
        crate::transport::SSH_AUTOSTART
    );
    let autostart = tokio::process::Command::new("ssh")
        .arg("-T")
        .arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
        .arg("-o")
        .arg("ControlPersist=300")
        .args(ssh_args)
        .arg("--")
        .arg(autostart_script)
        .output()
        .await
        .unwrap_or_else(|e| {
            eprintln!("blit: ssh: {e}");
            std::process::exit(1);
        });

    if !autostart.status.success() {
        let stderr = String::from_utf8_lossy(&autostart.stderr);
        eprintln!("blit: ssh autostart failed: {}", stderr.trim());
        std::process::exit(1);
    }

    let local_sock = format!("/tmp/blit-browser-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&local_sock);

    let child = tokio::process::Command::new("ssh")
        .arg("-N")
        .arg("-T")
        .arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
        .arg("-o")
        .arg("ControlPersist=300")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("StreamLocalBindUnlink=yes")
        .arg("-L")
        .arg(format!("{local_sock}:{remote_sock}"))
        .args(ssh_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("blit: ssh: {e}");
            std::process::exit(1);
        });

    BrowserConnector::SshForward {
        local_sock,
        _ssh_child: child,
    }
}

pub async fn run_browser_share(passphrase: &str, hub: &str, port: Option<u16>) {
    let html_etag: &'static str =
        Box::leak(blit_webserver::html_etag(WEB_INDEX_HTML_BR).into_boxed_str());

    let hub_json: &'static str = Box::leak(format!("{{\"hub\":\"{hub}\"}}").into_boxed_str());

    let config = Arc::new(blit_webserver::config::ConfigState::new());
    let passphrase = passphrase.to_owned();

    let bind_port = port.unwrap_or(0);
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{bind_port}"))
        .await
        .unwrap_or_else(|e| {
            eprintln!("blit: cannot bind to port {bind_port}: {e}");
            std::process::exit(1);
        });
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}/#{passphrase}");
    eprintln!("blit: serving browser UI at {url}");

    let app = axum::Router::new()
        .route(
            "/config",
            get({
                let config = config;
                let passphrase = passphrase.clone();
                move |request: axum::extract::Request| {
                    let config = config.clone();
                    let passphrase = passphrase.clone();
                    async move {
                        let is_ws = request
                            .headers()
                            .get("upgrade")
                            .and_then(|v| v.to_str().ok())
                            .map(|v| v.eq_ignore_ascii_case("websocket"))
                            .unwrap_or(false);
                        if is_ws {
                            match WebSocketUpgrade::from_request(request, &()).await {
                                Ok(ws) => ws
                                    .on_upgrade(move |socket| async move {
                                        blit_webserver::config::handle_config_ws(
                                            socket,
                                            &passphrase,
                                            &config,
                                        )
                                        .await;
                                    })
                                    .into_response(),
                                Err(e) => e.into_response(),
                            }
                        } else {
                            (
                                [(axum::http::header::CONTENT_TYPE, "application/json")],
                                hub_json,
                            )
                                .into_response()
                        }
                    }
                }
            }),
        )
        .fallback(get(move |request: axum::extract::Request| async move {
            if let Some(resp) = blit_webserver::try_font_route(request.uri().path(), None) {
                return resp;
            }
            let inm = request
                .headers()
                .get(axum::http::header::IF_NONE_MATCH)
                .map(|v| v.as_bytes());
            let ae = request
                .headers()
                .get(axum::http::header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok());
            blit_webserver::html_response(WEB_INDEX_HTML_BR, html_etag, inm, ae)
        }));

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
/// `/` (root) -> first destination (backward compat for old browsers).
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
        // Determine which destination this WebSocket is for.
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

    // Resolve the connector for this destination.
    let connector = if let Some(ref name) = dest_name {
        match state.destinations.get(name) {
            Some(info) => &info.connector,
            None => {
                let _ = ws
                    .send(Message::Text(
                        format!("error:unknown destination '{name}'").into(),
                    ))
                    .await;
                let _ = ws.close().await;
                return;
            }
        }
    } else {
        // Root path -> first destination (sorted for determinism).
        let mut names: Vec<&String> = state.destinations.keys().collect();
        names.sort();
        match names.first().and_then(|n| state.destinations.get(*n)) {
            Some(info) => &info.connector,
            None => {
                let _ = ws
                    .send(Message::Text("error:no destinations configured".into()))
                    .await;
                let _ = ws.close().await;
                return;
            }
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
