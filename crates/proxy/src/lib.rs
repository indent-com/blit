/// blit-proxy library — all proxy logic, usable in-process or as a binary.
///
/// Call [`proxy_socket_path`] to find the socket, then [`run`] to start the
/// proxy on the current thread (blocking, runs its own tokio runtime).
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, RwLock};

static VERBOSE: AtomicBool = AtomicBool::new(false);

macro_rules! log {
    ($($arg:tt)*) => {
        if VERBOSE.load(Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}

type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

// ---------------------------------------------------------------------------
// Proxy socket path (single stable path for the whole process)
// ---------------------------------------------------------------------------

pub fn proxy_socket_path() -> String {
    if let Ok(p) = std::env::var("BLIT_PROXY_SOCK") {
        return p;
    }
    #[cfg(unix)]
    {
        let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
        format!("{dir}/blit-proxy.sock")
    }
    #[cfg(windows)]
    {
        r"\\.\pipe\blit-proxy".into()
    }
}

// ---------------------------------------------------------------------------
// Auto-start helpers (shared by blit-cli and blit-gateway)
// ---------------------------------------------------------------------------

/// Returns true if a proxy is already listening at `path`.
pub async fn proxy_alive(path: &str) -> bool {
    #[cfg(unix)]
    {
        std::path::Path::new(path).exists() && tokio::net::UnixStream::connect(path).await.is_ok()
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        ClientOptions::new().open(path).is_ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        false
    }
}

/// Ensure a blit-proxy daemon is running, spawning one if necessary.
///
/// `proxy_bin` is the path to the binary that accepts a `proxy-daemon`
/// subcommand (typically `std::env::current_exe()`).  When the binary is
/// the standalone `blit-proxy` it should be invoked without arguments;
/// when it is the `blit` CLI it needs the `proxy-daemon` subcommand.
///
/// Returns the socket path on success.
pub async fn ensure_proxy(
    proxy_bin: &std::path::Path,
    use_subcommand: bool,
) -> Result<String, String> {
    let sock = proxy_socket_path();

    if proxy_alive(&sock).await {
        return Ok(sock);
    }

    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(proxy_bin);
        if use_subcommand {
            cmd.arg("proxy-daemon");
        }
        // SAFETY: pre_exec runs in the child between fork and exec.
        // setsid() is async-signal-safe per POSIX.
        unsafe {
            cmd.env("BLIT_PROXY_IDLE", "300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .map_err(|e| format!("blit-proxy: spawn failed: {e}"))?;
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = std::process::Command::new(proxy_bin);
        if use_subcommand {
            cmd.arg("proxy-daemon");
        }
        cmd.env("BLIT_PROXY_IDLE", "300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("blit-proxy: spawn failed: {e}"))?;
    }

    #[cfg(not(any(unix, windows)))]
    return Err("blit-proxy auto-start is not supported on this platform".into());

    // Wait up to 5 s for the socket/pipe to appear.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if proxy_alive(&sock).await {
            return Ok(sock);
        }
    }
    Err(format!("blit-proxy did not become ready at {sock} in time"))
}

// ---------------------------------------------------------------------------
// Upstream connection
// ---------------------------------------------------------------------------

struct UpstreamConn {
    reader: BoxRead,
    writer: BoxWrite,
}

// ---------------------------------------------------------------------------
// Share session pool — pools WebRTC Sessions, opens/closes channels per req
// ---------------------------------------------------------------------------

struct SharePool {
    idle: Mutex<VecDeque<blit_webrtc_forwarder::client::Session>>,
    passphrase: String,
    hub: String,
    pool_size: usize,
    active: AtomicUsize,
    last_activity: AtomicI64,
}

impl SharePool {
    fn new(passphrase: String, hub: String, pool_size: usize) -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(VecDeque::new()),
            passphrase,
            hub,
            pool_size,
            active: AtomicUsize::new(0),
            last_activity: AtomicI64::new(now_secs()),
        })
    }

    async fn acquire(
        &self,
    ) -> Result<
        (
            blit_webrtc_forwarder::client::Session,
            blit_webrtc_forwarder::client::ChannelHandle,
            UpstreamConn,
        ),
        String,
    > {
        loop {
            let session = {
                let mut idle = self.idle.lock().await;
                idle.pop_front()
            };
            match session {
                Some(s) => match s.open_channel().await {
                    Ok((handle, stream)) => {
                        let (r, w) = tokio::io::split(stream);
                        return Ok((
                            s,
                            handle,
                            UpstreamConn {
                                reader: Box::new(r),
                                writer: Box::new(w),
                            },
                        ));
                    }
                    Err(e) => {
                        log!("blit-proxy: share: idle session stale ({e}), discarding");
                        continue;
                    }
                },
                None => break,
            }
        }
        // No idle session — establish a new one.
        let (session, handle, stream) =
            blit_webrtc_forwarder::client::Session::establish(&self.passphrase, &self.hub)
                .await
                .map_err(|e| format!("{e}"))?;
        let (r, w) = tokio::io::split(stream);
        Ok((
            session,
            handle,
            UpstreamConn {
                reader: Box::new(r),
                writer: Box::new(w),
            },
        ))
    }

    async fn release(&self, session: blit_webrtc_forwarder::client::Session) {
        let mut idle = self.idle.lock().await;
        if idle.len() < self.pool_size {
            idle.push_back(session);
        }
    }

    fn client_connected(&self) {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.last_activity.store(i64::MAX, Ordering::Relaxed);
    }

    fn client_disconnected(&self) {
        let prev = self.active.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            self.last_activity.store(now_secs(), Ordering::Relaxed);
        }
    }

    async fn refill_loop(self: Arc<Self>) {
        loop {
            let need = {
                let idle = self.idle.lock().await;
                self.pool_size.saturating_sub(idle.len())
            };
            for _ in 0..need {
                match blit_webrtc_forwarder::client::Session::establish(&self.passphrase, &self.hub)
                    .await
                {
                    Ok((session, handle, _stream)) => {
                        // Close the initial channel; keep only the warm session.
                        session.close_channel(handle);
                        let mut idle = self.idle.lock().await;
                        if idle.len() < self.pool_size {
                            idle.push_back(session);
                        }
                    }
                    Err(e) => {
                        log!("blit-proxy: share: pre-warm failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

struct ShareRegistry {
    pools: RwLock<HashMap<String, Arc<SharePool>>>,
    pool_size: usize,
}

impl ShareRegistry {
    fn new(pool_size: usize) -> Arc<Self> {
        Arc::new(Self {
            pools: RwLock::new(HashMap::new()),
            pool_size,
        })
    }

    async fn get_or_create(self: &Arc<Self>, uri: &str) -> Arc<SharePool> {
        {
            let pools = self.pools.read().await;
            if let Some(p) = pools.get(uri) {
                return p.clone();
            }
        }
        let mut pools = self.pools.write().await;
        if let Some(p) = pools.get(uri) {
            return p.clone();
        }
        let (passphrase, hub) = parse_share_uri(uri.strip_prefix("share:").unwrap_or(uri));
        let pool = SharePool::new(passphrase, hub, self.pool_size);
        pools.insert(uri.to_string(), pool.clone());
        drop(pools);

        let pool_seed = pool.clone();
        tokio::spawn(async move {
            pool_seed.refill_loop().await;
        });

        pool
    }

    async fn latest_activity(&self) -> i64 {
        let pools = self.pools.read().await;
        if pools.is_empty() {
            return now_secs();
        }
        pools
            .values()
            .map(|p| p.last_activity.load(Ordering::Relaxed))
            .max()
            .unwrap_or_else(now_secs)
    }
}

// ---------------------------------------------------------------------------
// Per-target pool
// ---------------------------------------------------------------------------

struct Pool {
    idle: Mutex<VecDeque<UpstreamConn>>,
    /// Number of currently active (proxied) downstream clients.
    active: AtomicUsize,
    /// Unix seconds of last client connect or disconnect.
    /// `i64::MAX` while any client is active.
    last_activity: AtomicI64,
    pool_size: usize,
    upstream_uri: String,
}

impl Pool {
    fn new(upstream_uri: String, pool_size: usize) -> Arc<Self> {
        Arc::new(Self {
            idle: Mutex::new(VecDeque::new()),
            active: AtomicUsize::new(0),
            last_activity: AtomicI64::new(now_secs()),
            pool_size,
            upstream_uri,
        })
    }

    async fn acquire(&self) -> Result<UpstreamConn, String> {
        {
            let mut idle = self.idle.lock().await;
            if let Some(conn) = idle.pop_front() {
                return Ok(conn);
            }
        }
        self.connect_one().await
    }

    async fn connect_one(&self) -> Result<UpstreamConn, String> {
        connect_upstream(&self.upstream_uri).await
    }

    fn client_connected(&self) {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.last_activity.store(i64::MAX, Ordering::Relaxed);
    }

    fn client_disconnected(&self) {
        let prev = self.active.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            self.last_activity.store(now_secs(), Ordering::Relaxed);
        }
    }

    /// Background task: keep idle slots full.
    async fn refill_loop(self: Arc<Self>) {
        loop {
            let need = {
                let idle = self.idle.lock().await;
                self.pool_size.saturating_sub(idle.len())
            };
            for _ in 0..need {
                match self.connect_one().await {
                    Ok(conn) => {
                        self.idle.lock().await.push_back(conn);
                    }
                    Err(e) => {
                        log!(
                            "blit-proxy: [{uri}] upstream connect failed: {e}",
                            uri = self.upstream_uri
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// Pool registry: one pool per distinct upstream URI
// ---------------------------------------------------------------------------

struct Registry {
    pools: RwLock<HashMap<String, Arc<Pool>>>,
    pool_size: usize,
}

impl Registry {
    fn new(pool_size: usize) -> Arc<Self> {
        Arc::new(Self {
            pools: RwLock::new(HashMap::new()),
            pool_size,
        })
    }

    /// Get or create the pool for `uri`, spawning its refill task if new.
    async fn get_or_create(self: &Arc<Self>, uri: &str) -> Arc<Pool> {
        // Fast path: pool already exists.
        {
            let pools = self.pools.read().await;
            if let Some(p) = pools.get(uri) {
                return p.clone();
            }
        }
        // Slow path: create and seed.
        let mut pools = self.pools.write().await;
        // Re-check under write lock.
        if let Some(p) = pools.get(uri) {
            return p.clone();
        }
        let pool = Pool::new(uri.to_string(), self.pool_size);
        pools.insert(uri.to_string(), pool.clone());
        drop(pools);

        // Seed eagerly in a background task.
        let pool_seed = pool.clone();
        tokio::spawn(async move {
            for _ in 0..pool_seed.pool_size {
                match pool_seed.connect_one().await {
                    Ok(conn) => pool_seed.idle.lock().await.push_back(conn),
                    Err(e) => {
                        log!(
                            "blit-proxy: [{uri}] initial connect: {e}",
                            uri = pool_seed.upstream_uri
                        );
                        break;
                    }
                }
            }
            pool_seed.refill_loop().await;
        });

        pool
    }

    /// Returns the most-recent `last_activity` across all pools.
    /// Returns `i64::MAX` if any pool has an active client.
    async fn latest_activity(&self) -> i64 {
        let pools = self.pools.read().await;
        if pools.is_empty() {
            return now_secs();
        }
        pools
            .values()
            .map(|p| p.last_activity.load(Ordering::Relaxed))
            .max()
            .unwrap_or_else(now_secs)
    }
}

// ---------------------------------------------------------------------------
// Upstream transport implementations
// ---------------------------------------------------------------------------

async fn connect_upstream(uri: &str) -> Result<UpstreamConn, String> {
    if let Some(rest) = uri.strip_prefix("share:") {
        return connect_share(rest).await;
    }

    if let Some(rest) = uri.strip_prefix("ssh:") {
        return connect_ssh(rest).await;
    }

    // Extract query parameters from URIs that support them.
    let (base_uri, passphrase, cert_hash) = extract_uri_params(uri);

    if let Some(path) = base_uri.strip_prefix("socket:") {
        return connect_socket(path).await;
    }
    if let Some(addr) = base_uri.strip_prefix("tcp:") {
        return connect_tcp(addr).await;
    }
    if base_uri.starts_with("ws://") || base_uri.starts_with("wss://") {
        return connect_ws(&base_uri, passphrase.as_deref()).await;
    }
    if let Some(rest) = base_uri.strip_prefix("wt://") {
        let cert_bytes = cert_hash.as_deref().and_then(parse_hex);
        return connect_wt(rest, passphrase.as_deref(), &cert_bytes).await;
    }
    Err(format!(
        "unknown upstream URI scheme in '{uri}' \
         (expected socket:, tcp:, ws://, wss://, wt://, share:, or ssh:)"
    ))
}

/// Split `share:` URI rest into (passphrase, hub_url).
///
/// Accepted forms:
///   `myphrase`                       — use default hub
///   `myphrase?hub=wss://custom.hub`  — use specific hub
fn parse_share_uri(rest: &str) -> (String, String) {
    let (passphrase_raw, hub) = if let Some(q_pos) = rest.find('?') {
        let phrase = &rest[..q_pos];
        let query = &rest[q_pos + 1..];
        let hub = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("hub=").map(percent_decode))
            .unwrap_or_else(|| blit_webrtc_forwarder::DEFAULT_HUB_URL.to_string());
        (phrase.to_string(), hub)
    } else {
        (
            rest.to_string(),
            blit_webrtc_forwarder::DEFAULT_HUB_URL.to_string(),
        )
    };
    let passphrase = percent_decode(&passphrase_raw);
    let hub = blit_webrtc_forwarder::normalize_hub(&hub);
    (passphrase, hub)
}

async fn connect_share(rest: &str) -> Result<UpstreamConn, String> {
    let (passphrase, hub) = parse_share_uri(rest);
    let stream = blit_webrtc_forwarder::client::connect(&passphrase, &hub)
        .await
        .map_err(|e| format!("share:{rest}: {e}"))?;
    let (r, w) = tokio::io::split(stream);
    Ok(UpstreamConn {
        reader: Box::new(r),
        writer: Box::new(w),
    })
}

// ---------------------------------------------------------------------------
// SSH via embedded client (cross-platform)
// ---------------------------------------------------------------------------

/// Shared SSH connection pool for the proxy.  Connections are multiplexed
/// over a single TCP+SSH session per host, matching the gateway's behaviour.
fn ssh_pool() -> &'static blit_ssh::SshPool {
    static POOL: std::sync::OnceLock<blit_ssh::SshPool> = std::sync::OnceLock::new();
    POOL.get_or_init(blit_ssh::SshPool::new)
}

/// Connect to a remote blit-server via the embedded SSH client.
///
/// Uses `direct-streamlocal` channel forwarding (no external `ssh`, `nc`, or
/// `socat` required).  The connection is multiplexed and pooled so subsequent
/// calls to the same host reuse the TCP+SSH session.
async fn connect_ssh(rest: &str) -> Result<UpstreamConn, String> {
    if rest.is_empty() {
        return Err("ssh: destination requires a host".into());
    }
    let (user, host, socket) = blit_ssh::parse_ssh_uri(rest);
    let stream = ssh_pool()
        .connect(&host, user.as_deref(), socket.as_deref())
        .await
        .map_err(|e| format!("ssh:{rest}: {e}"))?;
    let (r, w) = tokio::io::split(stream);
    Ok(UpstreamConn {
        reader: Box::new(r),
        writer: Box::new(w),
    })
}

/// Split a URI into (base, passphrase, certHash) by parsing query params.
/// Only applies to ws://, wss://, wt:// — socket: and tcp: are returned as-is.
fn extract_uri_params(uri: &str) -> (String, Option<String>, Option<String>) {
    if !uri.starts_with("ws://") && !uri.starts_with("wss://") && !uri.starts_with("wt://") {
        return (uri.to_string(), None, None);
    }
    // Find '?'.
    let (base, query) = match uri.find('?') {
        Some(pos) => (&uri[..pos], Some(&uri[pos + 1..])),
        None => (uri, None),
    };
    let mut passphrase = None;
    let mut cert_hash = None;
    if let Some(q) = query {
        for param in q.split('&') {
            if let Some(v) = param.strip_prefix("passphrase=") {
                passphrase = Some(percent_decode(v));
            } else if let Some(v) = param.strip_prefix("certHash=") {
                cert_hash = Some(v.to_string());
            }
        }
    }
    (base.to_string(), passphrase, cert_hash)
}

fn percent_decode(s: &str) -> String {
    // Minimal %XX decoder sufficient for passphrases.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next().unwrap_or('0');
            let h2 = chars.next().unwrap_or('0');
            if let Ok(b) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                out.push(b as char);
                continue;
            }
        }
        out.push(c);
    }
    out
}

async fn connect_socket(path: &str) -> Result<UpstreamConn, String> {
    #[cfg(unix)]
    {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|e| format!("socket:{path}: {e}"))?;
        let (r, w) = tokio::io::split(stream);
        Ok(UpstreamConn {
            reader: Box::new(r),
            writer: Box::new(w),
        })
    }
    #[cfg(not(unix))]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let pipe = ClientOptions::new()
            .open(path)
            .map_err(|e| format!("socket:{path}: {e}"))?;
        let (r, w) = tokio::io::split(pipe);
        Ok(UpstreamConn {
            reader: Box::new(r),
            writer: Box::new(w),
        })
    }
}

async fn connect_tcp(addr: &str) -> Result<UpstreamConn, String> {
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("tcp:{addr}: {e}"))?;
    let _ = stream.set_nodelay(true);
    let (r, w) = tokio::io::split(stream);
    Ok(UpstreamConn {
        reader: Box::new(r),
        writer: Box::new(w),
    })
}

async fn connect_ws(uri: &str, passphrase: Option<&str>) -> Result<UpstreamConn, String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws, _) = tokio_tungstenite::connect_async(uri)
        .await
        .map_err(|e| format!("{uri}: {e}"))?;

    let pass = passphrase.unwrap_or("");
    ws.send(Message::Text(pass.into()))
        .await
        .map_err(|e| format!("{uri}: auth send: {e}"))?;
    match ws.next().await {
        Some(Ok(Message::Text(t))) if t.trim() == "ok" => {}
        Some(Ok(Message::Text(t))) => {
            return Err(format!("{uri}: auth rejected: {}", t.trim()));
        }
        other => {
            return Err(format!("{uri}: unexpected auth response: {other:?}"));
        }
    }

    let (ws_write, ws_read) = ws.split();
    Ok(UpstreamConn {
        reader: Box::new(WsFrameReader {
            inner: ws_read,
            buf: bytes::Bytes::new(),
        }),
        writer: Box::new(WsFrameWriter { inner: ws_write }),
    })
}

async fn connect_wt(
    rest: &str,
    passphrase: Option<&str>,
    cert_hash: &Option<Vec<u8>>,
) -> Result<UpstreamConn, String> {
    use web_transport_quinn as wt;

    // Build the URL for the WT session (must use https: scheme).
    let (host, port) = parse_wt_host_port(rest)?;
    let url: url::Url = format!("https://{host}:{port}/")
        .parse()
        .map_err(|e| format!("wt: url: {e}"))?;

    // Build the client with appropriate certificate verification.
    let client: wt::Client = if let Some(hash) = cert_hash {
        wt::ClientBuilder::new()
            .with_server_certificate_hashes(vec![hash.clone()])
            .map_err(|e| format!("wt: client build: {e}"))?
    } else {
        wt::ClientBuilder::new()
            .with_system_roots()
            .map_err(|e| format!("wt: client build: {e}"))?
    };

    let session = client
        .connect(url)
        .await
        .map_err(|e| format!("wt: connect {host}:{port}: {e}"))?;

    let (mut send, mut recv) = session
        .open_bi()
        .await
        .map_err(|e| format!("wt: open_bi: {e}"))?;

    // Auth: 2-byte-LE passphrase length + passphrase bytes, then read 1-byte response.
    let pass = passphrase.unwrap_or("").as_bytes();
    let mut auth_buf = Vec::with_capacity(2 + pass.len());
    auth_buf.extend_from_slice(&(pass.len() as u16).to_le_bytes());
    auth_buf.extend_from_slice(pass);
    send.write_all(&auth_buf)
        .await
        .map_err(|e| format!("wt: auth send: {e}"))?;

    let mut resp = [0u8; 1];
    recv.read_exact(&mut resp)
        .await
        .map_err(|e| format!("wt: auth recv: {e}"))?;
    if resp[0] != 1 {
        return Err(format!(
            "wt: auth rejected (response byte {:#04x})",
            resp[0]
        ));
    }

    Ok(UpstreamConn {
        reader: Box::new(recv),
        writer: Box::new(send),
    })
}

fn parse_wt_host_port(rest: &str) -> Result<(String, u16), String> {
    let without_path = rest.split('/').next().unwrap_or(rest);
    if let Some(colon) = without_path.rfind(':') {
        let host = without_path[..colon].to_string();
        let port: u16 = without_path[colon + 1..]
            .parse()
            .map_err(|_| format!("wt: invalid port in '{rest}'"))?;
        Ok((host, port))
    } else {
        Ok((without_path.to_string(), 443))
    }
}

fn parse_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// WS ↔ raw blit-frame adapters
// ---------------------------------------------------------------------------

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

struct WsFrameReader {
    inner: WsStream,
    buf: bytes::Bytes,
}

impl AsyncRead for WsFrameReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        loop {
            if !self.buf.is_empty() {
                let n = out.remaining().min(self.buf.len());
                out.put_slice(&self.buf[..n]);
                self.buf = self.buf.slice(n..);
                return std::task::Poll::Ready(Ok(()));
            }
            match self.inner.poll_next_unpin(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(None) => return std::task::Poll::Ready(Ok(())),
                std::task::Poll::Ready(Some(Err(e))) => {
                    return std::task::Poll::Ready(Err(std::io::Error::other(e)));
                }
                std::task::Poll::Ready(Some(Ok(msg))) => {
                    let data = match msg {
                        Message::Binary(d) => d,
                        Message::Close(_) => return std::task::Poll::Ready(Ok(())),
                        _ => continue,
                    };
                    let len = data.len() as u32;
                    let mut framed = Vec::with_capacity(4 + data.len());
                    framed.extend_from_slice(&len.to_le_bytes());
                    framed.extend_from_slice(&data);
                    self.buf = bytes::Bytes::from(framed);
                }
            }
        }
    }
}

struct WsFrameWriter {
    inner: WsSink,
}

impl AsyncWrite for WsFrameWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if buf.len() < 4 {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ws frame writer: buffer too small for length prefix",
            )));
        }
        let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
        if buf.len() < 4 + len {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "ws frame writer: buffer ({}) too small for frame payload ({})",
                    buf.len(),
                    4 + len
                ),
            )));
        }
        let payload = buf[4..4 + len].to_vec();
        match self.inner.poll_ready_unpin(cx) {
            std::task::Poll::Pending => return std::task::Poll::Pending,
            std::task::Poll::Ready(Err(e)) => {
                return std::task::Poll::Ready(Err(std::io::Error::other(e)));
            }
            std::task::Poll::Ready(Ok(())) => {}
        }
        match self.inner.start_send_unpin(Message::Binary(payload.into())) {
            Err(e) => std::task::Poll::Ready(Err(std::io::Error::other(e))),
            Ok(()) => std::task::Poll::Ready(Ok(4 + len)),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner
            .poll_flush_unpin(cx)
            .map_err(std::io::Error::other)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        self.inner
            .poll_close_unpin(cx)
            .map_err(std::io::Error::other)
    }
}

// ---------------------------------------------------------------------------
// Downstream listener
// ---------------------------------------------------------------------------

/// Read one line from the downstream socket (up to 4 KiB).
async fn read_line<S>(stream: &mut S) -> Option<String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte).await {
            Ok(0) | Err(_) => return None,
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > 4096 {
                    return None;
                }
            }
        }
    }
    Some(
        String::from_utf8_lossy(&buf)
            .trim_end_matches('\r')
            .to_string(),
    )
}

#[cfg(unix)]
async fn run_listener(
    registry: Arc<Registry>,
    share_registry: Arc<ShareRegistry>,
    sock_path: &str,
) {
    let _ = std::fs::remove_file(sock_path);
    let listener = tokio::net::UnixListener::bind(sock_path).unwrap_or_else(|e| {
        eprintln!("blit-proxy: cannot bind to {sock_path}: {e}");
        std::process::exit(1);
    });
    log!("blit-proxy: listening on {sock_path}");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let registry = registry.clone();
                let share_registry = share_registry.clone();
                tokio::spawn(async move {
                    handle_downstream(registry, share_registry, stream).await;
                });
            }
            Err(e) => log!("blit-proxy: accept error: {e}"),
        }
    }
}

#[cfg(windows)]
async fn run_listener(
    registry: Arc<Registry>,
    share_registry: Arc<ShareRegistry>,
    pipe_path: &str,
    first: tokio::net::windows::named_pipe::NamedPipeServer,
) {
    use tokio::net::windows::named_pipe::ServerOptions;
    log!("blit-proxy: listening on {pipe_path}");
    // Use the pre-created first instance (created with first_pipe_instance(true)
    // by the caller) to avoid a race window where the pipe name is unowned.
    let mut next_server = Some(first);
    loop {
        // Prepare the next server instance before awaiting the current connection,
        // so there is always a waiting server end after handoff.
        let server = match next_server.take() {
            Some(s) => s,
            None => match ServerOptions::new()
                .first_pipe_instance(false)
                .create(pipe_path)
            {
                Ok(s) => s,
                Err(e) => {
                    log!("blit-proxy: create pipe instance: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            },
        };
        // Create the *next* instance before connecting, so the pipe name is
        // never unowned between connections.
        let upcoming = ServerOptions::new()
            .first_pipe_instance(false)
            .create(pipe_path);
        if let Err(e) = server.connect().await {
            log!("blit-proxy: pipe connect: {e}");
            // Put the upcoming server back for the next iteration.
            if let Ok(u) = upcoming {
                next_server = Some(u);
            }
            continue;
        }
        if let Ok(u) = upcoming {
            next_server = Some(u);
        }
        let registry = registry.clone();
        let share_registry = share_registry.clone();
        tokio::spawn(async move {
            handle_downstream(registry, share_registry, server).await;
        });
    }
}

async fn handle_downstream<S>(
    registry: Arc<Registry>,
    share_registry: Arc<ShareRegistry>,
    mut downstream: S,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Handshake: read "target <uri>\n" or "shutdown\n"
    let line = match read_line(&mut downstream).await {
        Some(l) => l,
        None => return,
    };
    if line == "shutdown" {
        let _ = downstream.write_all(b"ok\n").await;
        log!("blit-proxy: shutdown requested, exiting");
        std::process::exit(0);
    }
    let uri = match line.strip_prefix("target ") {
        Some(u) if !u.is_empty() => u.to_string(),
        _ => {
            let _ = downstream.write_all(b"error invalid handshake\n").await;
            return;
        }
    };

    if uri.starts_with("share:") {
        // Route through session-reuse pool.
        let pool = share_registry.get_or_create(&uri).await;

        let (session, channel_handle, upstream) = match pool.acquire().await {
            Ok(t) => t,
            Err(e) => {
                log!("blit-proxy: [{uri}] share upstream unavailable: {e}");
                let msg = format!("error {e}\n");
                let _ = downstream.write_all(msg.as_bytes()).await;
                return;
            }
        };

        if downstream.write_all(b"ok\n").await.is_err() {
            session.close_channel(channel_handle);
            return;
        }

        pool.client_connected();

        let (mut ds_read, mut ds_write) = tokio::io::split(downstream);
        let (mut us_read, mut us_write) = (upstream.reader, upstream.writer);

        let mut d2u =
            tokio::spawn(async move { tokio::io::copy(&mut ds_read, &mut us_write).await });
        let mut u2d =
            tokio::spawn(async move { tokio::io::copy(&mut us_read, &mut ds_write).await });

        tokio::select! { _ = &mut d2u => { u2d.abort(); }, _ = &mut u2d => { d2u.abort(); } }

        // Close the DataChannel but keep the WebRTC session alive for reuse.
        session.close_channel(channel_handle);
        pool.release(session).await;
        pool.client_disconnected();
        return;
    }

    let pool = registry.get_or_create(&uri).await;

    let upstream = match pool.acquire().await {
        Ok(u) => u,
        Err(e) => {
            log!("blit-proxy: [{uri}] upstream unavailable: {e}");
            let msg = format!("error {e}\n");
            let _ = downstream.write_all(msg.as_bytes()).await;
            return;
        }
    };

    if downstream.write_all(b"ok\n").await.is_err() {
        return;
    }

    pool.client_connected();

    let (mut ds_read, mut ds_write) = tokio::io::split(downstream);
    let (mut us_read, mut us_write) = (upstream.reader, upstream.writer);

    let mut d2u = tokio::spawn(async move { tokio::io::copy(&mut ds_read, &mut us_write).await });
    let mut u2d = tokio::spawn(async move { tokio::io::copy(&mut us_read, &mut ds_write).await });

    tokio::select! { _ = &mut d2u => { u2d.abort(); }, _ = &mut u2d => { d2u.abort(); } }

    pool.client_disconnected();
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the proxy on the current thread (blocks until the process exits).
///
/// Reads `BLIT_PROXY_SOCK`, `BLIT_PROXY_POOL`, and `BLIT_PROXY_IDLE` from the
/// environment. When called from within the `blit` binary instead of the
/// standalone `blit-proxy` binary, `verbose` should be `false`.
pub fn run(verbose: bool) {
    if verbose {
        VERBOSE.store(true, Ordering::Relaxed);
    }

    let sock_path = proxy_socket_path();

    let pool_size: usize = std::env::var("BLIT_PROXY_POOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);

    let idle_secs: Option<u64> = std::env::var("BLIT_PROXY_IDLE")
        .ok()
        .and_then(|v| v.parse().ok());

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("blit-proxy: tokio runtime")
        .block_on(async move {
            rustls::crypto::ring::default_provider()
                .install_default()
                .ok(); // may already be installed by the CLI's runtime

            let registry = Registry::new(pool_size);
            let share_registry = ShareRegistry::new(pool_size);

            // Idle-timeout watcher.
            if let Some(idle) = idle_secs {
                let reg = registry.clone();
                let sreg = share_registry.clone();
                let sock = sock_path.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let last = reg
                            .latest_activity()
                            .await
                            .min(sreg.latest_activity().await);
                        if last == i64::MAX {
                            continue;
                        }
                        let elapsed = now_secs().saturating_sub(last) as u64;
                        if elapsed >= idle {
                            log!("blit-proxy: idle for {elapsed}s (limit {idle}s), exiting");
                            let _ = std::fs::remove_file(&sock);
                            std::process::exit(0);
                        }
                    }
                });
            }

            #[cfg(unix)]
            {
                let sock_cleanup = sock_path.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    let _ = std::fs::remove_file(&sock_cleanup);
                    std::process::exit(0);
                });
                run_listener(registry, share_registry, &sock_path).await;
            }
            #[cfg(windows)]
            {
                // Create the first pipe instance before signalling readiness
                // so that clients polling the pipe path see it immediately.
                use tokio::net::windows::named_pipe::ServerOptions;
                let first = ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(&sock_path)
                    .unwrap_or_else(|e| {
                        eprintln!("blit-proxy: cannot create pipe {sock_path}: {e}");
                        std::process::exit(1);
                    });
                // Pass `first` into run_listener so the pipe name is never
                // unowned between creation and the first client connection.
                run_listener(registry, share_registry, &sock_path, first).await;
            }
        });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_socket_path_default() {
        // Just verify it produces a non-empty string without panicking.
        let p = proxy_socket_path();
        assert!(!p.is_empty());
        assert!(p.contains("blit-proxy"));
    }

    #[test]
    fn parse_hex_valid() {
        assert_eq!(parse_hex("deadbeef"), Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(parse_hex(""), Some(vec![]));
    }

    #[test]
    fn parse_hex_odd_length() {
        assert_eq!(parse_hex("abc"), None);
    }

    #[test]
    fn parse_hex_invalid_char() {
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn extract_uri_params_no_query() {
        let (base, pass, cert) = extract_uri_params("wss://host:3264/");
        assert_eq!(base, "wss://host:3264/");
        assert_eq!(pass, None);
        assert_eq!(cert, None);
    }

    #[test]
    fn extract_uri_params_passphrase() {
        let (base, pass, cert) = extract_uri_params("wss://host:3264/?passphrase=secret");
        assert_eq!(base, "wss://host:3264/");
        assert_eq!(pass, Some("secret".into()));
        assert_eq!(cert, None);
    }

    #[test]
    fn extract_uri_params_both() {
        let (base, pass, cert) =
            extract_uri_params("wt://host:4433/?passphrase=abc&certHash=deadbeef");
        assert_eq!(base, "wt://host:4433/");
        assert_eq!(pass, Some("abc".into()));
        assert_eq!(cert, Some("deadbeef".into()));
    }

    #[test]
    fn extract_uri_params_socket_unchanged() {
        let (base, pass, cert) = extract_uri_params("socket:/tmp/blit.sock");
        assert_eq!(base, "socket:/tmp/blit.sock");
        assert_eq!(pass, None);
        assert_eq!(cert, None);
    }

    #[test]
    fn parse_wt_host_port_with_port() {
        assert_eq!(parse_wt_host_port("host:4433"), Ok(("host".into(), 4433)));
    }

    #[test]
    fn parse_wt_host_port_default() {
        assert_eq!(parse_wt_host_port("host"), Ok(("host".into(), 443)));
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn parse_share_uri_no_hub() {
        let (pass, hub) = parse_share_uri("myphrase");
        assert_eq!(pass, "myphrase");
        assert_eq!(
            hub,
            blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL)
        );
    }

    #[test]
    fn parse_share_uri_with_hub() {
        let (pass, hub) = parse_share_uri("myphrase?hub=wss://custom.example.com");
        assert_eq!(pass, "myphrase");
        assert_eq!(hub, "wss://custom.example.com");
    }

    #[test]
    fn parse_share_uri_hub_normalized() {
        let (pass, hub) = parse_share_uri("myphrase?hub=custom.example.com");
        assert_eq!(pass, "myphrase");
        assert_eq!(hub, "wss://custom.example.com");
    }

    #[test]
    fn parse_share_uri_percent_encoded_passphrase() {
        let (pass, hub) = parse_share_uri("my%3Fphrase");
        assert_eq!(pass, "my?phrase");
        assert_eq!(
            hub,
            blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL)
        );
    }
}
