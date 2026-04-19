use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub enum Transport {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    NamedPipe(tokio::net::windows::named_pipe::NamedPipeClient),
    Tcp(tokio::net::TcpStream),
    Duplex(tokio::io::DuplexStream),
}

impl Transport {
    pub fn split(
        self,
    ) -> (
        Box<dyn AsyncRead + Unpin + Send>,
        Box<dyn AsyncWrite + Unpin + Send>,
    ) {
        match self {
            #[cfg(unix)]
            Transport::Unix(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            #[cfg(windows)]
            Transport::NamedPipe(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Transport::Tcp(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
            Transport::Duplex(s) => {
                let (r, w) = tokio::io::split(s);
                (Box::new(r), Box::new(w))
            }
        }
    }
}

pub use blit_webserver::config::default_local_socket;

pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 4];
    r.read_exact(&mut hdr).await.ok()?;
    let len = u32::from_le_bytes(hdr) as usize;
    if len == 0 {
        return Some(vec![]);
    }
    if len > MAX_FRAME_SIZE {
        return None;
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

/// Read one logical S2C message, transparently reassembling any
/// `S2C_FRAGMENT` sequence emitted by the server.  Callers that consume
/// server messages should use this instead of raw `read_frame` so a
/// chunked bulk message (video keyframe, large terminal snapshot) is
/// delivered as a single contiguous buffer matching its original type.
///
/// `pending` is the caller-owned reassembly buffer — pass the same
/// `&mut Vec<u8>` across successive calls on the same stream.  Fragments
/// of a single message do not interleave with fragments of another
/// (TCP order + single-message-at-a-time sender), so one buffer is
/// enough.  Audio frames and other non-fragment messages pass through
/// untouched.
pub async fn read_message(
    r: &mut (impl AsyncRead + Unpin),
    pending: &mut Vec<u8>,
) -> Option<Vec<u8>> {
    loop {
        let frame = read_frame(r).await?;
        if frame.is_empty() || frame[0] != blit_remote::S2C_FRAGMENT {
            return Some(frame);
        }
        if frame.len() < 2 {
            // Malformed fragment header — drop, keep reading.
            continue;
        }
        let flags = frame[1];
        pending.extend_from_slice(&frame[2..]);
        if flags & blit_remote::FRAGMENT_FLAG_LAST != 0 {
            return Some(std::mem::take(pending));
        }
    }
}

pub fn make_frame(payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= u32::MAX as usize);
    let mut v = Vec::with_capacity(4 + payload.len());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

pub async fn write_frame(w: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> bool {
    w.write_all(&make_frame(payload)).await.is_ok()
}

pub async fn connect_ipc(path: &str) -> Result<Transport, String> {
    #[cfg(unix)]
    {
        Ok(Transport::Unix(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ))
    }
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        Ok(Transport::NamedPipe(
            ClientOptions::new()
                .open(path)
                .map_err(|e| format!("cannot connect to {path}: {e}"))?,
        ))
    }
}

// ---------------------------------------------------------------------------
// blit-proxy integration
// ---------------------------------------------------------------------------

/// The socket path of the single shared blit-proxy process.
/// Matches `proxy_socket_path()` in `crates/proxy/src/lib.rs`.
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

/// Build a `share:` proxy URI, embedding the hub when it's non-default.
///
/// The proxy URI format is `share:PASSPHRASE` or
/// `share:PASSPHRASE?hub=wss://custom.hub` so that blit-proxy knows
/// which signaling server to connect to.
pub fn share_proxy_uri(passphrase: &str, hub: &str) -> String {
    let normalized = blit_webrtc_forwarder::normalize_hub(hub);
    let default_hub = blit_webrtc_forwarder::normalize_hub(blit_webrtc_forwarder::DEFAULT_HUB_URL);
    // Percent-encode passphrase characters that would break query-string
    // parsing ('?', '&', '%').
    let encoded: String = passphrase
        .chars()
        .flat_map(|c| match c {
            '%' => vec!['%', '2', '5'],
            '?' => vec!['%', '3', 'F'],
            '&' => vec!['%', '2', '6'],
            other => vec![other],
        })
        .collect();
    if normalized == default_hub {
        format!("share:{encoded}")
    } else {
        // Percent-encode the hub URL for safe embedding in the query string.
        let hub_encoded: String = normalized
            .chars()
            .flat_map(|c| match c {
                '&' => vec!['%', '2', '6'],
                '%' => vec!['%', '2', '5'],
                other => vec![other],
            })
            .collect();
        format!("share:{encoded}?hub={hub_encoded}")
    }
}

/// Ensure a blit-proxy daemon is running.  Returns the socket/pipe path.
///
/// If no live proxy is found, re-execs the current binary as
/// `blit proxy-daemon` in a detached background process so it outlives
/// the calling CLI invocation.
pub async fn ensure_proxy() -> Result<String, String> {
    let sock = proxy_socket_path();

    // Check if an existing proxy is alive.
    if proxy_alive(&sock).await {
        return Ok(sock);
    }

    // Remove a stale socket file on Unix before launching.
    #[cfg(unix)]
    let _ = std::fs::remove_file(&sock);

    // Re-exec through the launcher (or current binary in dev builds).
    let exe = blit_proxy::blit_exe();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the child between fork and exec.
        // setsid() is async-signal-safe per POSIX.
        unsafe {
            std::process::Command::new(&exe)
                .arg("proxy-daemon")
                .env("BLIT_PROXY_IDLE", "300")
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
        // DETACHED_PROCESS | CREATE_NO_WINDOW — runs fully in the background.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(&exe)
            .arg("proxy-daemon")
            .env("BLIT_PROXY_IDLE", "300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("blit-proxy: spawn failed: {e}"))?;
    }

    // Wait up to 5 s for the socket/pipe to appear.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        if proxy_alive(&sock).await {
            return Ok(sock);
        }
    }
    Err(format!("blit-proxy did not become ready at {sock} in time"))
}

/// Returns true if a proxy is already listening at `path`.
async fn proxy_alive(path: &str) -> bool {
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
        false
    }
}

/// Send a `shutdown\n` command to a running blit-proxy, causing it to exit.
/// Silently does nothing if no proxy is running.
pub async fn stop_proxy() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[cfg(unix)]
    {
        let sock = proxy_socket_path();
        if !std::path::Path::new(&sock).exists() {
            return;
        }
        let Ok(mut stream) = tokio::net::UnixStream::connect(&sock).await else {
            return;
        };
        if stream.write_all(b"shutdown\n").await.is_err() {
            return;
        }
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await;
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let sock = proxy_socket_path();
        let Ok(mut stream) = ClientOptions::new().open(&sock) else {
            return;
        };
        if stream.write_all(b"shutdown\n").await.is_err() {
            return;
        }
        let mut reader = BufReader::new(&mut stream);
        let mut line = String::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_line(&mut line),
        )
        .await;
    }
}

/// Connect to an upstream via blit-proxy, auto-starting the proxy if needed.
/// Performs the `target <uri>\n` / `ok\n` handshake then returns the stream.
pub async fn connect_via_proxy(upstream_uri: &str) -> Result<Transport, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Maximum time to wait for the proxy to respond with `ok\n` after
    /// sending the `target` handshake.  This covers the full upstream
    /// connection time (e.g. WebRTC session setup for share: remotes).
    const PROXY_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    let sock = ensure_proxy().await?;

    let msg = format!("target {upstream_uri}\n");

    /// Read the proxy handshake response line byte-by-byte.
    ///
    /// Using a BufReader here would be incorrect: its read-ahead could
    /// consume server data that arrives right after `ok\n`, and that
    /// data would be silently lost when the BufReader is unwrapped via
    /// `into_inner()`.  Reading one byte at a time avoids the problem
    /// (the response is tiny — just `ok\n` or `error …\n`).
    async fn read_handshake_line<S: AsyncReadExt + Unpin>(
        stream: &mut S,
    ) -> Result<String, String> {
        let mut buf = Vec::with_capacity(64);
        let mut byte = [0u8; 1];
        loop {
            stream
                .read_exact(&mut byte)
                .await
                .map_err(|e| format!("blit-proxy: handshake read: {e}"))?;
            if byte[0] == b'\n' {
                break;
            }
            buf.push(byte[0]);
            if buf.len() > 4096 {
                return Err("blit-proxy: handshake response too long".into());
            }
        }
        Ok(String::from_utf8_lossy(&buf)
            .trim_end_matches('\r')
            .to_string())
    }

    #[cfg(unix)]
    {
        let mut stream = tokio::net::UnixStream::connect(&sock)
            .await
            .map_err(|e| format!("blit-proxy: connect to {sock}: {e}"))?;
        stream
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("blit-proxy: handshake write: {e}"))?;
        let resp = tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, read_handshake_line(&mut stream))
            .await
            .map_err(|_| format!("blit-proxy: timed out connecting to {upstream_uri}"))??;
        if resp == "ok" {
            return Ok(Transport::Unix(stream));
        } else if let Some(m) = resp.strip_prefix("error ") {
            return Err(format!("blit-proxy: {m}"));
        } else {
            return Err(format!("blit-proxy: unexpected response: {resp:?}"));
        }
    }

    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let mut stream = ClientOptions::new()
            .open(&sock)
            .map_err(|e| format!("blit-proxy: connect to {sock}: {e}"))?;
        stream
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| format!("blit-proxy: handshake write: {e}"))?;
        let resp = tokio::time::timeout(PROXY_HANDSHAKE_TIMEOUT, read_handshake_line(&mut stream))
            .await
            .map_err(|_| format!("blit-proxy: timed out connecting to {upstream_uri}"))??;
        if resp == "ok" {
            return Ok(Transport::NamedPipe(stream));
        } else if let Some(m) = resp.strip_prefix("error ") {
            return Err(format!("blit-proxy: {m}"));
        } else {
            return Err(format!("blit-proxy: unexpected response: {resp:?}"));
        }
    }

    #[allow(unreachable_code)]
    Err("blit-proxy: unsupported platform".into())
}

/// Connect to a blit-server described by a URI string.
///
/// Accepted forms (same syntax as `blit open` positional args):
///   ssh:[user@]host[:/socket]  — SSH bridge
///   tcp:host:port              — raw TCP
///   socket:/path               — explicit Unix socket / named pipe
///   share:passphrase           — WebRTC via hub
///   local                      — local blit-server (auto-start)
///   proxy:<upstream-uri>       — explicitly route via blit-proxy
///   <name>                     — bare name: looked up in `blit.remotes`
///
/// When `BLIT_PROXY` is not `0`, ssh/tcp/ws/wss/wt URIs are automatically
/// routed via blit-proxy (equivalent to the `proxy:` prefix).
pub async fn connect_uri(uri: &str, hub: &str) -> Result<Transport, String> {
    Box::pin(connect_uri_inner(
        uri,
        hub,
        std::collections::HashSet::new(),
    ))
    .await
}

async fn connect_uri_inner(
    uri: &str,
    hub: &str,
    mut visited: std::collections::HashSet<String>,
) -> Result<Transport, String> {
    // Explicit proxy: prefix — always via proxy regardless of BLIT_PROXY.
    if let Some(upstream) = uri.strip_prefix("proxy:") {
        return connect_via_proxy(upstream).await;
    }

    if let Some(rest) = uri.strip_prefix("ssh:") {
        if proxy_enabled() {
            return connect_via_proxy(uri).await;
        }
        let (user, host, socket) = blit_ssh::parse_ssh_uri(rest);
        let pool = blit_ssh::SshPool::new();
        let stream = pool
            .connect(&host, user.as_deref(), socket.as_deref())
            .await
            .map_err(|e| format!("ssh:{rest}: {e}"))?;
        return Ok(Transport::Duplex(stream));
    }
    if let Some(rest) = uri.strip_prefix("tcp:") {
        if proxy_enabled() {
            return connect_via_proxy(uri).await;
        }
        let s = tokio::net::TcpStream::connect(rest)
            .await
            .map_err(|e| format!("cannot connect to {rest}: {e}"))?;
        let _ = s.set_nodelay(true);
        return Ok(Transport::Tcp(s));
    }
    if uri.starts_with("ws://") || uri.starts_with("wss://") || uri.starts_with("wt://") {
        if proxy_enabled() {
            return connect_via_proxy(uri).await;
        }
        // Direct WS/WT connection — not implemented in blit-cli itself
        // (only blit-proxy speaks those transports).
        return Err(format!(
            "{uri}: ws/wss/wt direct connection requires blit-proxy \
             (set BLIT_PROXY=0 is not supported for these transports)"
        ));
    }
    if let Some(rest) = uri.strip_prefix("socket:") {
        return connect_ipc(rest).await;
    }
    if let Some(passphrase) = uri.strip_prefix("share:") {
        // Strip any ?hub= query string before extracting the passphrase.
        // The hub query param is only meaningful to blit-proxy; here we
        // need the raw passphrase and the caller-supplied hub.
        let passphrase_only = passphrase.split('?').next().unwrap_or(passphrase);
        if proxy_enabled() {
            let proxy_uri = share_proxy_uri(passphrase_only, hub);
            return connect_via_proxy(&proxy_uri).await;
        }
        let hub_url = blit_webrtc_forwarder::normalize_hub(hub);
        let stream = blit_webrtc_forwarder::client::connect(passphrase_only, &hub_url)
            .await
            .map_err(|e| format!("share:{passphrase_only}: {e}"))?;
        return Ok(Transport::Duplex(stream));
    }
    if uri == "local" {
        let path = default_local_socket();
        ensure_local_server(&path).await?;
        return connect_ipc(&path).await;
    }
    // Bare name — look up in blit.remotes, with cycle detection.
    let entries = blit_webserver::config::read_remotes();
    if let Some((_, target_uri)) = entries.into_iter().find(|(name, _)| name == uri) {
        if !visited.insert(uri.to_string()) {
            return Err(format!("blit.remotes: cycle detected resolving '{uri}'"));
        }
        return Box::pin(connect_uri_inner(&target_uri, hub, visited)).await;
    }
    Err(format!(
        "unknown target '{uri}' \
         (expected ssh:, tcp:, ws://, wss://, wt://, socket:, share:, proxy:, local, \
          or a name from blit.remotes)"
    ))
}

/// Returns true when the proxy should be used automatically.
/// Disabled by setting `BLIT_PROXY=0`.
pub fn proxy_enabled() -> bool {
    std::env::var("BLIT_PROXY").ok().as_deref() != Some("0")
}

/// Return the configured default target URI, if any.
///
/// Precedence: `BLIT_TARGET` env var > `blit.target` key in `blit.conf`.
/// Returns `None` if neither is set, meaning fall back to local.
pub fn default_target() -> Option<String> {
    if let Ok(v) = std::env::var("BLIT_TARGET")
        && !v.is_empty()
    {
        return Some(v);
    }
    let config = blit_webserver::config::read_config();
    config.get("blit.target").cloned()
}

pub async fn connect(on: &Option<String>, hub: &str) -> Result<Transport, String> {
    // Explicit --on flag, then BLIT_TARGET, then blit.conf `target`, then local.
    let effective_target = on.clone().or_else(default_target);
    if let Some(uri) = effective_target {
        return connect_uri(&uri, hub).await;
    }

    let path = default_local_socket();
    ensure_local_server(&path).await?;
    connect_ipc(&path).await
}

#[cfg(unix)]
pub async fn ensure_local_server(socket_path: &str) -> Result<(), String> {
    if std::path::Path::new(socket_path).exists() {
        match tokio::net::UnixStream::connect(socket_path).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                let _ = std::fs::remove_file(socket_path);
            }
        }
    }
    let config = blit_server::Config {
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
        shell_flags: std::env::var("BLIT_SHELL_FLAGS").unwrap_or_else(|_| "li".into()),
        scrollback: std::env::var("BLIT_SCROLLBACK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1_000_000),
        ipc_path: socket_path.to_string(),
        surface_encoders: blit_server::SurfaceEncoderPreference::defaults(),
        surface_quality: std::env::var("BLIT_SURFACE_QUALITY")
            .ok()
            .and_then(|v| blit_server::SurfaceQuality::parse(&v))
            .unwrap_or_default(),
        chroma: blit_server::ChromaSubsampling::from_env(),
        vaapi_device: std::env::var("BLIT_VAAPI_DEVICE")
            .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        #[cfg(unix)]
        fd_channel: None,
        verbose: false,
        max_connections: 0,
        max_ptys: 0,
        ping_interval: std::time::Duration::from_secs(10),
        skip_compositor: true,
    };
    tokio::spawn(blit_server::run(config));
    for _ in 0..100 {
        if std::path::Path::new(socket_path).exists() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err("server did not create socket in time".into())
}

#[cfg(windows)]
pub async fn ensure_local_server(pipe_path: &str) -> Result<(), String> {
    if connect_ipc(pipe_path).await.is_ok() {
        return Ok(());
    }
    let config = blit_server::Config {
        shell: std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()),
        shell_flags: String::new(),
        scrollback: std::env::var("BLIT_SCROLLBACK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1_000_000),
        ipc_path: pipe_path.to_string(),
        surface_encoders: blit_server::SurfaceEncoderPreference::defaults(),
        surface_quality: std::env::var("BLIT_SURFACE_QUALITY")
            .ok()
            .and_then(|v| blit_server::SurfaceQuality::parse(&v))
            .unwrap_or_default(),
        chroma: blit_server::ChromaSubsampling::from_env(),
        vaapi_device: std::env::var("BLIT_VAAPI_DEVICE")
            .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
        verbose: false,
        max_connections: 0,
        max_ptys: 0,
        ping_interval: std::time::Duration::from_secs(10),
        skip_compositor: true,
    };
    tokio::spawn(blit_server::run(config));
    for _ in 0..100 {
        if connect_ipc(pipe_path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Err("server did not create pipe in time".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── make_frame ──

    #[test]
    fn make_frame_empty_payload() {
        let frame = make_frame(&[]);
        assert_eq!(frame, vec![0, 0, 0, 0]);
    }

    #[test]
    fn make_frame_known_payload() {
        let frame = make_frame(b"hello");
        assert_eq!(frame.len(), 9);
        assert_eq!(&frame[0..4], &5u32.to_le_bytes());
        assert_eq!(&frame[4..], b"hello");
    }

    #[test]
    fn make_frame_single_byte() {
        let frame = make_frame(&[0xff]);
        assert_eq!(&frame[0..4], &1u32.to_le_bytes());
        assert_eq!(frame[4], 0xff);
    }

    // ── read_frame + make_frame round-trip ──

    #[tokio::test]
    async fn read_frame_round_trip() {
        let payload = b"blit protocol test";
        let frame = make_frame(payload);
        let mut cursor = std::io::Cursor::new(frame);
        let result = read_frame(&mut cursor).await.unwrap();
        assert_eq!(result, payload);
    }

    #[tokio::test]
    async fn read_frame_empty_payload() {
        let frame = make_frame(&[]);
        let mut cursor = std::io::Cursor::new(frame);
        let result = read_frame(&mut cursor).await.unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized() {
        let len = (MAX_FRAME_SIZE as u32 + 1).to_le_bytes();
        let mut cursor = std::io::Cursor::new(len.to_vec());
        let result = read_frame(&mut cursor).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_frame_eof_during_header() {
        let mut cursor = std::io::Cursor::new(vec![0x01, 0x00]);
        let result = read_frame(&mut cursor).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_frame_eof_during_body() {
        let mut data = 10u32.to_le_bytes().to_vec();
        data.extend_from_slice(b"short");
        let mut cursor = std::io::Cursor::new(data);
        let result = read_frame(&mut cursor).await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn read_frame_multiple_frames() {
        let mut data = make_frame(b"first");
        data.extend_from_slice(&make_frame(b"second"));
        let mut cursor = std::io::Cursor::new(data);
        let f1 = read_frame(&mut cursor).await.unwrap();
        let f2 = read_frame(&mut cursor).await.unwrap();
        assert_eq!(f1, b"first");
        assert_eq!(f2, b"second");
    }

    #[tokio::test]
    async fn write_frame_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(4096);
        let payload = b"write-test";
        let ok = write_frame(&mut client, payload).await;
        assert!(ok);
        drop(client);
        let result = read_frame(&mut server).await.unwrap();
        assert_eq!(result, payload);
    }

    // ── share_proxy_uri ──

    #[test]
    fn share_proxy_uri_default_hub() {
        let uri = share_proxy_uri("myphrase", blit_webrtc_forwarder::DEFAULT_HUB_URL);
        assert_eq!(uri, "share:myphrase");
    }

    #[test]
    fn share_proxy_uri_custom_hub() {
        let uri = share_proxy_uri("myphrase", "wss://custom.example.com");
        assert_eq!(uri, "share:myphrase?hub=wss://custom.example.com");
    }

    #[test]
    fn share_proxy_uri_encodes_special_chars() {
        let uri = share_proxy_uri("my?pass&word%x", blit_webrtc_forwarder::DEFAULT_HUB_URL);
        assert_eq!(uri, "share:my%3Fpass%26word%25x");
    }

    #[test]
    fn share_proxy_uri_normalizes_hub() {
        let uri = share_proxy_uri("phrase", "custom.example.com");
        assert_eq!(uri, "share:phrase?hub=wss://custom.example.com");
    }
}
