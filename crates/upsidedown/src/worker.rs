//! Relay worker (docs/upsidedown.md § Workers).
//!
//! One external port, TCP and UDP: WebTransport (uplink sessions and
//! browser consumers) over QUIC on UDP, WebSocket consumers over TLS on
//! TCP.  Consumer connections are spliced onto streams of the session's
//! uplink.  TLS uses the shared certificate from the store.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::tungstenite::Message;
use web_transport_quinn as wt;

use crate::jwt::{self, Role};
use crate::store::Store;

const HEARTBEAT: Duration = Duration::from_secs(5);
const KEY_TTL_SECS: u64 = 15;
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Session close codes (docs/upsidedown.md § Consumers).
const CLOSE_AUTH: u32 = 403;
const CLOSE_NO_UPLINK: u32 = 404;
const CLOSE_SUPERSEDED: u32 = 409;
const CLOSE_SHUTDOWN: u32 = 2;

struct WorkerState {
    name: String,
    port: u16,
    store: Store,
    keys: Vec<ed25519_dalek::VerifyingKey>,
    seal_key: [u8; 32],
    /// sid → (generation, uplink session).  Generation disambiguates
    /// latest-wins replacement from a stale session's own teardown.
    uplinks: Mutex<HashMap<String, (u64, wt::Session)>>,
    generation: AtomicU64,
    tls: RwLock<Arc<rustls::ServerConfig>>,
    endpoint: wt::quinn::Endpoint,
    /// Sealed PEM currently in use, to detect rotation.
    current_cert: Mutex<String>,
}

pub async fn run(name: String, port: u16) -> Result<(), String> {
    let env = crate::config::load().await?;
    run_with(env, name, port).await
}

pub async fn run_with(env: crate::config::Env, name: String, port: u16) -> Result<(), String> {
    // A worker only serves (and only registers) once it holds a certificate.
    let sealed = wait_for_cert(&env.store).await?;
    let pem = crate::certs::open_sealed(&env.seal_key, &sealed)?;
    let bundle = crate::certs::parse_bundle(&pem)?;

    let udp_addr = udp_bind_addr(port).await;
    let endpoint = wt::quinn::Endpoint::server(crate::certs::quic_config(&bundle)?, udp_addr)
        .map_err(|e| format!("QUIC bind {udp_addr}: {e}"))?;
    let tcp = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| format!("TCP bind {port}: {e}"))?;
    eprintln!("[worker {name}] listening on {udp_addr} (QUIC) and 0.0.0.0:{port} (TLS/WS)");

    let state = Arc::new(WorkerState {
        name,
        port,
        store: env.store,
        keys: env.keys,
        seal_key: env.seal_key,
        uplinks: Mutex::new(HashMap::new()),
        generation: AtomicU64::new(0),
        tls: RwLock::new(crate::certs::tls_config(&bundle)?),
        endpoint: endpoint.clone(),
        current_cert: Mutex::new(sealed),
    });

    tokio::spawn(heartbeat_loop(state.clone()));
    tokio::spawn(ws_accept_loop(tcp, state.clone()));
    tokio::spawn(shutdown_on_signal(state.clone()));

    let mut server = wt::Server::new(endpoint);
    while let Some(request) = server.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_wt_request(request, state).await {
                eprintln!("[worker] webtransport session: {e}");
            }
        });
    }
    Ok(())
}

/// On Fly, UDP services must bind the `fly-global-services` address.
async fn udp_bind_addr(port: u16) -> std::net::SocketAddr {
    if std::env::var("FLY_APP_NAME").is_ok() {
        if let Ok(mut addrs) = tokio::net::lookup_host(("fly-global-services", port)).await
            && let Some(addr) = addrs.next()
        {
            return addr;
        }
        eprintln!("[worker] fly-global-services did not resolve; binding 0.0.0.0");
    }
    ([0, 0, 0, 0], port).into()
}

async fn wait_for_cert(store: &Store) -> Result<String, String> {
    let mut warned = false;
    loop {
        if let Some(sealed) = store.get(crate::certs::CERT_KEY).await? {
            return Ok(sealed);
        }
        if !warned {
            eprintln!("[worker] waiting for the control plane to provision a certificate…");
            warned = true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// Heartbeat: registration, session-binding refresh, certificate rotation.
// ---------------------------------------------------------------------------

async fn heartbeat_loop(state: Arc<WorkerState>) {
    loop {
        let (count, sids): (usize, Vec<String>) = {
            let map = state.uplinks.lock().unwrap();
            (map.len(), map.keys().cloned().collect())
        };
        let value = serde_json::json!({ "port": state.port, "uplinks": count }).to_string();
        if let Err(e) = state
            .store
            .set_ex(&format!("worker:{}", state.name), &value, KEY_TTL_SECS)
            .await
        {
            // Existing sessions are worker-local: tolerate store outages and
            // keep serving; only registration and new attaches degrade.
            eprintln!("[worker {}] heartbeat failed: {e}", state.name);
        }
        for sid in sids {
            let _ = state
                .store
                .refresh_if_eq(&format!("session:{sid}"), &state.name, KEY_TTL_SECS)
                .await;
        }
        if let Err(e) = maybe_rotate_cert(&state).await {
            eprintln!("[worker {}] certificate reload failed: {e}", state.name);
        }
        tokio::time::sleep(HEARTBEAT).await;
    }
}

async fn maybe_rotate_cert(state: &Arc<WorkerState>) -> Result<(), String> {
    let Some(sealed) = state.store.get(crate::certs::CERT_KEY).await? else {
        return Ok(());
    };
    if *state.current_cert.lock().unwrap() == sealed {
        return Ok(());
    }
    let pem = crate::certs::open_sealed(&state.seal_key, &sealed)?;
    let bundle = crate::certs::parse_bundle(&pem)?;
    state
        .endpoint
        .set_server_config(Some(crate::certs::quic_config(&bundle)?));
    *state.tls.write().unwrap() = crate::certs::tls_config(&bundle)?;
    *state.current_cert.lock().unwrap() = sealed;
    eprintln!("[worker {}] certificate rotated", state.name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Shutdown (docs/upsidedown.md § Shutdown): explicit cleanup so uplinks and
// consumers fail over immediately instead of waiting out TTLs.
// ---------------------------------------------------------------------------

async fn shutdown_on_signal(state: Arc<WorkerState>) {
    wait_for_signal().await;
    eprintln!("[worker {}] shutting down", state.name);
    let _ = state.store.del(&format!("worker:{}", state.name)).await;
    let sessions: Vec<(String, wt::Session)> = state
        .uplinks
        .lock()
        .unwrap()
        .drain()
        .map(|(sid, (_, session))| (sid, session))
        .collect();
    for (sid, session) in sessions {
        let _ = state
            .store
            .del_if_eq(&format!("session:{sid}"), &state.name)
            .await;
        session.close(CLOSE_SHUTDOWN, b"worker shutting down");
    }
    std::process::exit(0);
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

// ---------------------------------------------------------------------------
// WebTransport: uplink registration and WT consumers.
// ---------------------------------------------------------------------------

async fn handle_wt_request(request: wt::Request, state: Arc<WorkerState>) -> Result<(), String> {
    let path = request.url.path().to_string();
    if let Some(token) = path.strip_prefix("/u/") {
        // Uplink registration: the URL is the credential (never logged).
        match jwt::verify(token, &state.keys, jwt::now_unix()) {
            Ok(claims) if claims.role == Role::Server => {
                let session = request
                    .ok()
                    .await
                    .map_err(|e| format!("uplink accept: {e}"))?;
                register_uplink(state, claims.sid, session).await;
                Ok(())
            }
            _ => {
                let _ = request.reject(axum::http::StatusCode::FORBIDDEN).await;
                Ok(())
            }
        }
    } else {
        let session = request
            .ok()
            .await
            .map_err(|e| format!("consumer accept: {e}"))?;
        handle_wt_consumer(session, state).await
    }
}

async fn register_uplink(state: Arc<WorkerState>, sid: String, session: wt::Session) {
    let generation = state.generation.fetch_add(1, Ordering::SeqCst);
    let old = state
        .uplinks
        .lock()
        .unwrap()
        .insert(sid.clone(), (generation, session.clone()));
    if let Some((_, old_session)) = old {
        // Latest-wins: a reconnecting uplink never fights its own zombie.
        old_session.close(CLOSE_SUPERSEDED, b"superseded by a new uplink");
    }
    let _ = state
        .store
        .set_ex(&format!("session:{sid}"), &state.name, KEY_TTL_SECS)
        .await;
    // Wake any /attach blocked on this session (docs/upsidedown.md § attach).
    state.store.publish_session(&sid).await;
    eprintln!("[worker {}] uplink connected for sid {sid}", state.name);

    // Tear down the binding when this session dies — but only if it is
    // still the current one (compare generation locally, value remotely).
    let err = session.closed().await;
    let still_current = {
        let mut map = state.uplinks.lock().unwrap();
        if map.get(&sid).map(|(g, _)| *g) == Some(generation) {
            map.remove(&sid);
            true
        } else {
            false
        }
    };
    if still_current {
        let _ = state
            .store
            .del_if_eq(&format!("session:{sid}"), &state.name)
            .await;
        eprintln!("[worker {}] uplink for sid {sid} gone: {err}", state.name);
    }
}

fn find_uplink(state: &WorkerState, sid: &str) -> Option<wt::Session> {
    state
        .uplinks
        .lock()
        .unwrap()
        .get(sid)
        .map(|(_, s)| s.clone())
}

async fn handle_wt_consumer(session: wt::Session, state: Arc<WorkerState>) -> Result<(), String> {
    let (mut send, mut recv) = tokio::time::timeout(AUTH_TIMEOUT, session.accept_bi())
        .await
        .map_err(|_| "auth timeout")?
        .map_err(|e| format!("accept_bi: {e}"))?;

    // [len:2 LE][token] preamble, answered 0x01/0x00 (transports.md).
    let token = tokio::time::timeout(AUTH_TIMEOUT, async {
        let mut len_buf = [0u8; 2];
        recv.read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("auth read: {e}"))?;
        let len = u16::from_le_bytes(len_buf) as usize;
        if len > 8192 {
            return Err("token too long".to_string());
        }
        let mut buf = vec![0u8; len];
        recv.read_exact(&mut buf)
            .await
            .map_err(|e| format!("auth read: {e}"))?;
        String::from_utf8(buf).map_err(|_| "token not UTF-8".to_string())
    })
    .await
    .map_err(|_| "auth timeout")??;

    let claims = match jwt::verify(&token, &state.keys, jwt::now_unix()) {
        Ok(c) if c.role == Role::Client => c,
        _ => {
            let _ = send.write_all(&[0]).await;
            session.close(CLOSE_AUTH, b"authentication failed");
            return Ok(());
        }
    };
    let Some(uplink) = find_uplink(&state, &claims.sid) else {
        session.close(CLOSE_NO_UPLINK, b"no uplink connected");
        return Ok(());
    };
    let (mut up_send, mut up_recv) = uplink
        .open_bi()
        .await
        .map_err(|e| format!("uplink open_bi: {e}"))?;
    send.write_all(&[1])
        .await
        .map_err(|e| format!("auth ack: {e}"))?;

    // Byte-for-byte splice: both sides carry the blit 4-byte LE framing.
    let down = async {
        if tokio::io::copy(&mut up_recv, &mut send).await.is_ok() {
            let _ = send.finish();
        }
    };
    let up = async {
        if tokio::io::copy(&mut recv, &mut up_send).await.is_ok() {
            let _ = up_send.finish();
        }
    };
    tokio::join!(down, up);
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket consumers: gateway-style frame bridging (one binary message per
// blit frame ⇄ the stream's 4-byte LE length prefixes).
// ---------------------------------------------------------------------------

async fn ws_accept_loop(listener: tokio::net::TcpListener, state: Arc<WorkerState>) {
    loop {
        let Ok((stream, _peer)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let acceptor = tokio_rustls::TlsAcceptor::from(state.tls.read().unwrap().clone());
            let Ok(tls_stream) = acceptor.accept(stream).await else {
                return;
            };
            let Ok(ws) = tokio_tungstenite::accept_async(tls_stream).await else {
                return;
            };
            if let Err(e) = handle_ws_consumer(ws, state).await {
                eprintln!("[worker] websocket consumer: {e}");
            }
        });
    }
}

async fn handle_ws_consumer<S>(
    mut ws: tokio_tungstenite::WebSocketStream<S>,
    state: Arc<WorkerState>,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // First text frame is the token, answered "ok"/"auth" (transports.md).
    let token = match tokio::time::timeout(AUTH_TIMEOUT, ws.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => t.to_string(),
        Ok(_) => return Err("expected token as first text frame".into()),
        Err(_) => return Err("auth timeout".into()),
    };
    let claims = match jwt::verify(&token, &state.keys, jwt::now_unix()) {
        Ok(c) if c.role == Role::Client => c,
        _ => {
            let _ = ws.send(Message::Text("auth".into())).await;
            let _ = ws.close(None).await;
            return Ok(());
        }
    };
    let Some(uplink) = find_uplink(&state, &claims.sid) else {
        // Distinct from auth failure: the session's uplink is not here.
        let _ = ws
            .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: 4404.into(),
                reason: "no uplink connected".into(),
            }))
            .await;
        return Ok(());
    };
    let (mut up_send, mut up_recv) = uplink
        .open_bi()
        .await
        .map_err(|e| format!("uplink open_bi: {e}"))?;
    ws.send(Message::Text("ok".into()))
        .await
        .map_err(|e| format!("auth ack: {e}"))?;

    let (mut ws_tx, mut ws_rx) = ws.split();
    let ws_to_uplink = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Binary(data) => {
                    let mut framed = Vec::with_capacity(4 + data.len());
                    framed.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    framed.extend_from_slice(&data);
                    if up_send.write_all(&framed).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => continue,
            }
        }
        let _ = up_send.finish();
    };
    let uplink_to_ws = async {
        loop {
            let mut len_buf = [0u8; 4];
            if up_recv.read_exact(&mut len_buf).await.is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len > 64 * 1024 * 1024 {
                break;
            }
            let mut payload = vec![0u8; len];
            if up_recv.read_exact(&mut payload).await.is_err() {
                break;
            }
            if ws_tx.send(Message::Binary(payload.into())).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    };
    tokio::join!(ws_to_uplink, uplink_to_ws);
    Ok(())
}
