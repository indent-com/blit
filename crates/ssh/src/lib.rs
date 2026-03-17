//! Embedded SSH client for blit.
//!
//! Provides connection pooling, ssh-agent authentication, `~/.ssh/config`
//! parsing, and `direct-streamlocal` channel forwarding for connecting to
//! remote blit-servers without shelling out to the system `ssh` binary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

use russh::client;
use russh::keys::{self, PrivateKeyWithHashAlg, agent};

// ── Error ──────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ssh: {0}")]
    Russh(#[from] russh::Error),
    #[error("ssh key: {0}")]
    Keys(#[from] keys::Error),
    #[error("ssh: {0}")]
    Io(#[from] std::io::Error),
    #[error("ssh: {0}")]
    Other(String),
}

// ── Shell scripts run on the remote ────────────────────────────────────

/// Resolve the remote blit socket path.
///
/// Wrapped in `sh -c` so the POSIX script runs correctly even when the
/// remote user's login shell is fish or another non-POSIX shell.
const SOCK_SEARCH: &str = r#"sh -c 'if [ -n "$BLIT_SOCK" ]; then S="$BLIT_SOCK"; elif [ -n "$TMPDIR" ] && [ -S "$TMPDIR/blit.sock" ]; then S="$TMPDIR/blit.sock"; elif [ -S "/tmp/blit-$(id -un).sock" ]; then S="/tmp/blit-$(id -un).sock"; elif [ -S "/run/blit/$(id -un).sock" ]; then S="/run/blit/$(id -un).sock"; elif [ -n "$XDG_RUNTIME_DIR" ] && [ -S "$XDG_RUNTIME_DIR/blit.sock" ]; then S="$XDG_RUNTIME_DIR/blit.sock"; else S=/tmp/blit.sock; fi; echo "$S"'"#;

/// Escape a string for use inside double quotes in a POSIX shell.
/// Handles `\`, `$`, `` ` ``, and `"`.
fn dq_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' | '$' | '`' | '"' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Install blit on the remote if missing, then start blit-server and
/// detach it from the session.
///
/// Wrapped in `sh -c` so the POSIX script runs correctly even when the
/// remote user's login shell is fish or another non-POSIX shell.  The
/// socket path is double-quote-escaped to avoid single-quote nesting
/// issues inside the outer `sh -c '…'` wrapper.
fn install_and_start_script(socket_path: &str) -> String {
    let escaped = dq_escape(socket_path);
    format!(
        "sh -c 'export PATH=\"$HOME/.local/bin:$PATH\"; \
         if ! command -v blit >/dev/null 2>&1 && ! command -v blit-server >/dev/null 2>&1; then \
           if command -v curl >/dev/null 2>&1; then BLIT_INSTALL_DIR=\"$HOME/.local/bin\" curl -sf https://install.blit.sh | sh >&2; \
           elif command -v wget >/dev/null 2>&1; then BLIT_INSTALL_DIR=\"$HOME/.local/bin\" wget -qO- https://install.blit.sh | sh >&2; fi; \
         fi; \
         S=\"{escaped}\"; \
         if [ -S \"$S\" ]; then \
           if command -v nc >/dev/null 2>&1; then nc -z -U \"$S\" 2>/dev/null || rm -f \"$S\"; \
           elif command -v socat >/dev/null 2>&1; then socat /dev/null \"UNIX-CONNECT:$S\" 2>/dev/null || rm -f \"$S\"; fi; \
         fi; \
         if ! [ -S \"$S\" ]; then \
           if command -v blit >/dev/null 2>&1; then nohup blit server </dev/null >/dev/null 2>&1 & \
           elif command -v blit-server >/dev/null 2>&1; then nohup blit-server </dev/null >/dev/null 2>&1 & fi; \
         fi; \
         echo ok'"
    )
}

// ── SSH config resolution ──────────────────────────────────────────────

/// Resolved SSH settings for a host, from `~/.ssh/config`.
#[derive(Default)]
struct ResolvedConfig {
    hostname: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_files: Vec<PathBuf>,
    #[allow(dead_code)]
    proxy_jump: Option<String>,
}

/// Minimal `~/.ssh/config` parser. Supports Host (with `*`/`?` globs),
/// Hostname, User, Port, IdentityFile, and ProxyJump.
fn resolve_ssh_config(host: &str) -> ResolvedConfig {
    let path = match home_dir() {
        Some(h) => h.join(".ssh").join("config"),
        None => return ResolvedConfig::default(),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return ResolvedConfig::default(),
    };

    let mut result = ResolvedConfig::default();
    let mut in_matching_block = false;
    let mut in_global = true; // before the first Host line

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c.is_ascii_whitespace() || c == '=') {
            Some((k, v)) => (k.trim(), v.trim().trim_start_matches('=')),
            None => continue,
        };
        let value = value.trim();
        if key.eq_ignore_ascii_case("Host") {
            in_global = false;
            in_matching_block = value
                .split_whitespace()
                .any(|pattern| host_matches(pattern, host));
            continue;
        }
        if !in_matching_block && !in_global {
            continue;
        }
        if key.eq_ignore_ascii_case("Hostname") && result.hostname.is_none() {
            result.hostname = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("User") && result.user.is_none() {
            result.user = Some(value.to_string());
        } else if key.eq_ignore_ascii_case("Port") && result.port.is_none() {
            result.port = value.parse().ok();
        } else if key.eq_ignore_ascii_case("IdentityFile") {
            let expanded = expand_tilde(value);
            result.identity_files.push(PathBuf::from(expanded));
        } else if key.eq_ignore_ascii_case("ProxyJump") && result.proxy_jump.is_none() {
            result.proxy_jump = Some(value.to_string());
        }
    }
    result
}

/// Simple glob match supporting `*` (any chars) and `?` (one char).
fn host_matches(pattern: &str, host: &str) -> bool {
    let mut p = pattern.chars().peekable();
    let mut h = host.chars().peekable();
    host_matches_inner(&mut p, &mut h)
}

fn host_matches_inner(
    p: &mut std::iter::Peekable<std::str::Chars>,
    h: &mut std::iter::Peekable<std::str::Chars>,
) -> bool {
    while let Some(&pc) = p.peek() {
        match pc {
            '*' => {
                p.next();
                if p.peek().is_none() {
                    return true; // trailing * matches everything
                }
                // Try matching * against 0..N chars of h
                loop {
                    let mut p2 = p.clone();
                    let mut h2 = h.clone();
                    if host_matches_inner(&mut p2, &mut h2) {
                        return true;
                    }
                    if h.next().is_none() {
                        return false;
                    }
                }
            }
            '?' => {
                p.next();
                if h.next().is_none() {
                    return false;
                }
            }
            _ => {
                p.next();
                match h.next() {
                    Some(hc) if hc == pc => {}
                    _ => return false,
                }
            }
        }
    }
    h.peek().is_none()
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return format!("{}/{rest}", home.display());
    }
    path.to_string()
}

// ── Handler ────────────────────────────────────────────────────────────

struct SshHandler {
    host: String,
    port: u16,
}

impl client::Handler for SshHandler {
    type Error = Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        let known_hosts_path = match home_dir() {
            Some(h) => h.join(".ssh").join("known_hosts"),
            None => return Ok(true), // No home dir — accept
        };
        if !known_hosts_path.exists() {
            // No known_hosts file — accept-new behaviour: create file and
            // record the key.
            if let Some(parent) = known_hosts_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            append_known_host(&known_hosts_path, &self.host, self.port, server_public_key);
            return Ok(true);
        }
        match keys::check_known_hosts_path(
            &self.host,
            self.port,
            server_public_key,
            &known_hosts_path,
        ) {
            Ok(true) => Ok(true),
            Ok(false) => {
                // Key not in file — accept-new: append and accept.
                append_known_host(&known_hosts_path, &self.host, self.port, server_public_key);
                Ok(true)
            }
            Err(keys::Error::KeyChanged { .. }) => Err(Error::Other(format!(
                "host key for {}:{} has changed! \
                     This could indicate a man-in-the-middle attack. \
                     Remove the old key from ~/.ssh/known_hosts to continue.",
                self.host, self.port
            ))),
            Err(_) => {
                // Other errors (parse failure, etc.) — accept-new.
                append_known_host(&known_hosts_path, &self.host, self.port, server_public_key);
                Ok(true)
            }
        }
    }
}

fn append_known_host(path: &Path, host: &str, port: u16, key: &keys::PublicKey) {
    use keys::PublicKeyBase64;
    let host_entry = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let algo = key.algorithm().to_string();
    let b64 = key.public_key_base64();
    let line = format!("{host_entry} {algo} {b64}\n");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(line.as_bytes())
        });
}

// ── SSH Pool ───────────────────────────────────────────────────────────

/// SSH connection pool. Maintains persistent SSH connections and opens
/// channels on demand. Multiple channels share a single TCP+SSH connection
/// per host. Thread-safe and cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct SshPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    /// Cached connections keyed by `"user@host:port"`.
    connections: Mutex<HashMap<String, CachedConnection>>,
}

struct CachedConnection {
    handle: client::Handle<SshHandler>,
    /// Resolved remote blit socket path (cached after first resolution).
    remote_socket: Option<String>,
}

impl Default for SshPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SshPool {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PoolInner {
                connections: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Open a `direct-streamlocal` channel to a remote blit-server.
    ///
    /// - Resolves `~/.ssh/config` for the target host.
    /// - Reuses an existing SSH connection if available.
    /// - Authenticates via ssh-agent, then falls back to key files.
    /// - If `remote_socket` is `None`, discovers the socket path on the remote.
    /// - Auto-starts blit-server on the remote if needed.
    /// - Returns a bidirectional `DuplexStream` connected to the remote socket.
    pub async fn connect(
        &self,
        host: &str,
        user: Option<&str>,
        remote_socket: Option<&str>,
    ) -> Result<tokio::io::DuplexStream, Error> {
        let config = resolve_ssh_config(host);
        let effective_host = config.hostname.as_deref().unwrap_or(host);
        let effective_user = user
            .map(String::from)
            .or(config.user.clone())
            .unwrap_or_else(current_username);
        let effective_port = config.port.unwrap_or(22);

        let key = format!("{effective_user}@{effective_host}:{effective_port}");

        let mut conns = self.inner.connections.lock().await;

        // Try reusing an existing connection.
        let need_new = match conns.get(&key) {
            Some(cached) => cached.handle.is_closed(),
            None => true,
        };

        if need_new {
            let handle =
                establish_connection(effective_host, effective_port, &effective_user, &config)
                    .await?;
            conns.insert(
                key.clone(),
                CachedConnection {
                    handle,
                    remote_socket: None,
                },
            );
        }

        let cached = conns.get_mut(&key).unwrap();

        // Resolve remote socket path if not cached and not explicitly provided.
        let socket_path = if let Some(explicit) = remote_socket {
            explicit.to_string()
        } else if let Some(ref cached_path) = cached.remote_socket {
            cached_path.clone()
        } else {
            let path = exec_command(&cached.handle, SOCK_SEARCH).await?;
            let path = path.trim().to_string();
            if path.is_empty() {
                return Err(Error::Other(
                    "could not determine remote blit socket path".into(),
                ));
            }
            cached.remote_socket = Some(path.clone());
            path
        };

        // Try to open the channel. If it fails, install + start and retry.
        let channel = match cached
            .handle
            .channel_open_direct_streamlocal(&socket_path)
            .await
        {
            Ok(ch) => ch,
            Err(_first_err) => {
                // Install blit if missing and (re)start the server.
                let _ = exec_command(&cached.handle, &install_and_start_script(&socket_path)).await;
                // Retry with back-off: the server needs a moment to create
                // the socket after starting.
                let mut last_err = _first_err;
                for attempt in 0..10 {
                    tokio::time::sleep(std::time::Duration::from_millis(100 * (attempt + 1))).await;
                    match cached
                        .handle
                        .channel_open_direct_streamlocal(&socket_path)
                        .await
                    {
                        Ok(ch) => return Ok(bridge_channel(ch)),
                        Err(e) => last_err = e,
                    }
                }
                return Err(Error::Other(format!(
                    "failed to connect to {socket_path} after install: {last_err}"
                )));
            }
        };

        Ok(bridge_channel(channel))
    }
}

/// Bridge an SSH channel to a `DuplexStream` so callers get a standard
/// tokio type with no russh types leaking.
fn bridge_channel(channel: russh::Channel<russh::client::Msg>) -> tokio::io::DuplexStream {
    let stream = channel.into_stream();
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let (mut sr, mut sw) = tokio::io::split(server);
        let (mut cr, mut cw) = tokio::io::split(stream);
        tokio::select! {
            _ = tokio::io::copy(&mut cr, &mut sw) => {}
            _ = tokio::io::copy(&mut sr, &mut cw) => {}
        }
    });
    client
}

// ── Connection + Authentication ────────────────────────────────────────

async fn establish_connection(
    host: &str,
    port: u16,
    user: &str,
    config: &ResolvedConfig,
) -> Result<client::Handle<SshHandler>, Error> {
    let ssh_config = client::Config {
        ..Default::default()
    };

    let handler = SshHandler {
        host: host.to_string(),
        port,
    };

    let mut handle = client::connect(Arc::new(ssh_config), (host, port), handler).await?;

    // Try ssh-agent first.
    if try_agent_auth(&mut handle, user).await {
        return Ok(handle);
    }

    // Fall back to key files.
    if try_key_file_auth(&mut handle, user, config).await? {
        return Ok(handle);
    }

    Err(Error::Other(format!(
        "authentication failed for {user}@{host}:{port} \
         (tried ssh-agent and key files)"
    )))
}

/// Try authenticating via ssh-agent. Returns true on success.
#[cfg(unix)]
async fn try_agent_auth(handle: &mut client::Handle<SshHandler>, user: &str) -> bool {
    let agent_path = match std::env::var("SSH_AUTH_SOCK") {
        Ok(p) if !p.is_empty() => p,
        _ => return false,
    };
    let stream = match tokio::net::UnixStream::connect(&agent_path).await {
        Ok(s) => s,
        Err(e) => {
            log::debug!("ssh-agent connect failed: {e}");
            return false;
        }
    };
    let mut agent = agent::client::AgentClient::connect(stream);
    let identities = match agent.request_identities().await {
        Ok(ids) => ids,
        Err(e) => {
            log::debug!("ssh-agent request_identities failed: {e}");
            return false;
        }
    };
    for identity in &identities {
        let public_key = identity.public_key().into_owned();
        match handle
            .authenticate_publickey_with(user, public_key, None, &mut agent)
            .await
        {
            Ok(russh::client::AuthResult::Success) => return true,
            Ok(_) => continue,
            Err(e) => {
                log::debug!("ssh-agent auth attempt failed: {e}");
                continue;
            }
        }
    }
    false
}

/// On non-Unix platforms, agent auth is not yet supported — fall back to key files.
#[cfg(not(unix))]
async fn try_agent_auth(_handle: &mut client::Handle<SshHandler>, _user: &str) -> bool {
    false
}

/// Try authenticating with key files. Returns true on success.
async fn try_key_file_auth(
    handle: &mut client::Handle<SshHandler>,
    user: &str,
    config: &ResolvedConfig,
) -> Result<bool, Error> {
    let home = match home_dir() {
        Some(h) => h,
        None => return Ok(false),
    };

    // Collect candidate key paths: explicit from config + defaults.
    let mut candidates: Vec<PathBuf> = config.identity_files.clone();
    for default in &["id_ed25519", "id_ecdsa", "id_rsa"] {
        let p = home.join(".ssh").join(default);
        if !candidates.contains(&p) {
            candidates.push(p);
        }
    }

    for path in &candidates {
        if !path.exists() {
            continue;
        }
        let key = match keys::load_secret_key(path, None) {
            Ok(k) => k,
            Err(e) => {
                log::debug!("could not load {}: {e}", path.display());
                continue;
            }
        };

        // Determine the best RSA hash algorithm if applicable.
        let hash_alg = handle.best_supported_rsa_hash().await.ok().flatten();
        let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg.flatten());

        match handle.authenticate_publickey(user, key_with_hash).await {
            Ok(russh::client::AuthResult::Success) => return Ok(true),
            Ok(_) => continue,
            Err(e) => {
                log::debug!("key auth failed for {}: {e}", path.display());
                continue;
            }
        }
    }
    Ok(false)
}

// ── Remote command execution ───────────────────────────────────────────

/// Execute a command on the remote and return its stdout.
async fn exec_command(handle: &client::Handle<SshHandler>, cmd: &str) -> Result<String, Error> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, cmd.as_bytes()).await?;

    let mut output = Vec::new();
    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => output.extend_from_slice(&data),
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
            _ => continue,
        }
    }
    Ok(String::from_utf8_lossy(&output).into_owned())
}

// ── Helpers ────────────────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
}

fn current_username() -> String {
    #[cfg(unix)]
    {
        std::env::var("USER").unwrap_or_else(|_| "root".into())
    }
    #[cfg(windows)]
    {
        std::env::var("USERNAME").unwrap_or_else(|_| "user".into())
    }
}

/// Parse an SSH URI: `[user@]host[:/socket]`.
/// Returns `(user, host, socket)`.
pub fn parse_ssh_uri(s: &str) -> (Option<String>, String, Option<String>) {
    let colon_start = s.find('@').map(|a| a + 1).unwrap_or(0);
    let (host_part, socket) = if let Some(rel) = s[colon_start..].find(':') {
        let pos = colon_start + rel;
        let path = &s[pos + 1..];
        if path.is_empty() {
            (s, None)
        } else {
            (&s[..pos], Some(path.to_string()))
        }
    } else {
        (s, None)
    };
    let (user, host) = if let Some(at) = host_part.rfind('@') {
        (
            Some(host_part[..at].to_string()),
            host_part[at + 1..].to_string(),
        )
    } else {
        (None, host_part.to_string())
    };
    (user, host, socket)
}
