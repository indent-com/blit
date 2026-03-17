use axum::extract::ws::{Message, WebSocket};
use axum::extract::{FromRequest, State, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

const INDEX_HTML: &str = include_str!("../../web/index.html");
const BROWSER_JS: &[u8] = include_bytes!("../../web/blit_browser.js");
const BROWSER_WASM: &[u8] = include_bytes!("../../web/blit_browser_bg.wasm");

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

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    eprintln!("listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

async fn root_handler(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Response {
    let path = request.uri().path();
    if path.ends_with("/blit_browser.js") {
        return (
            [(axum::http::header::CONTENT_TYPE, "application/javascript")],
            BROWSER_JS,
        )
            .into_response();
    }
    if path.ends_with("/blit_browser_bg.wasm") {
        return (
            [(axum::http::header::CONTENT_TYPE, "application/wasm")],
            BROWSER_WASM,
        )
            .into_response();
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
            if ws_tx
                .send(Message::Binary(data.into()))
                .await
                .is_err()
            {
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
