use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, State, WebSocketUpgrade};
use axum::http::header::CONTENT_TYPE;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// Wraps TcpListener to set TCP_NODELAY on every accepted connection,
/// disabling Nagle's algorithm for low-latency frame delivery.
struct NoDelayListener(tokio::net::TcpListener);

impl axum::serve::Listener for NoDelayListener {
    type Io = tokio::net::TcpStream;
    type Addr = std::net::SocketAddr;

    fn accept(&mut self) -> impl std::future::Future<Output = (Self::Io, Self::Addr)> + Send {
        async {
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

const INDEX_HTML: &str = include_str!("../../web/index.html");
const WEB_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../web");

struct Config {
    passphrase: String,
    sock_path: String,
}

type AppState = Arc<Config>;

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await.ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> bool {
    let len = payload.len() as u32;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(payload);
    writer.write_all(&buf).await.is_ok()
}

#[tokio::main]
async fn main() {
    let passphrase = std::env::var("BLIT_PASS").unwrap_or_else(|_| {
        eprintln!("BLIT_PASS environment variable required");
        std::process::exit(1);
    });
    let sock_path = std::env::var("BLIT_SOCK").unwrap_or_else(|_| {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
            format!("{dir}/blit.sock")
        } else {
            "/tmp/blit.sock".into()
        }
    });
    let addr = std::env::var("BLIT_ADDR").unwrap_or_else(|_| "0.0.0.0:3264".into());

    let state: AppState = Arc::new(Config {
        passphrase,
        sock_path,
    });

    let app = axum::Router::new()
        .fallback(get(root_handler))
        .with_state(state);

    let tcp = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let listener = NoDelayListener(tcp);
    eprintln!("listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn root_handler(State(state): State<AppState>, request: axum::extract::Request) -> Response {
    let path = request.uri().path();
    if let Some(asset_path) = static_asset_path(path) {
        if let Ok(bytes) = tokio::fs::read(&asset_path).await {
            return ([(CONTENT_TYPE, content_type(&asset_path))], bytes).into_response();
        }
    }
    let is_ws = request
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if is_ws {
        match WebSocketUpgrade::from_request(request, &state).await {
            Ok(ws) => ws.on_upgrade(move |socket| handle_ws(socket, state)),
            Err(e) => e.into_response(),
        }
    } else {
        Html(INDEX_HTML).into_response()
    }
}

fn static_asset_path(path: &str) -> Option<PathBuf> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let relative = Path::new(trimmed);
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(Path::new(WEB_ROOT).join(relative))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or_default() {
        "js" => "application/javascript",
        "wasm" => "application/wasm",
        "d.ts" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn handle_ws(mut ws: WebSocket, state: AppState) {
    let authed = loop {
        match ws.recv().await {
            Some(Ok(Message::Text(pass))) => {
                if pass.trim() == state.passphrase {
                    let _ = ws.send(Message::Text("ok".into())).await;
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
    eprintln!("client authenticated");

    let stream = match UnixStream::connect(&state.sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot connect to blit-server: {e}");
            return;
        }
    };
    let (mut sock_reader, mut sock_writer) = stream.into_split();
    let (mut ws_tx, mut ws_rx) = ws.split();

    let ws_to_sock = tokio::spawn(async move {
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

    let sock_to_ws = tokio::spawn(async move {
        while let Some(data) = read_frame(&mut sock_reader).await {
            if ws_tx.send(Message::Binary(data.into())).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = ws_to_sock => {}
        _ = sock_to_ws => {}
    }

    eprintln!("client disconnected");
}
