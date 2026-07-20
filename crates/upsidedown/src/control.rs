//! Control plane (docs/upsidedown.md § Control plane, § Certificates).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};

use crate::jwt::{self, Role};
use crate::store::Store;

/// Workers within this many active uplinks of the minimum are allocation
/// candidates — heartbeat counts are up to 5s stale, so a strict minimum
/// would herd a burst of arrivals onto one worker.
const POOL_MARGIN: u64 = 4;

/// How long `/attach` blocks waiting for the session's uplink to connect.
const ATTACH_WAIT: Duration = Duration::from_secs(30);

struct CpState {
    store: Store,
    keys: Vec<ed25519_dalek::VerifyingKey>,
    host: String,
    /// Dev mode only (self-signed certificate): `#sha256=` pin appended to
    /// relay URLs so uplinks can verify without system roots.
    dev_pin: Option<String>,
}

#[derive(Debug, PartialEq)]
struct WorkerInfo {
    name: String,
    port: u16,
    uplinks: u64,
}

pub async fn run() -> Result<(), String> {
    let env = crate::config::load().await?;
    run_with(env).await
}

pub async fn run_with(env: crate::config::Env) -> Result<(), String> {
    let listen: SocketAddr = std::env::var("UPSIDEDOWN_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8443".into())
        .parse()
        .map_err(|e| format!("UPSIDEDOWN_LISTEN: {e}"))?;
    let acme_domain = std::env::var("UPSIDEDOWN_ACME_DOMAIN").ok();

    let dev_pin = match acme_domain {
        Some(_) => None,
        None => Some(ensure_dev_cert(&env.store, &env.seal_key, &env.host).await?),
    };

    let state = Arc::new(CpState {
        store: env.store.clone(),
        keys: env.keys,
        host: env.host.clone(),
        dev_pin,
    });
    let app = Router::new()
        .route("/pool", get(pool))
        .route("/attach", get(attach))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    match acme_domain {
        Some(domain) => serve_acme(app, listen, domain, env.store, env.seal_key).await,
        // UPSIDEDOWN_HTTP=1: plain HTTP for local development only.
        None if std::env::var("UPSIDEDOWN_HTTP").ok().as_deref() == Some("1") => {
            eprintln!("[control] listening on {listen} (PLAIN HTTP — development only)");
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .map_err(|e| format!("bind {listen}: {e}"))?;
            axum::serve(listener, app)
                .await
                .map_err(|e| format!("serve: {e}"))
        }
        None => serve_dev(app, listen, env.store, env.seal_key).await,
    }
}

/// ACME via TLS-ALPN-01; account and certificate sealed into the store so
/// workers (and control-plane restarts) share one issuance.
async fn serve_acme(
    app: Router,
    listen: SocketAddr,
    domain: String,
    store: Store,
    seal_key: [u8; 32],
) -> Result<(), String> {
    use futures_util::StreamExt;

    let staging = std::env::var("UPSIDEDOWN_ACME_STAGING").ok().as_deref() == Some("1");
    let mut config = rustls_acme::AcmeConfig::new([domain.clone()])
        .cache(crate::certs::SealedCache { store, seal_key })
        .directory_lets_encrypt(!staging);
    if let Ok(contact) = std::env::var("UPSIDEDOWN_ACME_CONTACT") {
        config = config.contact_push(format!("mailto:{contact}"));
    }
    let mut acme = config.state();
    let acceptor = acme.axum_acceptor(acme.default_rustls_config());
    tokio::spawn(async move {
        loop {
            match acme.next().await {
                Some(Ok(event)) => eprintln!("[control] acme: {event:?}"),
                Some(Err(e)) => eprintln!("[control] acme error: {e}"),
                None => break,
            }
        }
    });

    eprintln!("[control] listening on {listen} (ACME: {domain})");
    axum_server::bind(listen)
        .acceptor(acceptor)
        .serve(app.into_make_service())
        .await
        .map_err(|e| format!("serve: {e}"))
}

/// Development: self-signed certificate from the store.
async fn serve_dev(
    app: Router,
    listen: SocketAddr,
    store: Store,
    seal_key: [u8; 32],
) -> Result<(), String> {
    let pem = crate::certs::load_bundle(&store, &seal_key)
        .await?
        .ok_or("dev certificate missing")?;
    let bundle = crate::certs::parse_bundle(&pem)?;
    let tls = crate::certs::tls_config(&bundle)?;
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls);
    eprintln!("[control] listening on {listen} (dev self-signed certificate)");
    axum_server::bind_rustls(listen, rustls_config)
        .serve(app.into_make_service())
        .await
        .map_err(|e| format!("serve: {e}"))
}

/// Write a self-signed bundle to the store if absent; return its pin.
async fn ensure_dev_cert(store: &Store, seal_key: &[u8; 32], host: &str) -> Result<String, String> {
    let pem = match crate::certs::load_bundle(store, seal_key).await? {
        Some(pem) => pem,
        None => {
            eprintln!("[control] no ACME domain configured; using a dev self-signed certificate");
            let pem = crate::certs::dev_bundle_pem(host)?;
            let sealed = crate::certs::seal(seal_key, pem.as_bytes())?;
            store.set(crate::certs::CERT_KEY, &sealed).await?;
            pem.into_bytes()
        }
    };
    let bundle = crate::certs::parse_bundle(&pem)?;
    Ok(jwt::b64url_encode(&crate::certs::leaf_hash(&bundle)))
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Extract and verify the bearer token, requiring `role`.
/// Returns the claims and the raw token (it is re-embedded in relay URLs).
#[allow(clippy::result_large_err)]
fn authorize(
    state: &CpState,
    headers: &HeaderMap,
    role: Role,
) -> Result<(jwt::Claims, String), Response> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token").into_response())?;
    let claims = jwt::verify(token, &state.keys, jwt::now_unix())
        .map_err(|e| (StatusCode::UNAUTHORIZED, e).into_response())?;
    if claims.role != role {
        return Err((StatusCode::FORBIDDEN, "wrong token role").into_response());
    }
    Ok((claims, token.to_string()))
}

fn parse_worker(key: &str, value: &str) -> Option<WorkerInfo> {
    let name = key.strip_prefix("worker:")?.to_string();
    let v: serde_json::Value = serde_json::from_str(value).ok()?;
    Some(WorkerInfo {
        name,
        port: u16::try_from(v.get("port")?.as_u64()?).ok()?,
        uplinks: v.get("uplinks")?.as_u64()?,
    })
}

/// Uniform pick among workers within `POOL_MARGIN` of the minimum load.
fn choose_worker(workers: &[WorkerInfo]) -> Option<&WorkerInfo> {
    use rand::RngExt as _;
    let min = workers.iter().map(|w| w.uplinks).min()?;
    let candidates: Vec<&WorkerInfo> = workers
        .iter()
        .filter(|w| w.uplinks <= min + POOL_MARGIN)
        .collect();
    let i = rand::rng().random_range(0..candidates.len());
    Some(candidates[i])
}

async fn pool(State(state): State<Arc<CpState>>, headers: HeaderMap) -> Response {
    let (_claims, token) = match authorize(&state, &headers, Role::Server) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let entries = match state.store.scan_prefix("worker:").await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[control] /pool store error: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "5")],
                "store unavailable",
            )
                .into_response();
        }
    };
    let workers: Vec<WorkerInfo> = entries
        .iter()
        .filter_map(|(k, v)| parse_worker(k, v))
        .collect();
    let Some(worker) = choose_worker(&workers) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "5")],
            "no registered workers",
        )
            .into_response();
    };

    let pin = state
        .dev_pin
        .as_deref()
        .map(|p| format!("#sha256={p}"))
        .unwrap_or_default();
    let relay = format!("https://{}:{}/u/{token}{pin}", state.host, worker.port);
    Json(serde_json::json!({ "relays": [relay] })).into_response()
}

/// Resolve `sid`'s worker to a consumer response, if its uplink is bound
/// and the worker is still registered.
async fn resolve_attach(state: &CpState, sid: &str) -> Option<Response> {
    let worker_name = state.store.get(&format!("session:{sid}")).await.ok()??;
    let key = format!("worker:{worker_name}");
    let w = state
        .store
        .get(&key)
        .await
        .ok()
        .flatten()
        .and_then(|v| parse_worker(&key, &v))?;
    Some(
        Json(serde_json::json!({
            "ws": format!("wss://{}:{}/", state.host, w.port),
            "wt": format!("https://{}:{}/", state.host, w.port),
        }))
        .into_response(),
    )
}

async fn attach(State(state): State<Arc<CpState>>, headers: HeaderMap) -> Response {
    let (claims, _token) = match authorize(&state, &headers, Role::Client) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let sid = claims.sid;

    // Fast path: uplink already connected.
    if let Some(resp) = resolve_attach(&state, &sid).await {
        return resp;
    }

    // Event-driven wait: subscribe *before* the re-check so a registration
    // in the gap can't be missed, then block until the worker publishes this
    // session (docs/upsidedown.md § attach).  The subscription drops with the
    // request if the client disconnects.
    let mut events = match state.store.subscribe_sessions().await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[control] /attach subscribe failed: {e}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "2")],
                "store unavailable",
            )
                .into_response();
        }
    };

    let closed = || {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "2")],
            "event stream closed",
        )
            .into_response()
    };
    let wait = tokio::time::sleep(ATTACH_WAIT);
    tokio::pin!(wait);
    loop {
        if let Some(resp) = resolve_attach(&state, &sid).await {
            return resp;
        }
        // Wait for an event naming our session (re-check then) or the timeout,
        // skipping unrelated sessions' events without touching the store.
        let matched = loop {
            tokio::select! {
                _ = &mut wait => break false,
                got = events.next_sid() => match got {
                    Some(s) if s == sid => break true,
                    Some(_) => continue,
                    None => return closed(),
                },
            }
        };
        if !matched {
            return (
                StatusCode::NOT_FOUND,
                [("retry-after", "5")],
                "session has no connected uplink",
            )
                .into_response();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str, port: u16, uplinks: u64) -> WorkerInfo {
        WorkerInfo {
            name: name.into(),
            port,
            uplinks,
        }
    }

    #[test]
    fn chooses_within_margin_only() {
        let workers = vec![w("w1", 4441, 0), w("w2", 4442, 3), w("w3", 4443, 100)];
        for _ in 0..50 {
            let picked = choose_worker(&workers).unwrap();
            assert_ne!(picked.name, "w3", "overloaded worker must not be picked");
        }
    }

    #[test]
    fn empty_pool_is_none() {
        assert!(choose_worker(&[]).is_none());
    }

    #[test]
    fn parses_heartbeat_json() {
        let info = parse_worker("worker:w2", r#"{"port":4442,"uplinks":17}"#).unwrap();
        assert_eq!(info, w("w2", 4442, 17));
        assert!(parse_worker("worker:w2", "junk").is_none());
        assert!(parse_worker("session:x", r#"{"port":1,"uplinks":0}"#).is_none());
    }
}
