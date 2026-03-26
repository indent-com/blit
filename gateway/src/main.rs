use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, State, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
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

const INDEX_HTML: &str = include_str!("../../web-app/dist/index.html");

struct Config {
    passphrase: String,
    sock_path: String,
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
            println!("blit-gateway {} — terminal streaming WebSocket gateway", env!("CARGO_PKG_VERSION"));
            println!();
            println!("All configuration is via environment variables:");
            println!("  BLIT_PASS    Browser passphrase (required)");
            println!("  BLIT_ADDR    Listen address (default: 0.0.0.0:3264)");
            println!("  BLIT_SOCK    Upstream server socket");
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

    let app = build_app(state);

    let tcp = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        eprintln!("blit-gateway: cannot bind to {addr}: {e}");
        std::process::exit(1);
    });
    let listener = NoDelayListener(tcp);
    eprintln!("listening on {addr}");
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
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
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
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
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
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/html"));
    }

}
