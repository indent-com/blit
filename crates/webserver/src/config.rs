use axum::extract::ws::{Message, WebSocket};
use futures_util::SinkExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

pub use crate::passphrase::AuthPassphrase;

pub struct ConfigState {
    pub tx: broadcast::Sender<String>,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<String>(64);
        spawn_watcher(tx.clone());
        Self { tx }
    }
}

const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const AUTH_MAX_UNAUTHENTICATED: usize = 32;
const AUTH_MAX_FAILURES: u32 = 5;
const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const AUTH_LOCKOUT: Duration = Duration::from_secs(60);

/// Shared authentication throttle for WebSocket/WebTransport passphrase checks.
///
/// It limits concurrent unauthenticated handshakes globally and temporarily
/// locks out peers that repeatedly fail authentication. Peer keys are supplied
/// by callers (typically the remote IP address, or a global fallback when the
/// transport cannot expose one).
#[derive(Clone)]
pub struct AuthThrottle {
    inner: Arc<Mutex<AuthThrottleInner>>,
    max_unauthenticated: usize,
    max_failures: u32,
    failure_window: Duration,
    lockout: Duration,
}

struct AuthThrottleInner {
    active_unauthenticated: usize,
    peers: HashMap<String, PeerAuthState>,
}

struct PeerAuthState {
    failures: u32,
    first_failure: Instant,
    locked_until: Option<Instant>,
}

/// RAII guard for one in-progress unauthenticated auth attempt.
pub struct AuthAttemptGuard {
    throttle: AuthThrottle,
    peer: String,
    released: bool,
}

impl Default for AuthThrottle {
    fn default() -> Self {
        Self::new()
    }
}

pub struct AuthContext<'a> {
    pub throttle: &'a AuthThrottle,
    pub peer: &'a str,
}

impl AuthThrottle {
    pub fn new() -> Self {
        Self::with_limits(
            AUTH_MAX_UNAUTHENTICATED,
            AUTH_MAX_FAILURES,
            AUTH_FAILURE_WINDOW,
            AUTH_LOCKOUT,
        )
    }

    fn with_limits(
        max_unauthenticated: usize,
        max_failures: u32,
        failure_window: Duration,
        lockout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(AuthThrottleInner {
                active_unauthenticated: 0,
                peers: HashMap::new(),
            })),
            max_unauthenticated,
            max_failures: max_failures.max(1),
            failure_window,
            lockout,
        }
    }

    pub fn begin(&self, peer: impl Into<String>) -> Option<AuthAttemptGuard> {
        let peer = peer.into();
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner.prune(now, self.failure_window);

        // Both refusals answer the client with AUTH_BUSY, which is deliberately
        // indistinguishable from a healthy server to anyone probing. Say why
        // here so an operator seeing "server busy" in the UI can tell a peer
        // lockout from saturation without reproducing it.
        if inner.active_unauthenticated >= self.max_unauthenticated {
            eprintln!(
                "blit: auth refused for {peer}: {} concurrent unauthenticated handshakes \
                 (limit {})",
                inner.active_unauthenticated, self.max_unauthenticated
            );
            return None;
        }
        if let Some(until) = inner
            .peers
            .get(&peer)
            .and_then(|state| state.locked_until)
            .filter(|until| *until > now)
        {
            eprintln!(
                "blit: auth refused for {peer}: locked out for another {}s",
                until.duration_since(now).as_secs()
            );
            return None;
        }

        inner.active_unauthenticated += 1;
        Some(AuthAttemptGuard {
            throttle: self.clone(),
            peer,
            released: false,
        })
    }

    fn record_success(&self, peer: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.peers.remove(peer);
    }

    fn record_failure(&self, peer: &str) {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();
        inner.prune(now, self.failure_window);
        let state = inner
            .peers
            .entry(peer.to_string())
            .or_insert_with(|| PeerAuthState {
                failures: 0,
                first_failure: now,
                locked_until: None,
            });

        if now.duration_since(state.first_failure) > self.failure_window {
            state.failures = 0;
            state.first_failure = now;
            state.locked_until = None;
        }
        state.failures = state.failures.saturating_add(1);
        if state.failures >= self.max_failures {
            state.failures = 0;
            state.first_failure = now;
            state.locked_until = Some(now + self.lockout);
            // Only a presented-and-mismatched passphrase reaches here, so this
            // names a real cause rather than a reconnect that happened to look
            // like one. Without it a lockout is invisible server-side and shows
            // up only as a client that cannot log in.
            eprintln!(
                "blit: auth lockout for {peer}: {} wrong passphrases within {}s — \
                 refusing for {}s",
                self.max_failures,
                self.failure_window.as_secs(),
                self.lockout.as_secs()
            );
        } else {
            eprintln!(
                "blit: wrong passphrase from {peer} ({}/{} before lockout)",
                state.failures, self.max_failures
            );
        }
    }

    fn release(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.active_unauthenticated = inner.active_unauthenticated.saturating_sub(1);
    }
}

impl AuthThrottleInner {
    fn prune(&mut self, now: Instant, failure_window: Duration) {
        self.peers.retain(|_, state| {
            if state
                .locked_until
                .is_some_and(|locked_until| locked_until > now)
            {
                return true;
            }
            state.failures > 0 && now.duration_since(state.first_failure) <= failure_window
        });
    }
}

impl AuthAttemptGuard {
    pub fn record_success(mut self) {
        self.throttle.record_success(&self.peer);
        self.release();
    }

    pub fn record_failure(mut self) {
        self.throttle.record_failure(&self.peer);
        self.release();
    }

    fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.throttle.release();
        }
    }
}

impl Drop for AuthAttemptGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Wire response for a passphrase the server rejected. Clients treat it as
/// "this credential is wrong" and discard it.
pub const AUTH_REJECTED: &str = "auth";

/// Wire response for an attempt the throttle refused before it could be
/// checked — a peer lockout or the global concurrent-handshake cap. The
/// credential was never examined, so clients must keep it and retry rather
/// than dropping the user at the login screen.
pub const AUTH_BUSY: &str = "busy";

/// How one authentication attempt ended.
///
/// The distinction matters to the throttle: only a passphrase that was
/// actually presented and did not match may count against a peer's failure
/// budget. A socket that goes away mid-handshake — a page navigation, a
/// suspended tab, a client abandoning a probe — is an ordinary reconnect, and
/// charging it locks out honest users (docs/design/net.md § service worker).
enum AuthOutcome {
    Accepted,
    Rejected,
    Abandoned,
}

/// Authenticate a text WebSocket passphrase with timeout, active-connection
/// limiting, and failed-attempt backoff. Sends [`AUTH_REJECTED`] and closes on
/// a wrong passphrase, [`AUTH_BUSY`] when the throttle refused the attempt.
/// When ok_message is present, it is sent after a successful authentication
/// before returning.
pub async fn authenticate_text_ws(
    ws: &mut WebSocket,
    token: &AuthPassphrase,
    throttle: &AuthThrottle,
    peer: &str,
    ok_message: Option<&str>,
) -> bool {
    let Some(guard) = throttle.begin(peer.to_string()) else {
        let _ = ws.send(Message::Text(AUTH_BUSY.into())).await;
        let _ = ws.close().await;
        return false;
    };

    let outcome = tokio::time::timeout(AUTH_TIMEOUT, async {
        loop {
            match ws.recv().await {
                Some(Ok(Message::Text(pass))) => {
                    break if token.verify(pass.trim()) {
                        AuthOutcome::Accepted
                    } else {
                        AuthOutcome::Rejected
                    };
                }
                Some(Ok(Message::Ping(d))) => {
                    let _ = ws.send(Message::Pong(d)).await;
                }
                _ => break AuthOutcome::Abandoned,
            }
        }
    })
    .await
    // A handshake that never produced a passphrase within the window is
    // abandoned, not failed.
    .unwrap_or(AuthOutcome::Abandoned);

    match outcome {
        AuthOutcome::Accepted => {
            guard.record_success();
            if let Some(msg) = ok_message
                && ws.send(Message::Text(msg.into())).await.is_err()
            {
                return false;
            }
            true
        }
        AuthOutcome::Rejected => {
            guard.record_failure();
            let _ = ws.send(Message::Text(AUTH_REJECTED.into())).await;
            let _ = ws.close().await;
            false
        }
        // Dropping the guard releases the handshake slot without touching the
        // peer's failure count.
        AuthOutcome::Abandoned => {
            drop(guard);
            let _ = ws.close().await;
            false
        }
    }
}

fn blit_config_dir() -> PathBuf {
    #[cfg(unix)]
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
            PathBuf::from(home).join(".config")
        });
    #[cfg(windows)]
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"C:\ProgramData"));
    base.join("blit")
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_CONFIG") {
        return PathBuf::from(p);
    }
    blit_config_dir().join("blit.conf")
}

pub fn remotes_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_REMOTES") {
        return PathBuf::from(p);
    }
    blit_config_dir().join("blit.remotes")
}

/// Resolve the local blit server IPC socket path.
///
/// Checks `BLIT_SOCK` first (explicit override), then probes well-known
/// paths with existence checks so we find a running server regardless of
/// which fallback it used at startup.
#[cfg(unix)]
pub fn default_local_socket() -> String {
    if let Ok(p) = std::env::var("BLIT_SOCK") {
        return p;
    }
    if let Ok(dir) = std::env::var("TMPDIR") {
        let p = format!("{dir}/blit.sock");
        if std::path::Path::new(&p).exists() {
            return p;
        }
    }
    if let Ok(user) = std::env::var("USER") {
        let p = format!("/tmp/blit-{user}.sock");
        if std::path::Path::new(&p).exists() {
            return p;
        }
        let sys = format!("/run/blit/{user}.sock");
        if std::path::Path::new(&sys).exists() {
            return sys;
        }
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return format!("{dir}/blit.sock");
    }
    "/tmp/blit.sock".into()
}

/// Resolve the local blit server IPC pipe path (Windows).
#[cfg(windows)]
pub fn default_local_socket() -> String {
    if let Ok(p) = std::env::var("BLIT_SOCK") {
        return p;
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "default".into());
    format!(r"\\.\pipe\blit-{user}")
}

/// Acquire an exclusive cross-process lock for the config directory.
/// Returns a `File` whose lifetime holds the lock (released on drop).
/// On non-Unix platforms this is a no-op that returns `None`.
fn lock_config_dir() -> Option<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = blit_config_dir();
        let _ = std::fs::create_dir_all(&dir);
        let lock_path = dir.join("blit.lock");
        if let Ok(f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
        {
            // Block until we get the lock.
            use std::os::unix::io::AsRawFd;
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } == 0 {
                return Some(f);
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub fn read_config() -> HashMap<String, String> {
    let path = config_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            eprintln!("blit: could not read {}: {e}", path.display());
            return HashMap::new();
        }
    };
    parse_config_str(&contents)
}

/// A single entry in `blit.remotes`. `disabled` entries are persisted as
/// `# name = uri` and are excluded from connection resolution but preserved
/// across restarts so users can re-enable them later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub uri: String,
    pub disabled: bool,
}

/// Read `blit.remotes` and return ordered enabled `(name, uri)` pairs.
/// If the file does not exist, provisions it with `local = local` (0600).
/// Disabled entries are filtered out — use [`read_remotes_full`] to keep them.
pub fn read_remotes() -> Vec<(String, String)> {
    read_remotes_full()
        .into_iter()
        .filter(|e| !e.disabled)
        .map(|e| (e.name, e.uri))
        .collect()
}

/// Read `blit.remotes` including disabled entries.
pub fn read_remotes_full() -> Vec<RemoteEntry> {
    let path = remotes_path();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let default = vec![RemoteEntry {
                name: "local".to_string(),
                uri: "local".to_string(),
                disabled: false,
            }];
            write_remotes(&default);
            return default;
        }
        Err(e) => {
            eprintln!("blit: could not read {}: {e}", path.display());
            return vec![];
        }
    };
    parse_remotes_full(&contents)
}

/// Atomically read-modify-write `blit.conf` under an exclusive flock.
pub fn modify_config(f: impl FnOnce(&mut HashMap<String, String>)) {
    let _lock = lock_config_dir();
    let mut map = read_config();
    f(&mut map);
    write_config(&map);
}

/// Atomically read-modify-write `blit.remotes` under an exclusive flock.
pub fn modify_remotes(f: impl FnOnce(&mut Vec<RemoteEntry>)) {
    let _lock = lock_config_dir();
    let mut entries = read_remotes_full();
    f(&mut entries);
    write_remotes(&entries);
}

/// Parse `blit.remotes` content into ordered enabled `(name, uri)` pairs.
/// Disabled entries (`# name = uri`) are filtered out — use
/// [`parse_remotes_full`] to keep them.
pub fn parse_remotes_str(contents: &str) -> Vec<(String, String)> {
    parse_remotes_full(contents)
        .into_iter()
        .filter(|e| !e.disabled)
        .map(|e| (e.name, e.uri))
        .collect()
}

/// Whether `name` can be an entry name in `blit.remotes` / `blit.roots`.
///
/// Every rule is forced by a format the name has to survive intact:
///
/// * the file is `name = value`, so an `=` reparses as the start of the
///   value, and a leading `#` reparses as the disabled marker — an entry
///   added as enabled would come back disabled;
/// * the config-socket verbs (`remotes-add <name> <uri>`) are
///   space-delimited, so any whitespace splits the name in two;
/// * a newline splits the line itself.
///
/// One function rather than a condition per caller. There were four, they had
/// drifted apart, and the parser's was the strictest — so `blit remote add
/// 'my remote' ssh:host` reported success, wrote the line, and the next read
/// dropped it without a word.
pub fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('=')
        && !name.starts_with('#')
        && !name.contains(char::is_whitespace)
}

/// Parse `name = value` lines shared by `blit.remotes` and `blit.roots`.
/// Format: `name = value` for enabled; `# name = value` (optional whitespace
/// after `#`) for disabled. Blank lines and other `#` lines are ignored.
/// Duplicate names: last wins; first-seen order is preserved.
fn parse_kv_entries(contents: &str) -> Vec<(String, String, bool)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, (String, String, bool)> = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (body, disabled) = if let Some(rest) = line.strip_prefix('#') {
            (rest.trim_start(), true)
        } else {
            (line, false)
        };
        let Some((k, v)) = body.split_once('=') else {
            continue;
        };
        let name = k.trim().to_string();
        let value = v.trim().to_string();
        // Names that cannot round-trip are never materialized. Writers
        // reject them up front (see `valid_entry_name`); this is the backstop
        // for a hand-edited file.
        if !valid_entry_name(&name) || value.is_empty() {
            continue;
        }
        if !map.contains_key(&name) {
            order.push(name.clone());
        }
        map.insert(name.clone(), (name, value, disabled));
    }
    order.into_iter().map(|k| map.remove(&k).unwrap()).collect()
}

/// Parse `blit.remotes` content including disabled entries.
/// Format: `name = uri` for enabled; `# name = uri` (with optional whitespace
/// after `#`) for disabled. Other `#` lines and blank lines are ignored.
/// Duplicate names: last wins (same as blit.conf).
pub fn parse_remotes_full(contents: &str) -> Vec<RemoteEntry> {
    parse_kv_entries(contents)
        .into_iter()
        .map(|(name, uri, disabled)| RemoteEntry {
            name,
            uri,
            disabled,
        })
        .collect()
}

fn serialize_remotes(entries: &[RemoteEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        if e.disabled {
            out.push_str("# ");
        }
        out.push_str(&e.name);
        out.push_str(" = ");
        out.push_str(&e.uri);
        out.push('\n');
    }
    out
}

/// Write `blit.remotes` atomically with mode 0o600 (owner read/write only).
pub fn write_remotes(entries: &[RemoteEntry]) {
    let path = remotes_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let contents = serialize_remotes(entries);
    write_secret_file(&path, &contents);
}

/// Write a file with mode 0o600 (owner-only).  On Unix this is done by
/// writing to a temp file with the right mode, then atomically renaming.
/// On Windows we just write normally (ACLs are handled separately if needed).
fn write_secret_file(path: &PathBuf, contents: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Write to a sibling temp file with a unique name (pid + counter)
        // so concurrent writers don't clobber each other's temp files.
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let tmp = path.with_extension(format!("tmp.{pid}.{seq}"));
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(contents.as_bytes())
            });
        if result.is_ok() {
            let _ = std::fs::rename(&tmp, path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(path, contents);
    }
}

fn serialize_config_str(map: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = map.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    lines.sort();
    lines.push(String::new());
    lines.join("\n")
}

pub fn write_config(map: &HashMap<String, String>) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_secret_file(&path, &serialize_config_str(map));
}

/// Watches a single file in its parent directory and calls `on_change`
/// whenever the file is modified.  Skips access (read) events.
fn spawn_file_watcher<F>(path: PathBuf, label: &'static str, on_change: F)
where
    F: Fn() + Send + 'static,
{
    use notify::{RecursiveMode, Watcher};

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let watch_dir = path.parent().unwrap_or(&path).to_path_buf();
    let file_name = path.file_name().map(|n| n.to_os_string());

    std::thread::Builder::new()
        .name(format!("{label}-watcher"))
        .spawn(move || {
            let (ntx, nrx) = std::sync::mpsc::channel();
            let mut watcher = match notify::recommended_watcher(ntx) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("blit: {label} watcher failed: {e}");
                    return;
                }
            };
            if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
                eprintln!("blit: {label} watch failed: {e}");
                return;
            }
            loop {
                match nrx.recv() {
                    Ok(Ok(event)) => {
                        if matches!(event.kind, notify::EventKind::Access(_)) {
                            continue;
                        }
                        let matches = file_name.as_ref().is_none_or(|name| {
                            event.paths.iter().any(|p| p.file_name() == Some(name))
                        });
                        if matches {
                            on_change();
                        }
                    }
                    Ok(Err(_)) => continue,
                    Err(_) => break,
                }
            }
        })
        .expect("failed to spawn file-watcher thread");
}

fn spawn_watcher(tx: broadcast::Sender<String>) {
    let path = config_path();
    spawn_file_watcher(path, "config", move || {
        let map = read_config();
        for (k, v) in &map {
            let _ = tx.send(format!("{k}={v}"));
        }
        let _ = tx.send("ready".into());
    });
}

// ---------------------------------------------------------------------------
// RemotesState — live-reloading blit.remotes with 0o600 permissions
// ---------------------------------------------------------------------------

/// Manages `blit.remotes`: reads/writes the file, watches for external
/// changes, and broadcasts the serialised contents to all subscribers.
///
/// The broadcast value is the raw file text (same as what `read_remotes`
/// would parse), sent as a single string so receivers can re-parse it.
/// The config WebSocket handler prefixes it with `"remotes:"`.
#[derive(Clone)]
pub struct RemotesState {
    inner: Arc<RemotesInner>,
}

struct RemotesInner {
    /// Cached current contents (raw file text, normalized).
    contents: RwLock<String>,
    tx: broadcast::Sender<String>,

    /// False in ephemeral mode: `set` broadcasts but never touches disk.
    /// Without this, "no file I/O" was only true until the first write —
    /// which let a `blit open` session's temporary destination list
    /// overwrite the user's real config, and let the unit tests clobber
    /// `~/.config/blit/` on every `cargo test`.
    persist: bool,
}

impl RemotesState {
    /// Full persistent mode: reads `blit.remotes`, watches it for changes.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        let inner = Arc::new(RemotesInner {
            contents: RwLock::new(serialize_remotes(&read_remotes_full())),
            tx,
            persist: true,
        });
        let watcher_inner = inner.clone();
        spawn_file_watcher(remotes_path(), "remotes", move || {
            // Read directly — do not auto-provision. The file may be
            // intentionally empty (user removed all remotes).
            let text = std::fs::read_to_string(remotes_path()).unwrap_or_default();
            *watcher_inner.contents.write().unwrap() = text.clone();
            let _ = watcher_inner.tx.send(text);
        });
        Self { inner }
    }

    /// Ephemeral mode: starts with the given text, no file I/O, no watcher.
    /// Used by `blit open` to advertise the session's destinations to the
    /// browser without touching `blit.remotes`.
    pub fn ephemeral(initial: String) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RemotesInner {
                contents: RwLock::new(initial),
                tx,
                persist: false,
            }),
        }
    }

    /// Returns the current serialized remotes contents.
    pub fn get(&self) -> String {
        self.inner.contents.read().unwrap().clone()
    }

    /// Overwrite `blit.remotes` with `entries` and broadcast the change.
    pub fn set(&self, entries: &[RemoteEntry]) {
        if self.inner.persist {
            write_remotes(entries);
        }
        let text = serialize_remotes(entries);
        *self.inner.contents.write().unwrap() = text.clone();
        let _ = self.inner.tx.send(text);
    }

    /// Atomically read-modify-write `blit.remotes` under an exclusive flock,
    /// then update the in-memory cache and broadcast.
    pub fn modify(&self, f: impl FnOnce(&mut Vec<RemoteEntry>)) {
        let _lock = lock_config_dir();
        let mut entries = parse_remotes_full(&self.get());
        f(&mut entries);
        self.set(&entries);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.tx.subscribe()
    }
}

impl Default for RemotesState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// RootsState — live-reloading blit.roots (IDE workspace roots)
// ---------------------------------------------------------------------------

/// A single entry in `blit.roots`: a named workspace root the IDE can browse.
/// `value` is an opaque `remote:path` spec (e.g. `local:/home/me/app`); the
/// server stores and serves it verbatim, the client parses the remote/path.
/// `disabled` entries are persisted as `# name = value` and hidden from the
/// picker but preserved for re-enabling. Unlike `blit.remotes`, the file is
/// never auto-provisioned — an absent file just means no declared roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootEntry {
    pub name: String,
    pub value: String,
    pub disabled: bool,
}

pub fn roots_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_ROOTS") {
        return PathBuf::from(p);
    }
    blit_config_dir().join("blit.roots")
}

/// Parse `blit.roots` content including disabled entries.
pub fn parse_roots_full(contents: &str) -> Vec<RootEntry> {
    parse_kv_entries(contents)
        .into_iter()
        .map(|(name, value, disabled)| RootEntry {
            name,
            value,
            disabled,
        })
        .collect()
}

/// Read `blit.roots` including disabled entries; empty if the file is absent.
pub fn read_roots_full() -> Vec<RootEntry> {
    match std::fs::read_to_string(roots_path()) {
        Ok(c) => parse_roots_full(&c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => {
            eprintln!("blit: could not read {}: {e}", roots_path().display());
            vec![]
        }
    }
}

fn serialize_roots(entries: &[RootEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        if e.disabled {
            out.push_str("# ");
        }
        out.push_str(&e.name);
        out.push_str(" = ");
        out.push_str(&e.value);
        out.push('\n');
    }
    out
}

/// Write `blit.roots` atomically with mode 0o600.
pub fn write_roots(entries: &[RootEntry]) {
    let path = roots_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_secret_file(&path, &serialize_roots(entries));
}

/// Atomically read-modify-write `blit.roots` under an exclusive flock.
pub fn modify_roots(f: impl FnOnce(&mut Vec<RootEntry>)) {
    let _lock = lock_config_dir();
    let mut entries = read_roots_full();
    f(&mut entries);
    write_roots(&entries);
}

/// One `blit.forwards` entry: a name and a port-forward spec
/// (docs/design/net.md § A named list). Same shape as [`RootEntry`], and for
/// the same reason: `blit.conf` is a flat key→value map and cannot hold an
/// ordered list, so a list of named things gets its own file. `disabled`
/// entries are persisted as `# name = spec` and skipped by
/// `blit forward --all` but preserved for re-enabling. Never
/// auto-provisioned — an absent file means no declared forwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardEntry {
    pub name: String,
    pub spec: String,
    pub disabled: bool,
}

pub fn forwards_path() -> PathBuf {
    if let Ok(p) = std::env::var("BLIT_FORWARDS") {
        return PathBuf::from(p);
    }
    blit_config_dir().join("blit.forwards")
}

/// Parse `blit.forwards` content including disabled entries.
pub fn parse_forwards_full(contents: &str) -> Vec<ForwardEntry> {
    parse_kv_entries(contents)
        .into_iter()
        .map(|(name, spec, disabled)| ForwardEntry {
            name,
            spec,
            disabled,
        })
        .collect()
}

/// Read `blit.forwards` including disabled entries; empty if absent.
pub fn read_forwards_full() -> Vec<ForwardEntry> {
    match std::fs::read_to_string(forwards_path()) {
        Ok(c) => parse_forwards_full(&c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(e) => {
            eprintln!("blit: could not read {}: {e}", forwards_path().display());
            vec![]
        }
    }
}

fn serialize_forwards(entries: &[ForwardEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        if e.disabled {
            out.push_str("# ");
        }
        out.push_str(&e.name);
        out.push_str(" = ");
        out.push_str(&e.spec);
        out.push('\n');
    }
    out
}

/// Write `blit.forwards` atomically with mode 0o600.
pub fn write_forwards(entries: &[ForwardEntry]) {
    let path = forwards_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_secret_file(&path, &serialize_forwards(entries));
}

/// Atomically read-modify-write `blit.forwards` under an exclusive flock.
pub fn modify_forwards(f: impl FnOnce(&mut Vec<ForwardEntry>)) {
    let _lock = lock_config_dir();
    let mut entries = read_forwards_full();
    f(&mut entries);
    write_forwards(&entries);
}

/// Manages `blit.roots` exactly as [`RemotesState`] manages `blit.remotes`:
/// reads/writes the file, watches for external changes, and broadcasts the
/// raw serialized contents to all subscribers. The config WebSocket handler
/// prefixes the broadcast with `"roots:"`.
#[derive(Clone)]
pub struct RootsState {
    inner: Arc<RootsInner>,
}

struct RootsInner {
    contents: RwLock<String>,
    tx: broadcast::Sender<String>,

    /// False in ephemeral mode: `set` broadcasts but never touches disk.
    /// Without this, "no file I/O" was only true until the first write —
    /// which let a `blit open` session's temporary destination list
    /// overwrite the user's real config, and let the unit tests clobber
    /// `~/.config/blit/` on every `cargo test`.
    persist: bool,
}

impl RootsState {
    /// Full persistent mode: reads `blit.roots`, watches it for changes.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        let inner = Arc::new(RootsInner {
            contents: RwLock::new(serialize_roots(&read_roots_full())),
            tx,
            persist: true,
        });
        let watcher_inner = inner.clone();
        spawn_file_watcher(roots_path(), "roots", move || {
            let text = std::fs::read_to_string(roots_path()).unwrap_or_default();
            *watcher_inner.contents.write().unwrap() = text.clone();
            let _ = watcher_inner.tx.send(text);
        });
        Self { inner }
    }

    /// Ephemeral mode: starts with the given text, no file I/O, no watcher.
    pub fn ephemeral(initial: String) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RootsInner {
                contents: RwLock::new(initial),
                tx,
                persist: false,
            }),
        }
    }

    /// Returns the current serialized roots contents.
    pub fn get(&self) -> String {
        self.inner.contents.read().unwrap().clone()
    }

    /// Overwrite `blit.roots` with `entries` and broadcast the change.
    pub fn set(&self, entries: &[RootEntry]) {
        if self.inner.persist {
            write_roots(entries);
        }
        let text = serialize_roots(entries);
        *self.inner.contents.write().unwrap() = text.clone();
        let _ = self.inner.tx.send(text);
    }

    /// Atomically read-modify-write `blit.roots` under an exclusive flock,
    /// then update the in-memory cache and broadcast.
    pub fn modify(&self, f: impl FnOnce(&mut Vec<RootEntry>)) {
        let _lock = lock_config_dir();
        let mut entries = parse_roots_full(&self.get());
        f(&mut entries);
        self.set(&entries);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.inner.tx.subscribe()
    }
}

impl Default for RootsState {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_config_str(contents: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    map
}

/// Handle the `/config` WebSocket connection.
///
/// Protocol (server → client):
///   - `"auth"` then close — authentication rejected.
///
/// After auth:
///   1. `"ok"` — authentication accepted.
///   2. `"remotes:<text>"` — sent immediately (and re-sent on any change to
///      `blit.remotes`).  `<text>` is the raw `blit.remotes` file contents:
///      `name = uri` lines for enabled remotes, `# name = uri` lines for
///      disabled ones.  Empty string if the file does not exist.
///   3. `"roots:<text>"` — the raw `blit.roots` file contents in the same
///      `name = value` format (`value` is an opaque `remote:path` spec).
///      Empty string when there are no declared roots.
///   4. Zero or more `"key=value"` messages — current browser settings.
///   5. `"ready"` — end of initial burst; live updates follow.
///
/// After `"ready"`, the server pushes:
///   - `"remotes:<text>"` when `blit.remotes` changes.
///   - `"roots:<text>"` when `blit.roots` changes.
///   - `"key=value"` when `blit.conf` changes.
///
/// The client may send:
///   - `"set key value"` — persist a browser setting.
///   - `"remotes-add name uri"` — add or update a remote; name must not
///     contain `=` or whitespace; uri must be non-empty.  If the entry
///     existed and was disabled, it is re-enabled.
///   - `"remotes-remove name"` — remove a remote by name (regardless of
///     enabled/disabled state).
///   - `"remotes-toggle name"` — flip a remote's disabled state.  Disabled
///     remotes are persisted as `# name = uri` and excluded from connection
///     resolution.
///   - `"remotes-set-default name"` — write `target = name` to `blit.conf`
///     (or remove the key if name is empty or `"local"`).  The updated
///     `target` value is then broadcast to all config-WS clients as a
///     normal `"target=value"` message via the config-file watcher.
///   - `"remotes-reorder name1 name2 …"` — reorder remotes to match the
///     supplied name sequence; any names not listed are appended at the end
///     in their original relative order.  Disabled state is preserved.
///   - `"roots-add name value"` / `"roots-remove name"` /
///     `"roots-toggle name"` / `"roots-reorder name1 name2 …"` — the exact
///     analogues for `blit.roots`.  `value` is an opaque `remote:path` spec.
#[allow(clippy::too_many_arguments)]
pub async fn handle_config_ws(
    mut ws: WebSocket,
    token: &AuthPassphrase,
    config: &ConfigState,
    remotes: Option<&RemotesState>,
    remotes_transform: Option<fn(&str) -> String>,
    roots: Option<&RootsState>,
    extra_init: &[String],
    auth: AuthContext<'_>,
) {
    if !authenticate_text_ws(&mut ws, token, auth.throttle, auth.peer, Some("ok")).await {
        return;
    }

    // Subscribe before reading the snapshot so we can't miss a concurrent write.
    let mut remotes_rx = remotes.map(|r| r.subscribe());
    let mut roots_rx = roots.map(|r| r.subscribe());

    // Send the current remotes snapshot (even if empty — client can rely on
    // always receiving this message after "ok").
    let remotes_text = remotes.map(|r| r.get()).unwrap_or_default();
    let remotes_text = remotes_transform
        .map(|f| f(&remotes_text))
        .unwrap_or(remotes_text);
    if ws
        .send(Message::Text(format!("remotes:{remotes_text}").into()))
        .await
        .is_err()
    {
        return;
    }

    // Send the current roots snapshot (always, even if empty).
    let roots_text = roots.map(|r| r.get()).unwrap_or_default();
    if ws
        .send(Message::Text(format!("roots:{roots_text}").into()))
        .await
        .is_err()
    {
        return;
    }

    let map = read_config();
    for (k, v) in &map {
        if ws
            .send(Message::Text(format!("{k}={v}").into()))
            .await
            .is_err()
        {
            return;
        }
    }
    for msg in extra_init {
        if ws.send(Message::Text(msg.clone().into())).await.is_err() {
            return;
        }
    }
    if ws.send(Message::Text("ready".into())).await.is_err() {
        return;
    }

    let mut config_rx = config.tx.subscribe();

    loop {
        // Build the select! arms dynamically based on whether we have a
        // destinations receiver.  We can't use an Option inside select!
        // directly, so we use a never-resolving future as a stand-in.
        tokio::select! {
            msg = ws.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let text = text.trim();
                        if let Some(rest) = text.strip_prefix("set ")
                            && let Some((k, v)) = rest.split_once(' ') {
                                let k = k.trim().replace(['\n', '\r'], "");
                                let v = v.trim().replace(['\n', '\r'], "");
                                if k.is_empty() { continue; }
                                modify_config(|map| {
                                    if v.is_empty() {
                                        map.remove(&k);
                                    } else {
                                        map.insert(k, v);
                                    }
                                });
                        } else if let Some(rest) = text.strip_prefix("remotes-add ") {
                            // "remotes-add <name> <uri>" — name is first whitespace-delimited
                            // word, uri is the remainder after a single space.
                            if let Some((raw_name, raw_uri)) = rest.split_once(' ') {
                                let name = raw_name.trim().replace(['\n', '\r'], "");
                                let uri = raw_uri.trim().replace(['\n', '\r'], "");
                                if valid_entry_name(&name)
                                    && !uri.is_empty()
                                    && let Some(r) = remotes
                                {
                                    r.modify(|entries| {
                                        if let Some(pos) = entries.iter().position(|e| e.name == name) {
                                            entries[pos].uri = uri;
                                            // An explicit add re-enables a previously
                                            // disabled entry.
                                            entries[pos].disabled = false;
                                        } else {
                                            entries.push(RemoteEntry {
                                                name,
                                                uri,
                                                disabled: false,
                                            });
                                        }
                                    });
                                }
                            }
                        } else if let Some(name) = text.strip_prefix("remotes-remove ") {
                            let name = name.trim().replace(['\n', '\r'], "");
                            if !name.is_empty()
                                && let Some(r) = remotes
                            {
                                r.modify(|entries| {
                                    entries.retain(|e| e.name != name);
                                });
                            }
                        } else if let Some(name) = text.strip_prefix("remotes-toggle ") {
                            let name = name.trim().replace(['\n', '\r'], "");
                            if !name.is_empty()
                                && let Some(r) = remotes
                            {
                                r.modify(|entries| {
                                    if let Some(pos) =
                                        entries.iter().position(|e| e.name == name)
                                    {
                                        entries[pos].disabled = !entries[pos].disabled;
                                    }
                                });
                            }
                        } else if let Some(name) = text.strip_prefix("remotes-set-default ") {
                            // Write blit.target = <name> to blit.conf (or remove it for local/empty).
                            let name = name.trim().replace(['\n', '\r'], "");
                            modify_config(|map| {
                                if name.is_empty() || name == "local" {
                                    map.remove("blit.target");
                                } else {
                                    map.insert("blit.target".into(), name);
                                }
                            });
                        } else if let Some(rest) = text.strip_prefix("remotes-reorder ") {
                            // "remotes-reorder name1 name2 …" — reorder entries to match
                            // the supplied sequence; unlisted entries are appended at end.
                            if let Some(r) = remotes {
                                let desired: Vec<String> = rest
                                    .split_whitespace()
                                    .map(|s| s.replace(['\n', '\r'], ""))
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                if !desired.is_empty() {
                                    r.modify(|entries| {
                                        let by_name: std::collections::HashMap<String, RemoteEntry> =
                                            entries
                                                .iter()
                                                .map(|e| (e.name.clone(), e.clone()))
                                                .collect();
                                        let mut reordered: Vec<RemoteEntry> = desired
                                            .iter()
                                            .filter_map(|n| by_name.get(n).cloned())
                                            .collect();
                                        let desired_set: std::collections::HashSet<&str> =
                                            desired.iter().map(|s| s.as_str()).collect();
                                        for e in entries.iter() {
                                            if !desired_set.contains(e.name.as_str()) {
                                                reordered.push(e.clone());
                                            }
                                        }
                                        *entries = reordered;
                                    });
                                }
                            }
                        } else if let Some(rest) = text.strip_prefix("roots-add ") {
                            // "roots-add <name> <value>" — value is an opaque
                            // remote:path spec (remainder after the first space).
                            if let Some((raw_name, raw_value)) = rest.split_once(' ') {
                                let name = raw_name.trim().replace(['\n', '\r'], "");
                                let value = raw_value.trim().replace(['\n', '\r'], "");
                                if valid_entry_name(&name)
                                    && !value.is_empty()
                                    && let Some(r) = roots
                                {
                                    r.modify(|entries| {
                                        if let Some(pos) =
                                            entries.iter().position(|e| e.name == name)
                                        {
                                            entries[pos].value = value;
                                            entries[pos].disabled = false;
                                        } else {
                                            entries.push(RootEntry {
                                                name,
                                                value,
                                                disabled: false,
                                            });
                                        }
                                    });
                                }
                            }
                        } else if let Some(name) = text.strip_prefix("roots-remove ") {
                            let name = name.trim().replace(['\n', '\r'], "");
                            if !name.is_empty()
                                && let Some(r) = roots
                            {
                                r.modify(|entries| {
                                    entries.retain(|e| e.name != name);
                                });
                            }
                        } else if let Some(name) = text.strip_prefix("roots-toggle ") {
                            let name = name.trim().replace(['\n', '\r'], "");
                            if !name.is_empty()
                                && let Some(r) = roots
                            {
                                r.modify(|entries| {
                                    if let Some(pos) =
                                        entries.iter().position(|e| e.name == name)
                                    {
                                        entries[pos].disabled = !entries[pos].disabled;
                                    }
                                });
                            }
                        } else if let Some(rest) = text.strip_prefix("roots-reorder ") {
                            // "roots-reorder name1 name2 …" — reorder to match the
                            // sequence; unlisted entries are appended at the end.
                            if let Some(r) = roots {
                                let desired: Vec<String> = rest
                                    .split_whitespace()
                                    .map(|s| s.replace(['\n', '\r'], ""))
                                    .filter(|s| !s.is_empty())
                                    .collect();
                                if !desired.is_empty() {
                                    r.modify(|entries| {
                                        let by_name: std::collections::HashMap<String, RootEntry> =
                                            entries
                                                .iter()
                                                .map(|e| (e.name.clone(), e.clone()))
                                                .collect();
                                        let mut reordered: Vec<RootEntry> = desired
                                            .iter()
                                            .filter_map(|n| by_name.get(n).cloned())
                                            .collect();
                                        let desired_set: std::collections::HashSet<&str> =
                                            desired.iter().map(|s| s.as_str()).collect();
                                        for e in entries.iter() {
                                            if !desired_set.contains(e.name.as_str()) {
                                                reordered.push(e.clone());
                                            }
                                        }
                                        *entries = reordered;
                                    });
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(_)) => break,
                    _ => continue,
                }
            }
            broadcast = config_rx.recv() => {
                match broadcast {
                    Ok(line) => {
                        if ws.send(Message::Text(line.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            remotes_update = async {
                match remotes_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match remotes_update {
                    Ok(text) => {
                        let text = remotes_transform
                            .map(|f| f(&text))
                            .unwrap_or(text);
                        if ws
                            .send(Message::Text(format!("remotes:{text}").into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Missed some intermediate updates — send current snapshot.
                        if let Some(r) = remotes {
                            let text = r.get();
                            let text = remotes_transform
                                .map(|f| f(&text))
                                .unwrap_or(text);
                            if ws
                                .send(Message::Text(format!("remotes:{text}").into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            roots_update = async {
                match roots_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match roots_update {
                    Ok(text) => {
                        if ws
                            .send(Message::Text(format!("roots:{text}").into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Missed some updates — send the current snapshot.
                        if let Some(r) = roots
                            && ws
                                .send(Message::Text(format!("roots:{}", r.get()).into()))
                                .await
                                .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_config_str ──

    #[test]
    fn parse_empty_string() {
        let map = parse_config_str("");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_comments_and_blanks() {
        let map = parse_config_str("# comment\n\n  # another\n");
        assert!(map.is_empty());
    }

    #[test]
    fn parse_key_value() {
        let map = parse_config_str("font = Menlo\ntheme = dark\n");
        assert_eq!(map.get("font").unwrap(), "Menlo");
        assert_eq!(map.get("theme").unwrap(), "dark");
    }

    #[test]
    fn parse_trims_whitespace() {
        let map = parse_config_str("  key  =  value  ");
        assert_eq!(map.get("key").unwrap(), "value");
    }

    #[test]
    fn parse_line_without_equals() {
        let map = parse_config_str("no-equals-here\nkey=val");
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("key").unwrap(), "val");
    }

    #[test]
    fn parse_equals_in_value() {
        let map = parse_config_str("cmd = a=b=c");
        assert_eq!(map.get("cmd").unwrap(), "a=b=c");
    }

    #[test]
    fn parse_duplicate_keys_last_wins() {
        let map = parse_config_str("key = first\nkey = second");
        assert_eq!(map.get("key").unwrap(), "second");
    }

    #[test]
    fn parse_mixed_content() {
        let input = "# header\nfont = FiraCode\n\n# size\nsize = 14\ntheme=light";
        let map = parse_config_str(input);
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("font").unwrap(), "FiraCode");
        assert_eq!(map.get("size").unwrap(), "14");
        assert_eq!(map.get("theme").unwrap(), "light");
    }

    // ── write_config round-trip ──

    #[test]
    fn serialize_config_produces_sorted_output() {
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("z".into(), "last".into());
        map.insert("a".into(), "first".into());
        let output = serialize_config_str(&map);
        assert!(output.starts_with("a = first"));
        assert!(output.contains("z = last"));
    }

    #[test]
    fn round_trip_parse_serialize() {
        let input = "alpha = 1\nbeta = 2\ngamma = 3";
        let map = parse_config_str(input);
        let serialized = serialize_config_str(&map);
        let reparsed = parse_config_str(&serialized);
        assert_eq!(map, reparsed);
    }

    // ── RemotesState mutations (remotes-add / remotes-remove) ──

    fn entry(name: &str, uri: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            uri: uri.to_string(),
            disabled: false,
        }
    }

    #[test]
    fn remotes_add_new_entry() {
        let state = RemotesState::ephemeral(String::new());
        let mut entries = parse_remotes_full(&state.get());
        entries.push(entry("rabbit", "ssh:rabbit"));
        state.set(&entries);
        let got = parse_remotes_str(&state.get());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], ("rabbit".to_string(), "ssh:rabbit".to_string()));
    }

    #[test]
    fn remotes_add_updates_existing() {
        let initial = "rabbit = ssh:rabbit\n";
        let state = RemotesState::ephemeral(initial.to_string());
        let mut entries = parse_remotes_full(&state.get());
        if let Some(pos) = entries.iter().position(|e| e.name == "rabbit") {
            entries[pos].uri = "tcp:rabbit:3264".to_string();
        }
        state.set(&entries);
        let got = parse_remotes_str(&state.get());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "tcp:rabbit:3264");
    }

    #[test]
    fn remotes_remove_existing() {
        let initial = "rabbit = ssh:rabbit\nhound = ssh:hound\n";
        let state = RemotesState::ephemeral(initial.to_string());
        let mut entries = parse_remotes_full(&state.get());
        entries.retain(|e| e.name != "rabbit");
        state.set(&entries);
        let got = parse_remotes_str(&state.get());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "hound");
    }

    #[test]
    fn remotes_remove_nonexistent_is_noop() {
        let initial = "rabbit = ssh:rabbit\n";
        let state = RemotesState::ephemeral(initial.to_string());
        let mut entries = parse_remotes_full(&state.get());
        let before = entries.len();
        entries.retain(|e| e.name != "does-not-exist");
        assert_eq!(entries.len(), before);
    }

    // ── Disabled remotes (commented) ──

    #[test]
    fn parse_disabled_entry() {
        let entries = parse_remotes_full("# rabbit = ssh:rabbit\nhound = ssh:hound\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "rabbit");
        assert_eq!(entries[0].uri, "ssh:rabbit");
        assert!(entries[0].disabled);
        assert_eq!(entries[1].name, "hound");
        assert!(!entries[1].disabled);
    }

    #[test]
    fn parse_disabled_no_space_after_hash() {
        let entries = parse_remotes_full("#rabbit = ssh:rabbit\n");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].disabled);
    }

    #[test]
    fn parse_remotes_str_filters_disabled() {
        let active = parse_remotes_str("# rabbit = ssh:rabbit\nhound = ssh:hound\n");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, "hound");
    }

    #[test]
    fn parse_skips_pure_comments() {
        let entries = parse_remotes_full("# This is just a header\n# also a comment\n");
        assert!(entries.is_empty());
    }

    #[test]
    fn round_trip_disabled() {
        let initial = "rabbit = ssh:rabbit\n# hound = ssh:hound\n";
        let entries = parse_remotes_full(initial);
        let serialized = serialize_remotes(&entries);
        let reparsed = parse_remotes_full(&serialized);
        assert_eq!(entries, reparsed);
        assert!(serialized.contains("# hound = ssh:hound"));
    }

    #[test]
    fn remotes_toggle_flips_state() {
        let state = RemotesState::ephemeral("rabbit = ssh:rabbit\n".into());
        state.modify(|entries| {
            if let Some(pos) = entries.iter().position(|e| e.name == "rabbit") {
                entries[pos].disabled = !entries[pos].disabled;
            }
        });
        let entries = parse_remotes_full(&state.get());
        assert_eq!(entries.len(), 1);
        assert!(entries[0].disabled);
        // Active view excludes it.
        assert!(parse_remotes_str(&state.get()).is_empty());
    }

    #[test]
    fn remotes_add_reenables_disabled() {
        let state = RemotesState::ephemeral("# rabbit = ssh:old\n".into());
        // Simulate the WS handler's add logic.
        state.modify(|entries| {
            let name = "rabbit".to_string();
            if let Some(pos) = entries.iter().position(|e| e.name == name) {
                entries[pos].uri = "ssh:new".to_string();
                entries[pos].disabled = false;
            } else {
                entries.push(RemoteEntry {
                    name,
                    uri: "ssh:new".to_string(),
                    disabled: false,
                });
            }
        });
        let entries = parse_remotes_full(&state.get());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].uri, "ssh:new");
        assert!(!entries[0].disabled);
    }

    #[test]
    fn remotes_reorder_preserves_disabled() {
        let initial = "alpha = a\n# beta = b\ngamma = c\n";
        let entries = parse_remotes_full(initial);
        // Reorder alpha → gamma → beta.
        let desired = ["gamma", "alpha", "beta"];
        let by_name: std::collections::HashMap<String, RemoteEntry> = entries
            .iter()
            .map(|e| (e.name.clone(), e.clone()))
            .collect();
        let reordered: Vec<RemoteEntry> = desired
            .iter()
            .filter_map(|n| by_name.get(*n).cloned())
            .collect();
        let serialized = serialize_remotes(&reordered);
        let reparsed = parse_remotes_full(&serialized);
        assert_eq!(reparsed.len(), 3);
        assert_eq!(reparsed[0].name, "gamma");
        assert!(!reparsed[0].disabled);
        assert_eq!(reparsed[2].name, "beta");
        assert!(reparsed[2].disabled);
    }

    /// The rule every writer and the parser now share. These used to be four
    /// separate conditions that had drifted — and the tests here asserted the
    /// condition inline rather than calling anything, so they passed whatever
    /// the code did.
    #[test]
    fn entry_names_must_survive_both_formats() {
        for ok in ["rabbit", "prod-1", "a", "héllo", "x.y_z:1"] {
            assert!(valid_entry_name(ok), "{ok:?} should be usable");
        }
        for bad in [
            "",           // nothing to name
            "foo=bar",    // reparses as name "foo", value "bar"
            "#foo",       // reparses as a disabled entry
            "my remote",  // splits the space-delimited add verb
            "my\tremote", // ditto, and survives split_once(' ')
            "my\nremote", // splits the line
            " lead",
            "trail ",
        ] {
            assert!(!valid_entry_name(bad), "{bad:?} should be refused");
        }
    }

    /// The parser is the backstop for a hand-edited file: a name it would
    /// refuse must not come back as an entry.
    #[test]
    fn parser_drops_names_it_could_not_write() {
        let parsed =
            parse_remotes_full("good = ssh:host\nmy remote = ssh:other\n##bad = ssh:third\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "good");
    }

    // ── set-default writes blit.target key to blit.conf ──

    #[test]
    fn set_default_inserts_target_key() {
        let mut map = parse_config_str("font = Mono\n");
        map.insert("blit.target".into(), "rabbit".into());
        let serialized = serialize_config_str(&map);
        let reparsed = parse_config_str(&serialized);
        assert_eq!(
            reparsed.get("blit.target").map(|s| s.as_str()),
            Some("rabbit")
        );
        assert_eq!(reparsed.get("font").map(|s| s.as_str()), Some("Mono"));
    }

    #[test]
    fn set_default_local_removes_target_key() {
        let mut map = parse_config_str("blit.target = rabbit\nfont = Mono\n");
        // "local" or empty → remove the key
        map.remove("blit.target");
        let serialized = serialize_config_str(&map);
        let reparsed = parse_config_str(&serialized);
        assert!(!reparsed.contains_key("blit.target"));
        assert_eq!(reparsed.get("font").map(|s| s.as_str()), Some("Mono"));
    }
    #[test]
    fn auth_throttle_limits_concurrent_unauthenticated_attempts() {
        let throttle =
            AuthThrottle::with_limits(1, 5, Duration::from_secs(60), Duration::from_secs(60));
        let first = throttle.begin("peer").expect("first attempt allowed");
        assert!(throttle.begin("other").is_none());
        drop(first);
        assert!(throttle.begin("other").is_some());
    }

    #[test]
    fn auth_throttle_locks_out_repeated_failures_and_clears_on_success() {
        let throttle =
            AuthThrottle::with_limits(4, 2, Duration::from_secs(60), Duration::from_secs(60));
        throttle.begin("peer").unwrap().record_failure();
        let success = throttle.begin("peer").expect("not locked before threshold");
        success.record_success();
        throttle.begin("peer").unwrap().record_failure();
        assert!(
            throttle.begin("peer").is_some(),
            "success reset failure count"
        );
        throttle.begin("bad").unwrap().record_failure();
        throttle.begin("bad").unwrap().record_failure();
        assert!(throttle.begin("bad").is_none(), "bad peer is locked out");
        assert!(throttle.begin("other").is_some(), "lockout is per peer");
    }

    /// An abandoned handshake — a page navigation, a suspended tab, a client
    /// dropping a WebTransport probe to fall back to WebSocket — used to be
    /// charged as a failed authentication. Enough of them locked out a user who
    /// never typed a wrong passphrase, and the lockout then answered with the
    /// same "auth" the UI takes as "discard your stored passphrase".
    #[test]
    fn auth_throttle_ignores_handshakes_that_never_presented_a_passphrase() {
        let throttle =
            AuthThrottle::with_limits(32, 3, Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..10 {
            drop(throttle.begin("peer").expect("abandoned attempt allowed"));
        }
        assert!(
            throttle.begin("peer").is_some(),
            "abandoned handshakes must not count towards the failure budget"
        );
    }

    #[test]
    fn auth_throttle_releases_a_slot_exactly_once() {
        let throttle =
            AuthThrottle::with_limits(1, 5, Duration::from_secs(60), Duration::from_secs(60));
        // record_failure() releases, and the subsequent Drop must not release a
        // second time — a double decrement would let the cap drift upwards.
        throttle.begin("peer").unwrap().record_failure();
        let held = throttle.begin("other").expect("slot freed once");
        assert!(throttle.begin("third").is_none(), "cap still holds at one");
        drop(held);
    }

    // ── blit.roots (RootsState) ──

    #[test]
    fn roots_parse_and_round_trip_disabled() {
        let initial = "app = local:/home/me/app\n# prod = server:/srv/app\n";
        let entries = parse_roots_full(initial);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "app");
        assert_eq!(entries[0].value, "local:/home/me/app");
        assert!(!entries[0].disabled);
        assert_eq!(entries[1].name, "prod");
        assert!(entries[1].disabled);
        let serialized = serialize_roots(&entries);
        assert_eq!(parse_roots_full(&serialized), entries);
        assert!(serialized.contains("# prod = server:/srv/app"));
    }

    #[test]
    fn roots_add_new_and_update_existing() {
        let state = RootsState::ephemeral(String::new());
        // Add new.
        state.modify(|entries| {
            entries.push(RootEntry {
                name: "app".into(),
                value: "local:/a".into(),
                disabled: false,
            });
        });
        // Update existing (WS add semantics: retarget + re-enable).
        state.modify(|entries| {
            let name = "app".to_string();
            if let Some(pos) = entries.iter().position(|e| e.name == name) {
                entries[pos].value = "local:/b".into();
                entries[pos].disabled = false;
            } else {
                entries.push(RootEntry {
                    name,
                    value: "local:/b".into(),
                    disabled: false,
                });
            }
        });
        let entries = parse_roots_full(&state.get());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].value, "local:/b");
    }

    #[test]
    fn roots_toggle_and_remove() {
        let state = RootsState::ephemeral("app = local:/a\nlib = local:/b\n".into());
        state.modify(|entries| {
            if let Some(pos) = entries.iter().position(|e| e.name == "app") {
                entries[pos].disabled = !entries[pos].disabled;
            }
        });
        state.modify(|entries| entries.retain(|e| e.name != "lib"));
        let entries = parse_roots_full(&state.get());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "app");
        assert!(entries[0].disabled);
    }

    #[test]
    fn roots_empty_when_absent() {
        // An absent file must not auto-provision (unlike blit.remotes).
        let state = RootsState::ephemeral(String::new());
        assert!(parse_roots_full(&state.get()).is_empty());
    }
}
