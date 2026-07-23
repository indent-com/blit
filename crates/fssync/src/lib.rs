//! Filesystem state sync engine (docs/fs-watch.md).
//!
//! The server side of `FEATURE_FS`, split in two:
//!
//! - A **shared root** per watched `(path, recursive, cross_filesystem)`,
//!   refcounted across every sync of that root on every connection: one
//!   native watcher, one hint-driven reconciler owning the canonical
//!   metadata index, publishing immutable `Arc<Index>` snapshots.
//! - A **per-sync engine** holding only client state: the shadow snapshot
//!   (what the client holds), the held-content map for delta bases, the
//!   ack window, and staged `RESET … SYNC` update assembly.
//!
//! Content flows through the process-wide content-addressed blob store:
//! once any sync reads and hashes a file, the reconciler adopts the hash
//! and every other sync serves those bytes from memory. Native backends
//! deliver *hints* (a path may have changed / rescan everything); all
//! protocol-visible behavior lives here, so the three platforms behave
//! identically by construction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use std::{fs, io};

use blit_remote::fs::{
    FS_CLOSED_CLIENT_REQUEST, FS_CLOSED_RESOURCE_LIMIT, FS_CLOSED_ROOT_GONE, FS_DONE_CONFLICT,
    FS_DONE_INVALID, FS_DONE_NOT_FOUND, FS_DONE_OK, FS_DONE_OTHER, FS_DONE_PERMISSION,
    FS_DONE_TOO_LARGE, FS_DONE_WRONG_TYPE, FS_ENTRY_DIR, FS_ENTRY_FILE, FS_ENTRY_NO_CONTENT,
    FS_ENTRY_OTHER, FS_ENTRY_SYMLINK, FS_ENTRY_TYPE_MASK, FS_ENTRY_UNREADABLE, FS_ENTRY_UNSTABLE,
    FS_FILE_NOT_FOUND, FS_FILE_OK, FS_FILE_UNREADABLE, FS_OP_HARDLINK, FS_OP_MKDIR,
    FS_OP_MKPARENTS, FS_OP_NO_CAS, FS_OP_REMOVE, FS_OP_RENAME, FS_OP_SYMLINK, FS_UPDATE_RESET,
    FS_UPDATE_SYNC, FS_WRITE_DURABLE, FS_WRITE_FOLLOW_SYMLINK, FS_WRITE_MKPARENTS, FS_WRITE_NO_CAS,
    FsContent, FsRecord, append_fs_record, msg_fs_closed, msg_fs_done, msg_fs_file, msg_fs_update,
};

pub mod backend;

// ---------------------------------------------------------------------------
// Options and handles
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SyncOptions {
    pub recursive: bool,
    pub content: bool,
    pub cross_filesystem: bool,
    /// Settle/batching window.
    pub latency: Duration,
    /// Per-file inline content cap in bytes.
    pub inline_max: u64,
    /// Unacknowledged-byte credit window.
    pub window_bytes: usize,
    /// Uncompressed records target per update.
    pub batch_target: usize,
    /// Hard cap on indexed entries.
    pub max_entries: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            content: false,
            cross_filesystem: false,
            latency: env_ms("BLIT_FS_LATENCY_MS", 20),
            inline_max: env_u64("BLIT_FS_INLINE_MAX", 16 * 1024 * 1024),
            window_bytes: env_u64("BLIT_FS_WINDOW", 1024 * 1024) as usize,
            batch_target: 64 * 1024,
            max_entries: env_u64("BLIT_FS_MAX_ENTRIES", 1_000_000) as usize,
        }
    }
}

fn env_ms(name: &str, default: u64) -> Duration {
    Duration::from_millis(env_u64(name, default).clamp(1, 1000))
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A hint from a native backend. Hints are unreliable and duplicated; the
/// reconciler verifies everything against the filesystem before emitting.
#[derive(Clone, Debug)]
pub enum Hint {
    /// Something at or under this absolute path may have changed.
    Dirty(PathBuf),
    /// Events may have been lost; re-verify the whole tree.
    Rescan,
}

/// Per-connection in-flight write accounting. The server inserts a
/// request's nonce before dispatch — rejecting a duplicate (`INVALID`) or
/// an over-cap request (`BUDGET`) — and attaches this guard to the request;
/// the engine drops it once the request is answered, removing the nonce and
/// freeing a slot. Bounds the otherwise-unbounded engine channel depth (and
/// thus resident inbound content) to the in-flight cap.
#[derive(Debug)]
pub struct InflightGuard {
    set: Arc<Mutex<std::collections::HashSet<u16>>>,
    nonce: u16,
}

impl InflightGuard {
    pub fn new(set: Arc<Mutex<std::collections::HashSet<u16>>>, nonce: u16) -> Self {
        InflightGuard { set, nonce }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.set.lock() {
            set.remove(&self.nonce);
        }
    }
}

/// A content write forwarded to the engine (docs/design/fs-write.md).
/// `path` is the escaped wire path; `flags` are `FS_WRITE_*`.
#[derive(Clone, Debug)]
pub struct WriteReq {
    pub nonce: u16,
    pub path: String,
    pub base: u128,
    pub mode: u32,
    pub flags: u8,
    pub content_kind: u8,
    pub content: Vec<u8>,
    /// Freed (nonce slot released) when this request is dropped after the
    /// engine answers it. `None` in tests and embedders without accounting.
    pub inflight: Option<Arc<InflightGuard>>,
}

/// A metadata op forwarded to the engine. `op` is `FS_OP_*`; `a`/`b` are
/// escaped wire paths (`b` empty except for `RENAME`).
#[derive(Clone, Debug)]
pub struct OpReq {
    pub nonce: u16,
    pub op: u8,
    pub a: String,
    pub b: String,
    pub base: u128,
    pub mode: u32,
    pub flags: u8,
    pub inflight: Option<Arc<InflightGuard>>,
}

/// Commands forwarded from the client connection.
#[derive(Clone, Debug)]
pub enum Command {
    Ack(u32),
    Fetch { nonce: u16, path: String },
    Write(WriteReq),
    Op(OpReq),
    Stop,
}

/// Registration interface a backend exposes to the reconciler so newly
/// created directories can be watched (inotify). FSEvents/RDCW backends are
/// naturally recursive and use the no-op default.
pub trait BackendHandle: Send {
    fn add_dir(&self, _dir: &Path) {}
}

pub struct NoopBackend;
impl BackendHandle for NoopBackend {}

// ---------------------------------------------------------------------------
// Shared roots: one native watcher + one canonical index per watched root,
// shared by every sync of that root across all connections.
// ---------------------------------------------------------------------------

/// Identity of a shared root. Enumeration scope is part of the identity:
/// recursive and non-recursive syncs of the same directory index different
/// trees and cannot share a reconciler.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootKey {
    /// Canonical root path (see [`validate_root`]).
    pub path: PathBuf,
    pub recursive: bool,
    pub cross_filesystem: bool,
}

/// Reconciler inbox.
enum RootMsg {
    Hint(Hint),
    Subscribe {
        id: u64,
        tx: Sender<SyncMsg>,
        latency: Duration,
    },
    Unsubscribe {
        id: u64,
    },
    /// An engine read and hashed a file's content; the reconciler adopts
    /// the hash if the stat still matches, so other syncs can serve the
    /// bytes straight from the blob store.
    HashLearned {
        path: String,
        meta: NodeMeta,
    },
}

/// What the reconciler publishes to subscribed engines.
enum RootUpdate {
    /// A new immutable snapshot of the canonical index. `settled` is when
    /// the reconciler's batch began settling, so the engine can honor the
    /// requested window without adding a second one on top (the reconciler
    /// already waited `latency`). `None` = already settled, emit at once.
    Snapshot {
        index: Arc<Index>,
        settled: Option<Instant>,
    },
    /// The root is gone or over budget; the sync must close with `reason`.
    Closed(u8),
}

/// Per-sync engine inbox.
enum SyncMsg {
    Cmd(Command),
    Root(RootUpdate),
}

/// A shared root: keeps the native watcher armed and the reconciler
/// reachable. Engines hold an `Arc`; when the last one drops, the watcher
/// disarms, the reconciler's inbox disconnects, and its thread exits.
pub struct SharedRootHandle {
    key: RootKey,
    tx: Sender<RootMsg>,
    /// Set to the close reason once the reconciler shuts the root down
    /// (root gone, permission lost, resource limit). A closed root is dead
    /// forever; a later `open_root` of the same key must not join it.
    closed: Arc<OnceLock<u8>>,
    /// Keeps the native watch alive for the root's lifetime.
    _backend: Mutex<Option<backend::WatchBackend>>,
}

impl SharedRootHandle {
    pub fn key(&self) -> &RootKey {
        &self.key
    }

    /// A hint sender for tests and embedders with their own change source.
    pub fn hint_sender(&self) -> HintSender {
        HintSender {
            tx: self.tx.clone(),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.get().is_some()
    }
}

type Registry = std::collections::HashMap<RootKey, std::sync::Weak<SharedRootHandle>>;

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Open (or join) the shared root for `key`, arming a native watcher on
/// first open — before the initial enumeration, so nothing slips between
/// scan and event delivery. On failure returns an `FS_STATUS_*` code plus
/// diagnostic, so the server can answer `FS_SYNCED` accurately.
pub fn open_root(key: RootKey) -> Result<Arc<SharedRootHandle>, (u8, String)> {
    open_root_inner(key, true)
}

/// Open (or join) a shared root without a native watcher; hints come from
/// [`SharedRootHandle::hint_sender`]. For tests and embedders.
pub fn open_root_unwatched(key: RootKey) -> Arc<SharedRootHandle> {
    open_root_inner(key, false).expect("unwatched open cannot fail")
}

/// Map a native-watch arming failure to an `FS_STATUS_*` code.
fn watch_error_status(err: &notify::Error) -> u8 {
    use blit_remote::fs::{
        FS_STATUS_NOT_FOUND, FS_STATUS_OTHER, FS_STATUS_PERMISSION_DENIED, FS_STATUS_RESOURCE_LIMIT,
    };
    match &err.kind {
        notify::ErrorKind::MaxFilesWatch => FS_STATUS_RESOURCE_LIMIT,
        notify::ErrorKind::PathNotFound => FS_STATUS_NOT_FOUND,
        notify::ErrorKind::Io(e) => match e.raw_os_error() {
            // ENFILE / EMFILE / ENOSPC — descriptor or watch exhaustion.
            Some(23) | Some(24) | Some(28) => FS_STATUS_RESOURCE_LIMIT,
            _ => match e.kind() {
                io::ErrorKind::PermissionDenied => FS_STATUS_PERMISSION_DENIED,
                io::ErrorKind::NotFound => FS_STATUS_NOT_FOUND,
                _ => FS_STATUS_OTHER,
            },
        },
        _ => FS_STATUS_OTHER,
    }
}

fn open_root_inner(key: RootKey, watched: bool) -> Result<Arc<SharedRootHandle>, (u8, String)> {
    // Join an existing live, open root under the lock.
    {
        let mut map = registry().lock().unwrap();
        map.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = map
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
            .filter(|h| !h.is_closed())
        {
            return Ok(existing);
        }
    }
    // Arm the native watcher *outside* the registry lock: `inotify_add_watch`
    // / FSEvents stream creation can be slow, and holding the global lock
    // across it would serialize every connection opening any root. Arming
    // before the reconciler spawns preserves the arm-before-scan contract.
    let (tx, rx) = mpsc::channel();
    let backend = if watched {
        let hints = HintSender { tx: tx.clone() };
        Some(
            backend::watch(&key.path, key.recursive, hints)
                .map_err(|e| (watch_error_status(&e), e.to_string()))?,
        )
    } else {
        None
    };
    let mut map = registry().lock().unwrap();
    map.retain(|_, weak| weak.strong_count() > 0);
    // Another thread may have created (and armed) the same root while we
    // were arming; prefer theirs and drop our now-redundant watcher.
    if let Some(existing) = map
        .get(&key)
        .and_then(std::sync::Weak::upgrade)
        .filter(|h| !h.is_closed())
    {
        return Ok(existing);
    }
    let closed: Arc<OnceLock<u8>> = Arc::new(OnceLock::new());
    let handle = Arc::new(SharedRootHandle {
        key: key.clone(),
        tx,
        closed: closed.clone(),
        _backend: Mutex::new(backend),
    });
    let reconciler_key = key.clone();
    std::thread::Builder::new()
        .name("blit-fsroot".into())
        .spawn(move || Reconciler::new(reconciler_key, rx, closed).run())
        .expect("spawn fssync reconciler");
    map.insert(key, Arc::downgrade(&handle));
    Ok(handle)
}

/// Handle owned by the client connection. Dropping it stops the engine
/// (and, transitively, releases its share of the root).
pub struct SyncHandle {
    tx: Sender<SyncMsg>,
    /// Set once the engine thread has exited (client gone, stopped, or an
    /// engine-initiated `FS_CLOSED`). Lets the server reap dead entries
    /// whose id it never saw a `FS_STOP` for.
    done: Arc<std::sync::atomic::AtomicBool>,
}

impl SyncHandle {
    pub fn command(&self, cmd: Command) -> bool {
        self.tx.send(SyncMsg::Cmd(cmd)).is_ok()
    }

    /// True once the engine thread has exited. The `FS_CLOSED` it may have
    /// emitted is already in the FIFO outbox before this flips, so reaping
    /// after observing `true` can never reorder it against a reused id.
    pub fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(SyncMsg::Cmd(Command::Stop));
    }
}

/// Wrap the reconciler inbox for a hint source (native backend or test).
#[derive(Clone)]
pub struct HintSender {
    tx: Sender<RootMsg>,
}

impl HintSender {
    pub fn send(&self, hint: Hint) -> bool {
        self.tx.send(RootMsg::Hint(hint)).is_ok()
    }
}

/// Messages the engine emits, ready for the client outbox. Returns `false`
/// when the client is gone; the engine then exits.
pub type Outbox = Box<dyn FnMut(Vec<u8>) -> bool + Send>;

/// Validate and canonicalize a requested root. Returns the canonical path
/// or an `FS_STATUS_*` code plus diagnostic.
pub fn validate_root(path: &str) -> Result<PathBuf, (u8, String)> {
    use blit_remote::fs::{FS_STATUS_NOT_FOUND, FS_STATUS_OTHER, FS_STATUS_PERMISSION_DENIED};
    if path.is_empty() || path.contains('\0') {
        return Err((FS_STATUS_OTHER, "invalid path".into()));
    }
    match fs::canonicalize(path) {
        Ok(p) => Ok(p),
        Err(e) => {
            let status = match e.kind() {
                io::ErrorKind::NotFound => FS_STATUS_NOT_FOUND,
                io::ErrorKind::PermissionDenied => FS_STATUS_PERMISSION_DENIED,
                _ => FS_STATUS_OTHER,
            };
            Err((status, e.to_string()))
        }
    }
}

/// Spawn a sync engine subscribed to `shared`, streaming to `outbox`.
/// The engine's initial `RESET … SYNC` series is cut from the root's
/// current snapshot — later syncs of an already-watched root never rescan.
pub fn start_sync(
    shared: &Arc<SharedRootHandle>,
    sync_id: u16,
    opts: SyncOptions,
    outbox: Outbox,
) -> SyncHandle {
    static SUB_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let sub_id = SUB_IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = mpsc::channel();
    let _ = shared.tx.send(RootMsg::Subscribe {
        id: sub_id,
        tx: tx.clone(),
        latency: opts.latency,
    });
    let engine = SyncEngine::new(sync_id, shared.clone(), sub_id, opts, rx, outbox);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_thread = done.clone();
    std::thread::Builder::new()
        .name(format!("blit-fssync-{sync_id}"))
        .spawn(move || {
            engine.run();
            // run() has already queued any FS_CLOSED into the outbox FIFO.
            done_thread.store(true, std::sync::atomic::Ordering::Release);
        })
        .expect("spawn fssync engine");
    SyncHandle { tx, done }
}

// ---------------------------------------------------------------------------
// Path escaping: every wire path is valid UTF-8; non-UTF-8 bytes become %XX,
// literal '%' becomes %25. Deterministic and reversible.
// ---------------------------------------------------------------------------

pub fn escape_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    loop {
        match std::str::from_utf8(rest) {
            Ok(s) => {
                push_escaping_percent(&mut out, s);
                return out;
            }
            Err(e) => {
                let (valid, after) = rest.split_at(e.valid_up_to());
                push_escaping_percent(&mut out, unsafe { std::str::from_utf8_unchecked(valid) });
                let bad = e.error_len().unwrap_or(after.len());
                for &b in &after[..bad] {
                    out.push_str(&format!("%{b:02X}"));
                }
                rest = &after[bad..];
            }
        }
    }
}

fn push_escaping_percent(out: &mut String, s: &str) {
    for ch in s.chars() {
        if ch == '%' {
            out.push_str("%25");
        } else {
            out.push(ch);
        }
    }
}

/// Reverse [`escape_bytes`]. Returns `None` on malformed escapes.
pub fn unescape_to_bytes(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Escape UTF-16 code units (Windows names): valid text passes through
/// (`%` → `%25`), unpaired surrogates become `%uXXXX`. A literal `%u` in a
/// name escapes to `%25u`, so the forms never collide. Pure so every host
/// can test it; `cfg(windows)` wires it to `OsStr`.
pub fn escape_wide(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok('%') => out.push_str("%25"),
            Ok(c) => out.push(c),
            Err(e) => {
                out.push_str(&format!("%u{:04X}", e.unpaired_surrogate()));
            }
        }
    }
    out
}

/// Reverse [`escape_wide`]: `%uXXXX` → one code unit, `%XX` → one unit
/// below 0x100 (covers `%25`), everything else re-encoded as UTF-16.
pub fn unescape_to_wide(s: &str) -> Option<Vec<u16>> {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if bytes.get(i + 1) == Some(&b'u') {
                out.push(u16::from_str_radix(s.get(i + 2..i + 6)?, 16).ok()?);
                i += 6;
            } else {
                out.push(u16::from(
                    u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?,
                ));
                i += 3;
            }
        } else {
            let c = s[i..].chars().next()?;
            let mut buf = [0u16; 2];
            out.extend_from_slice(c.encode_utf16(&mut buf));
            i += c.len_utf8();
        }
    }
    Some(out)
}

/// Escape a whole path for wire use (e.g. the `FS_SYNCED` canonical-root
/// detail): same scheme as components, separators left intact.
#[cfg(unix)]
pub fn escape_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    escape_bytes(path.as_os_str().as_bytes())
}

#[cfg(windows)]
pub fn escape_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    escape_wide(&path.as_os_str().encode_wide().collect::<Vec<_>>())
}

#[cfg(all(not(unix), not(windows)))]
pub fn escape_path(path: &Path) -> String {
    escape_bytes(path.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn os_to_wire(name: &std::ffi::OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;
    escape_bytes(name.as_bytes())
}

#[cfg(windows)]
fn os_to_wire(name: &std::ffi::OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;
    escape_wide(&name.encode_wide().collect::<Vec<_>>())
}

#[cfg(all(not(unix), not(windows)))]
fn os_to_wire(name: &std::ffi::OsStr) -> String {
    escape_bytes(name.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn wire_to_os(component: &str) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;
    Some(std::ffi::OsString::from_vec(unescape_to_bytes(component)?))
}

#[cfg(windows)]
fn wire_to_os(component: &str) -> Option<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    Some(std::ffi::OsString::from_wide(&unescape_to_wide(component)?))
}

#[cfg(all(not(unix), not(windows)))]
fn wire_to_os(component: &str) -> Option<std::ffi::OsString> {
    Some(
        String::from_utf8(unescape_to_bytes(component)?)
            .ok()?
            .into(),
    )
}

/// Resolve a wire path (relative, '/'-separated, escaped) against a root.
/// Rejects traversal — the result always stays under the root.
pub fn resolve_wire_path(root: &Path, wire: &str) -> Option<PathBuf> {
    use std::path::Component;
    let mut abs = root.to_path_buf();
    if wire.is_empty() {
        return Some(abs);
    }
    for component in wire.split('/') {
        // Validate the *decoded* component, not the escaped wire text:
        // `%2E%2E` decodes to `..` and `%2F` to `/`, so a check on the
        // escaped form (`component == ".."`) is bypassable and would let
        // a crafted request climb out of the root. Decode first, then
        // require exactly one normal path component — rejecting empty,
        // `.`, `..`, absolute/prefix pieces, and any embedded separator.
        let os = wire_to_os(component)?;
        let mut parts = Path::new(&os).components();
        match (parts.next(), parts.next()) {
            (Some(Component::Normal(part)), None) if part == os.as_os_str() => abs.push(part),
            _ => return None,
        }
    }
    Some(abs)
}

fn join_wire(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}/{child}")
    }
}

// ---------------------------------------------------------------------------
// Metadata index
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMeta {
    /// Node type in `FS_ENTRY_TYPE_MASK` bits (flags added at send time).
    pub node_type: u8,
    pub size: u64,
    pub mtime_ns: u64,
    pub mode: u32,
    /// BLAKE3-128 of content; 0 until the file has been read.
    pub hash: u128,
    /// File identity used for move detection; (0, 0) when unavailable.
    pub dev_ino: (u64, u64),
}

impl NodeMeta {
    fn same_identity(&self, other: &NodeMeta) -> bool {
        self.node_type == other.node_type && self.dev_ino != (0, 0) && self.dev_ino == other.dev_ino
    }

    fn content_changed(&self, prev: &NodeMeta) -> bool {
        self.node_type != prev.node_type
            || self.size != prev.size
            || self.mtime_ns != prev.mtime_ns
            || self.dev_ino != prev.dev_ino
    }

    /// Equality for diffing: everything except `hash`, which is a lazily
    /// learned annotation — a hash fill-in alone must not produce records.
    fn visible_eq(&self, other: &NodeMeta) -> bool {
        self.node_type == other.node_type
            && self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && self.mode == other.mode
            && self.dev_ino == other.dev_ino
    }
}

fn stat_meta(path: &Path) -> io::Result<NodeMeta> {
    let md = fs::symlink_metadata(path)?;
    let ft = md.file_type();
    let node_type = if ft.is_file() {
        FS_ENTRY_FILE
    } else if ft.is_dir() {
        FS_ENTRY_DIR
    } else if ft.is_symlink() {
        FS_ENTRY_SYMLINK
    } else {
        FS_ENTRY_OTHER
    };
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    #[cfg(unix)]
    let (mode, dev_ino) = {
        use std::os::unix::fs::MetadataExt;
        (md.mode(), (md.dev(), md.ino()))
    };
    #[cfg(not(unix))]
    let (mode, dev_ino) = (0u32, (0u64, 0u64));
    Ok(NodeMeta {
        node_type,
        // A symlink's "content" is its target bytes (docs/design/fs-write.md
        // "Links"), so its size is the target length, as lstat reports it.
        size: if ft.is_file() || ft.is_symlink() {
            md.len()
        } else {
            0
        },
        mtime_ns,
        mode,
        hash: 0,
        dev_ino,
    })
}

type Index = BTreeMap<String, NodeMeta>;

fn is_under(path: &str, root: &str) -> bool {
    root.is_empty()
        || path == root
        || (path.len() > root.len()
            && path.starts_with(root)
            && path.as_bytes()[root.len()] == b'/')
}

/// The wire path of `rel`'s parent: `""`  for a top-level entry (its
/// parent is the root), `None` for the root itself.
fn parent_wire(rel: &str) -> Option<&str> {
    if rel.is_empty() {
        None
    } else {
        Some(match rel.rfind('/') {
            Some(i) => &rel[..i],
            None => "",
        })
    }
}

/// Rebase `path` (which must be under `from`) onto `to`, preserving the
/// subtree suffix — the path transform a `MOVE from→to` performs. Shared
/// by the held-content map, the retry set, and the diff move fix-ups.
fn rebase_subtree_path(path: &str, from: &str, to: &str) -> String {
    let suffix = if path.len() > from.len() {
        &path[from.len() + usize::from(!from.is_empty())..]
    } else {
        ""
    };
    if suffix.is_empty() {
        to.to_string()
    } else if to.is_empty() {
        suffix.to_string()
    } else {
        format!("{to}/{suffix}")
    }
}

// ---------------------------------------------------------------------------
// Diff with move detection
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffOp {
    /// `content_changed` distinguishes data changes from metadata-only ones.
    Upsert {
        path: String,
        content_changed: bool,
    },
    Delete {
        path: String,
    },
    Move {
        from: String,
        to: String,
    },
}

/// Compute ops that transform `prev` into `curr`.
///
/// Move detection is a diff-time join on file identity `(dev, ino)`:
/// disappeared and appeared entries with matching identity become `MOVE`
/// (shallowest first, descendants covered), so a renamed directory never
/// retransmits its files' content. Anything ambiguous decays to
/// delete + upsert, which is always valid.
pub fn diff(prev: &Index, curr: &Index) -> Vec<DiffOp> {
    let mut removed: Vec<&String> = Vec::new();
    let mut added: Vec<&String> = Vec::new();
    let mut changed: Vec<(&String, bool)> = Vec::new();

    let mut pi = prev.iter().peekable();
    let mut ci = curr.iter().peekable();
    loop {
        match (pi.peek(), ci.peek()) {
            (Some((pk, pv)), Some((ck, cv))) => {
                if pk == ck {
                    if !cv.visible_eq(pv) {
                        changed.push((ck, cv.content_changed(pv)));
                    }
                    pi.next();
                    ci.next();
                } else if pk < ck {
                    removed.push(pk);
                    pi.next();
                } else {
                    added.push(ck);
                    ci.next();
                }
            }
            (Some((pk, _)), None) => {
                removed.push(pk);
                pi.next();
            }
            (None, Some((ck, _))) => {
                added.push(ck);
                ci.next();
            }
            (None, None) => break,
        }
    }

    // Identity join: removed × added, shallowest (shortest path) first so a
    // directory move covers its descendants.
    let mut moves: Vec<(String, String)> = Vec::new();
    let mut removed_covered = vec![false; removed.len()];
    let mut added_covered = vec![false; added.len()];
    let mut by_identity: std::collections::HashMap<(u64, u64), usize> =
        std::collections::HashMap::new();
    for (idx, path) in removed.iter().enumerate() {
        let meta = &prev[*path];
        if meta.dev_ino != (0, 0) {
            by_identity.insert(meta.dev_ino, idx);
        }
    }
    let mut add_order: Vec<usize> = (0..added.len()).collect();
    add_order.sort_by_key(|&i| added[i].len());
    for ai in add_order {
        if added_covered[ai] {
            continue;
        }
        let to = added[ai];
        let cmeta = &curr[to];
        let Some(&ri) = by_identity.get(&cmeta.dev_ino) else {
            continue;
        };
        if removed_covered[ri] || !prev[removed[ri]].same_identity(cmeta) {
            continue;
        }
        let from = removed[ri];
        // Cover both subtrees.
        for (i, r) in removed.iter().enumerate() {
            if is_under(r, from) {
                removed_covered[i] = true;
            }
        }
        for (i, a) in added.iter().enumerate() {
            if is_under(a, to) {
                added_covered[i] = true;
            }
        }
        moves.push((from.clone(), to.clone()));
    }

    let mut ops = Vec::new();
    // Moves first (so later deletes of emptied ancestors don't prune them),
    // then deletes, then upserts.
    for (from, to) in moves {
        ops.push(DiffOp::Move { from, to });
    }
    for (i, path) in removed.iter().enumerate() {
        if removed_covered[i] {
            continue;
        }
        // Skip paths whose ancestor is also being deleted; DELETE prunes.
        let ancestor_deleted = removed
            .iter()
            .enumerate()
            .any(|(j, r)| j != i && !removed_covered[j] && is_under(path, r) && *r != *path);
        if !ancestor_deleted {
            ops.push(DiffOp::Delete {
                path: (*path).clone(),
            });
        }
    }
    for (i, path) in added.iter().enumerate() {
        if !added_covered[i] {
            ops.push(DiffOp::Upsert {
                path: (*path).clone(),
                content_changed: true,
            });
        }
    }
    // A moved subtree is not necessarily identical at its new path: in the
    // same settle window children may have been modified, created, or
    // deleted, and the root's own metadata may differ — all invisible to
    // the client after MOVE alone. Emit fix-ups for every visible
    // difference between the old subtree (rebased onto `to`) and the new.
    for op in &ops.clone() {
        let DiffOp::Move { from, to } = op else {
            continue;
        };
        let rebase = |root: &str, other_root: &str, path: &str| -> String {
            let suffix = if path.len() > root.len() {
                &path[root.len() + usize::from(!root.is_empty())..]
            } else {
                ""
            };
            if suffix.is_empty() {
                other_root.to_string()
            } else if other_root.is_empty() {
                suffix.to_string()
            } else {
                format!("{other_root}/{suffix}")
            }
        };
        for (path, _) in prev.iter().filter(|(p, _)| is_under(p, from)) {
            let new_path = rebase(from, to, path);
            if !curr.contains_key(&new_path) {
                ops.push(DiffOp::Delete { path: new_path });
            }
        }
        for (path, new) in curr.iter().filter(|(p, _)| is_under(p, to)) {
            let old_path = rebase(to, from, path);
            match prev.get(&old_path) {
                Some(old) if new.visible_eq(old) => {}
                Some(old) => ops.push(DiffOp::Upsert {
                    path: path.clone(),
                    content_changed: new.content_changed(old),
                }),
                None => ops.push(DiffOp::Upsert {
                    path: path.clone(),
                    content_changed: true,
                }),
            }
        }
    }
    for (path, content_changed) in changed {
        ops.push(DiffOp::Upsert {
            path: path.clone(),
            content_changed,
        });
    }
    ops
}

// ---------------------------------------------------------------------------
// Verified content reads
// ---------------------------------------------------------------------------

pub enum ReadOutcome {
    Stable(Vec<u8>),
    Unstable,
    Unreadable,
}

enum ReadMetaOutcome {
    /// Content plus the stat it was verified against.
    Stable(Vec<u8>, NodeMeta),
    Unstable,
    Unreadable,
}

/// Read an entry's content with torn-read protection: identity/size/mtime
/// are compared before and after the read; one retry, then `Unstable`.
/// A symlink's content is its target bytes, never the file it points to.
fn read_verified_meta(path: &Path) -> ReadMetaOutcome {
    for _ in 0..2 {
        let Ok(before) = stat_meta(path) else {
            return ReadMetaOutcome::Unreadable;
        };
        let read = if before.node_type == FS_ENTRY_SYMLINK {
            link_target_bytes(path)
        } else {
            fs::read(path)
        };
        let Ok(data) = read else {
            return ReadMetaOutcome::Unreadable;
        };
        match stat_meta(path) {
            Ok(after)
                if after.dev_ino == before.dev_ino
                    && after.size == before.size
                    && after.mtime_ns == before.mtime_ns =>
            {
                return ReadMetaOutcome::Stable(data, after);
            }
            Ok(_) => continue,
            Err(_) => return ReadMetaOutcome::Unreadable,
        }
    }
    ReadMetaOutcome::Unstable
}

/// [`read_verified_meta`] without the stat, for fetch responses and tests.
pub fn read_verified(path: &Path) -> ReadOutcome {
    match read_verified_meta(path) {
        ReadMetaOutcome::Stable(data, _) => ReadOutcome::Stable(data),
        ReadMetaOutcome::Unstable => ReadOutcome::Unstable,
        ReadMetaOutcome::Unreadable => ReadOutcome::Unreadable,
    }
}

/// Coarse filesystem clocks (FAT's 2 s, some network FS) can leave a
/// just-written file with an mtime indistinguishable from a rewrite in the
/// same granule. A file whose mtime is within this window of now is
/// "racily clean" — its hash must not be adopted as an identity others can
/// serve content by. Matches git's racy-index margin, widened for FAT.
const RACY_WINDOW_NS: u64 = 2_000_000_000;

fn racily_clean(mtime_ns: u64) -> bool {
    let now_ns = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    now_ns.saturating_sub(mtime_ns) < RACY_WINDOW_NS
}

fn blake3_128(data: &[u8]) -> u128 {
    let hash = blake3::hash(data);
    u128::from_le_bytes(hash.as_bytes()[..16].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Writes (docs/design/fs-write.md): the path-confinement guard mutations
// need on top of reads, plus atomic-replace / create-exclusive primitives.
// Pure platform code — the CAS, hint injection, and echo priming that use
// these live in the engine (`SyncEngine::exec_write` / `exec_op`).
// ---------------------------------------------------------------------------

/// Per-write content cap (`BLIT_FS_WRITE_MAX`, default 16 MiB); refused
/// with `TOO_LARGE`. The decompress guard already bounds inbound bytes at
/// the 64 MiB protocol cap.
fn fs_write_max() -> u64 {
    std::env::var("BLIT_FS_WRITE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(16 * 1024 * 1024)
}

fn write_io_status(e: &io::Error) -> u8 {
    match e.kind() {
        io::ErrorKind::NotFound => FS_DONE_NOT_FOUND,
        io::ErrorKind::PermissionDenied => FS_DONE_PERMISSION,
        io::ErrorKind::AlreadyExists => FS_DONE_CONFLICT,
        _ => FS_DONE_OTHER,
    }
}

/// How a final-component symlink at the target is treated.
enum SymlinkPolicy {
    /// Refuse it (a content write could escape the root through it).
    Refuse,
    /// Write through it, but only if its canonical target stays under root.
    Follow,
    /// Operate on the link itself (remove/rename move or unlink the link,
    /// never following it — safe, no escape).
    Operate,
}

/// Resolve and confine a write target. Component-validates the wire path
/// (the traversal fix), then canonicalizes the target's *parent* and
/// re-confirms it is under the already-canonical `root` — defeating an
/// in-tree symlink whose target escapes, which `resolve_wire_path` (no
/// symlink resolution) would miss. The final component is handled per
/// `policy`. Returns the absolute path to operate on, or an `FS_DONE_*`
/// status on refusal.
fn resolve_write_target(root: &Path, wire: &str, policy: SymlinkPolicy) -> Result<PathBuf, u8> {
    let abs = resolve_wire_path(root, wire).ok_or(FS_DONE_INVALID)?;
    // The root itself ("") is never a content/op target.
    let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
        return Err(FS_DONE_INVALID);
    };
    let canon_parent = fs::canonicalize(parent).map_err(|e| write_io_status(&e))?;
    if !canon_parent.starts_with(root) {
        return Err(FS_DONE_PERMISSION);
    }
    let target = canon_parent.join(name);
    match fs::symlink_metadata(&target) {
        Ok(md) if md.file_type().is_symlink() => match policy {
            SymlinkPolicy::Refuse => Err(FS_DONE_PERMISSION),
            SymlinkPolicy::Operate => Ok(target),
            SymlinkPolicy::Follow => {
                let resolved = fs::canonicalize(&target).map_err(|e| write_io_status(&e))?;
                if resolved.starts_with(root) {
                    Ok(resolved)
                } else {
                    Err(FS_DONE_PERMISSION)
                }
            }
        },
        _ => Ok(target),
    }
}

/// A unique sibling temp path for atomic replace (same directory ⇒ same
/// filesystem ⇒ atomic `rename`).
fn temp_sibling(target: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!(".blit-tmp-{}-{n}", std::process::id()))
}

/// Set `mode` on an open file (Unix); preserve the replaced file's mode
/// when `mode` is 0 and a file exists at `at`.
#[cfg(unix)]
fn apply_mode(f: &fs::File, at: &Path, mode: u32) {
    if mode == 0
        && let Ok(md) = fs::metadata(at)
    {
        let _ = f.set_permissions(md.permissions());
    }
}
#[cfg(not(unix))]
fn apply_mode(_f: &fs::File, _at: &Path, _mode: u32) {}

/// fsync `f` and its parent directory (F_FULLFSYNC on macOS via std's
/// `sync_all`) so a crash after return cannot lose the write.
fn fsync_durable(f: &fs::File, target: &Path) -> io::Result<()> {
    f.sync_all()?;
    #[cfg(unix)]
    if let Some(dir) = target.parent()
        && let Ok(d) = fs::File::open(dir)
    {
        let _ = d.sync_all();
    }
    let _ = target;
    Ok(())
}

/// Write `bytes` to `target` atomically: a same-directory temp file, then
/// `rename` over the destination — a reader sees the old bytes or the new,
/// never a torn write. `mode` 0 preserves the existing file's mode.
fn write_atomic(target: &Path, bytes: &[u8], mode: u32, durable: bool) -> io::Result<()> {
    use std::io::Write as _;
    let tmp = temp_sibling(target);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let mut f = opts.open(&tmp)?;
    let staged = (|| {
        f.write_all(bytes)?;
        apply_mode(&f, target, mode);
        if durable {
            f.sync_all()?;
        }
        Ok(())
    })();
    drop(f);
    if let Err(e) = staged {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    #[cfg(unix)]
    if durable && let Ok(d) = fs::File::open(target.parent().unwrap_or_else(|| Path::new("."))) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Create `target` exclusively (`O_EXCL`): fails `AlreadyExists` if the
/// path exists, race-free even against an external creator — the
/// create-exclusive ("New File") precondition.
fn create_exclusive(target: &Path, bytes: &[u8], mode: u32, durable: bool) -> io::Result<()> {
    use std::io::Write as _;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    // Open exclusively first: a pre-existing file / concurrent creator
    // (AlreadyExists) is never touched by the cleanup below.
    let mut f = opts.open(target)?;
    let staged = (|| {
        f.write_all(bytes)?;
        if durable {
            fsync_durable(&f, target)?;
        }
        Ok(())
    })();
    drop(f);
    if let Err(e) = staged {
        // Restore the "path does not exist" invariant so a retry re-attempts
        // the create instead of hitting a phantom CONFLICT on the partial
        // bytes (and leaves nothing for the reconciler to echo).
        let _ = fs::remove_file(target);
        return Err(e);
    }
    Ok(())
}

/// The current on-disk content hash of `path`, or 0 (the "absent"
/// sentinel) when missing or unreadable. A symlink hashes its target
/// bytes, matching the read side. Read under the write lock, so no other
/// blit writer can interleave; an external writer is the disclosed,
/// irreducible window.
fn current_hash(path: &Path) -> u128 {
    let bytes = match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => link_target_bytes(path),
        _ => fs::read(path),
    };
    match bytes {
        Ok(bytes) => blake3_128(&bytes),
        Err(_) => 0,
    }
}

/// A symlink's target as content bytes: verbatim on Unix, lossy UTF-8
/// elsewhere (a client-minted target is UTF-8 and round-trips exactly).
fn link_target_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    Ok(target.to_string_lossy().into_owned().into_bytes())
}

/// Create a symlink at `at` whose target is the verbatim string `target`.
#[cfg(unix)]
fn symlink_at(target: &str, at: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}
#[cfg(windows)]
fn symlink_at(target: &str, at: &Path) -> io::Result<()> {
    // Windows symlinks are typed: pick the directory flavor when the
    // target resolves to a directory right now, the file flavor otherwise
    // (including dangling targets).
    let resolved = at.parent().unwrap_or_else(|| Path::new(".")).join(target);
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, at)
    } else {
        std::os::windows::fs::symlink_file(target, at)
    }
}
#[cfg(not(any(unix, windows)))]
fn symlink_at(_target: &str, _at: &Path) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// The reconciler's index key for an absolute path under `root`: each
/// component escaped and `/`-joined, exactly as `note_hint` derives it.
/// Used to key echo priming by the path the change actually lands under
/// (which differs from the client's wire path for a followed symlink).
fn wire_key_for(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut wire = String::new();
    for comp in rel.components() {
        wire = join_wire(&wire, &os_to_wire(comp.as_os_str()));
    }
    Some(wire)
}

/// A process-global lock keyed by a canonical filesystem path. The
/// compare-hash-and-write critical section serializes on the on-disk
/// *file*, not the `RootKey`: two writers reaching the same file through
/// different roots (recursive vs not, or a root and a nested root) hold
/// distinct `SharedRootHandle`s, so a per-root lock could not have closed
/// their CAS race. Distinct files still lock independently and run in
/// parallel. The map self-prunes dropped entries, so it stays O(live
/// writers).
fn path_write_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<Mutex<()>>>>> =
        OnceLock::new();
    let mut map = LOCKS.get_or_init(Default::default).lock().unwrap();
    if let Some(existing) = map.get(path).and_then(std::sync::Weak::upgrade) {
        return existing;
    }
    map.retain(|_, w| w.strong_count() > 0);
    let lock = Arc::new(Mutex::new(()));
    map.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

/// Create `target_parent` and any missing ancestors for `MKPARENTS`,
/// confined to `root`: the deepest existing ancestor is canonicalized and
/// re-checked under root, then each missing component is created (never
/// `create_dir_all`, which would happily descend through an existing
/// symlink pointing outside the root and create directories there).
fn create_parents_confined(root: &Path, target_parent: &Path) -> Result<(), u8> {
    let mut existing = target_parent.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
            return Err(FS_DONE_INVALID);
        };
        tail.push(name);
        existing = existing.parent().map(Path::to_path_buf).unwrap_or_default();
        if existing.as_os_str().is_empty() {
            return Err(FS_DONE_INVALID);
        }
    }
    let mut cur = fs::canonicalize(&existing).map_err(|e| write_io_status(&e))?;
    if !cur.starts_with(root) {
        return Err(FS_DONE_PERMISSION);
    }
    for name in tail.iter().rev() {
        cur.push(name);
        if let Err(e) = fs::create_dir(&cur) {
            // Tolerate only a REAL concurrently-created directory, never a
            // symlink: `symlink_metadata` does not follow the link, so a
            // symlink planted in this slot between the existence walk and
            // now is rejected instead of silently descended through.
            let real_dir = fs::symlink_metadata(&cur)
                .map(|m| m.file_type().is_dir())
                .unwrap_or(false);
            if !real_dir {
                return Err(write_io_status(&e));
            }
        }
        // Re-canonicalize and re-confirm each created component stays under
        // root before the next `push` descends through it — defense in depth
        // against a racing in-tree symlink redirecting the tail outside.
        match fs::canonicalize(&cur) {
            Ok(c) if c.starts_with(root) => cur = c,
            Ok(_) => return Err(FS_DONE_PERMISSION),
            Err(e) => return Err(write_io_status(&e)),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Content-addressed blob store and delta encoding
// ---------------------------------------------------------------------------

/// Content-addressed LRU cache of file bytes, keyed by BLAKE3-128 and
/// shared by every sync in the process: identical files cost one entry,
/// and delta bases are found by the hash each engine recorded for the
/// content its client holds. Eviction only costs efficiency — a missing
/// base falls back to full content.
pub struct BlobStore {
    budget: usize,
    total: usize,
    seq: u64,
    by_hash: std::collections::HashMap<u128, (Arc<Vec<u8>>, u64)>,
    by_age: BTreeMap<u64, u128>,
}

impl BlobStore {
    pub fn new(budget: usize) -> Self {
        BlobStore {
            budget,
            total: 0,
            seq: 0,
            by_hash: Default::default(),
            by_age: Default::default(),
        }
    }

    /// Fetch a blob and refresh its LRU position.
    pub fn get(&mut self, hash: u128) -> Option<Arc<Vec<u8>>> {
        let (data, seq) = self.by_hash.get(&hash)?.clone();
        self.by_age.remove(&seq);
        self.seq += 1;
        self.by_age.insert(self.seq, hash);
        self.by_hash.insert(hash, (data.clone(), self.seq));
        Some(data)
    }

    /// Insert (or refresh) a blob, evicting the oldest entries past the
    /// budget. Blobs larger than the whole budget are not stored.
    pub fn put(&mut self, hash: u128, data: Arc<Vec<u8>>) {
        if data.len() > self.budget {
            return;
        }
        if self.by_hash.contains_key(&hash) {
            self.get(hash);
            return;
        }
        self.seq += 1;
        self.total += data.len();
        self.by_age.insert(self.seq, hash);
        self.by_hash.insert(hash, (data, self.seq));
        while self.total > self.budget {
            let (&seq, &oldest) = self
                .by_age
                .iter()
                .next()
                .expect("total > 0 implies entries");
            self.by_age.remove(&seq);
            if let Some((old, _)) = self.by_hash.remove(&oldest) {
                self.total -= old.len();
            }
        }
    }
}

/// The process-wide store; budget via `BLIT_FS_BLOB_MAX` (default 256 MiB).
pub fn blob_store() -> &'static Mutex<BlobStore> {
    static STORE: OnceLock<Mutex<BlobStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        Mutex::new(BlobStore::new(
            env_u64("BLIT_FS_BLOB_MAX", 256 * 1024 * 1024) as usize,
        ))
    })
}

fn push_leb128(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Single-span delta: the longest common prefix and suffix become `COPY`s,
/// the middle an `INSERT`. Covers appends, prepends, truncations, and one
/// contiguous in-place edit — the common shapes of saved files and logs.
/// Scattered edits degrade to a large `INSERT`; the caller falls back to
/// full content when the encoding is not clearly smaller.
pub fn encode_delta(base: &[u8], new: &[u8]) -> Vec<u8> {
    let bound = base.len().min(new.len());
    let mut prefix = 0;
    while prefix < bound && base[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    let bound = bound - prefix;
    while suffix < bound && base[base.len() - 1 - suffix] == new[new.len() - 1 - suffix] {
        suffix += 1;
    }
    let mut ops = Vec::new();
    if prefix > 0 {
        ops.push(0x01);
        push_leb128(&mut ops, 0);
        push_leb128(&mut ops, prefix as u64);
    }
    let middle = &new[prefix..new.len() - suffix];
    if !middle.is_empty() {
        ops.push(0x02);
        push_leb128(&mut ops, middle.len() as u64);
        ops.extend_from_slice(middle);
    }
    if suffix > 0 {
        ops.push(0x01);
        push_leb128(&mut ops, (base.len() - suffix) as u64);
        push_leb128(&mut ops, suffix as u64);
    }
    ops
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// One per shared root: owns the canonical index, verifies hints against
/// the filesystem, and publishes immutable snapshots to subscribed sync
/// engines. Exits when its inbox disconnects (last handle dropped).
struct Reconciler {
    root: PathBuf,
    /// Scan scope from the [`RootKey`] plus env-default budgets; the
    /// per-client knobs in here (content, window…) are unused.
    opts: SyncOptions,
    rx: Receiver<RootMsg>,
    backend: Box<dyn BackendHandle>,
    canonical: Index,
    /// Last published snapshot; republished to every new subscriber.
    snapshot: Arc<Index>,
    subs: std::collections::HashMap<u64, (Sender<SyncMsg>, Duration)>,
    /// Settle window: the minimum over subscribers.
    latency: Duration,
    dirty: std::collections::BTreeSet<String>,
    full_rescan: bool,
    pending_since: Option<Instant>,
    /// Learned-hash changes are annotations only (invisible to the
    /// per-sync diff, which excludes `hash`), so they publish on a coarse
    /// interval rather than the settle window — otherwise the burst of
    /// hashes after an initial content sync would trigger one full-index
    /// clone + publish each.
    hash_dirty_since: Option<Instant>,
    /// Root-level failure; sticky, replayed to late subscribers.
    closed: Option<u8>,
    /// Shared with the handle so `open_root` never joins a closed root.
    closed_flag: Arc<OnceLock<u8>>,
}

/// Coalescing window for hash-only (annotation) publishes.
const HASH_PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

impl Reconciler {
    fn new(key: RootKey, rx: Receiver<RootMsg>, closed_flag: Arc<OnceLock<u8>>) -> Self {
        let opts = SyncOptions {
            recursive: key.recursive,
            cross_filesystem: key.cross_filesystem,
            ..Default::default()
        };
        Reconciler {
            root: key.path,
            latency: opts.latency,
            opts,
            rx,
            backend: Box::new(NoopBackend),
            canonical: Index::new(),
            snapshot: Arc::new(Index::new()),
            subs: Default::default(),
            dirty: Default::default(),
            full_rescan: false,
            pending_since: None,
            hash_dirty_since: None,
            closed: None,
            closed_flag,
        }
    }

    fn run(mut self) {
        // Initial enumeration; the watcher was armed at open, so anything
        // missed during the scan is already queued as a hint.
        match self.scan_all() {
            Ok(index) => {
                self.canonical = index;
                self.snapshot = Arc::new(self.canonical.clone());
            }
            Err(reason) => self.close(reason),
        }
        loop {
            let deadline = |since: Option<Instant>, window: Duration| {
                since.map(|s| (s + window).saturating_duration_since(Instant::now()))
            };
            let timeout = if self.closed.is_some() {
                Duration::from_secs(3600)
            } else {
                [
                    deadline(self.pending_since, self.latency),
                    deadline(self.hash_dirty_since, HASH_PUBLISH_INTERVAL),
                ]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(Duration::from_secs(3600))
            };
            match self.rx.recv_timeout(timeout) {
                Ok(RootMsg::Hint(hint)) => self.note_hint(hint),
                Ok(RootMsg::Subscribe { id, tx, latency }) => {
                    let update = match self.closed {
                        Some(reason) => RootUpdate::Closed(reason),
                        // The current snapshot is already settled; the new
                        // subscriber's initial series should stream at once.
                        None => RootUpdate::Snapshot {
                            index: self.snapshot.clone(),
                            settled: None,
                        },
                    };
                    let _ = tx.send(SyncMsg::Root(update));
                    self.subs.insert(id, (tx, latency));
                    self.recompute_latency();
                }
                Ok(RootMsg::Unsubscribe { id }) => {
                    self.subs.remove(&id);
                    self.recompute_latency();
                }
                Ok(RootMsg::HashLearned { path, meta }) => {
                    if let Some(existing) = self.canonical.get_mut(&path)
                        && existing.hash != meta.hash
                        && existing.node_type == meta.node_type
                        && existing.dev_ino == meta.dev_ino
                        && existing.size == meta.size
                        && existing.mtime_ns == meta.mtime_ns
                    {
                        existing.hash = meta.hash;
                        if self.hash_dirty_since.is_none() {
                            self.hash_dirty_since = Some(Instant::now());
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            let elapsed = |since: Option<Instant>, window: Duration| {
                since.is_some_and(|s| Instant::now().saturating_duration_since(s) >= window)
            };
            if self.closed.is_none()
                && (elapsed(self.pending_since, self.latency)
                    || elapsed(self.hash_dirty_since, HASH_PUBLISH_INTERVAL))
            {
                self.tick();
            }
        }
    }

    fn recompute_latency(&mut self) {
        self.latency = self
            .subs
            .values()
            .map(|(_, latency)| *latency)
            .min()
            .unwrap_or(self.opts.latency);
    }

    fn close(&mut self, reason: u8) {
        self.closed = Some(reason);
        // Publish before broadcasting so a racing open_root observes the
        // closure and spawns a fresh root rather than joining this dead one.
        let _ = self.closed_flag.set(reason);
        self.pending_since = None;
        for (tx, _) in self.subs.values() {
            let _ = tx.send(SyncMsg::Root(RootUpdate::Closed(reason)));
        }
    }

    fn note_hint(&mut self, hint: Hint) {
        match hint {
            Hint::Rescan => self.full_rescan = true,
            Hint::Dirty(abs) => {
                let rel = match abs.strip_prefix(&self.root) {
                    Ok(rel) => rel,
                    Err(_) => return,
                };
                let mut wire = String::new();
                let mut depth = 0usize;
                for comp in rel.components() {
                    wire = join_wire(&wire, &os_to_wire(comp.as_os_str()));
                    depth += 1;
                }
                // Non-recursive syncs index the root and its immediate
                // children only; deeper hints are outside the sync.
                if !self.opts.recursive && depth > 1 {
                    return;
                }
                self.dirty.insert(wire);
            }
        }
        if self.pending_since.is_none() {
            self.pending_since = Some(Instant::now());
        }
    }

    /// Settle: verify accumulated dirt, publish a snapshot if anything
    /// (including a learned hash) changed.
    fn tick(&mut self) {
        // When real dirt drove this tick, the batch began settling at
        // pending_since; engines settle from that instant so the total
        // change-to-wire delay is one window, not two.
        let settled = self.pending_since;
        self.pending_since = None;
        self.hash_dirty_since = None;
        if self.full_rescan {
            self.full_rescan = false;
            self.dirty.clear();
            match self.scan_all() {
                Ok(index) => self.canonical = index,
                Err(reason) => return self.close(reason),
            }
        } else {
            let dirty = std::mem::take(&mut self.dirty);
            for rel in dirty {
                if let Err(reason) = self.reconcile(&rel) {
                    return self.close(reason);
                }
            }
        }
        if self.canonical != *self.snapshot {
            self.snapshot = Arc::new(self.canonical.clone());
            for (tx, _) in self.subs.values() {
                let _ = tx.send(SyncMsg::Root(RootUpdate::Snapshot {
                    index: self.snapshot.clone(),
                    settled,
                }));
            }
        }
    }

    fn scan_all(&mut self) -> Result<Index, u8> {
        let mut index = Index::new();
        let root = self.root.clone();
        self.scan_into(&mut index, &root, "", self.opts.recursive, None)
            .map_err(|e| match e.kind() {
                io::ErrorKind::NotFound => FS_CLOSED_ROOT_GONE,
                io::ErrorKind::PermissionDenied => FS_CLOSED_PERMISSION_LOST_COMPAT,
                _ if e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO) => FS_CLOSED_RESOURCE_LIMIT,
                _ => FS_CLOSED_RESOURCE_LIMIT,
            })?;
        Ok(index)
    }

    /// Scan `abs` (wire path `rel`) into `index`. `root_dev` bounds
    /// cross-filesystem traversal; directories are registered with the
    /// backend as they are discovered.
    fn scan_into(
        &mut self,
        index: &mut Index,
        abs: &Path,
        rel: &str,
        recurse: bool,
        root_dev: Option<u64>,
    ) -> io::Result<()> {
        let meta = stat_meta(abs)?;
        if index.len() >= self.opts.max_entries {
            return Err(io::Error::from_raw_os_error(RESOURCE_LIMIT_ERRNO));
        }
        let is_dir = meta.node_type == FS_ENTRY_DIR;
        let dev = meta.dev_ino.0;
        index.insert(rel.to_string(), meta);
        if !is_dir {
            return Ok(());
        }
        let root_dev = root_dev.or(Some(dev));
        if !self.opts.cross_filesystem && Some(dev) != root_dev {
            return Ok(()); // report the mount point, don't descend
        }
        self.backend.add_dir(abs);
        if !recurse && !rel.is_empty() {
            return Ok(());
        }
        let entries = match fs::read_dir(abs) {
            Ok(e) => e,
            Err(_) => return Ok(()), // unreadable dir: node stays, children unknown
        };
        for entry in entries.flatten() {
            let name = os_to_wire(&entry.file_name());
            let child_rel = join_wire(rel, &name);
            let child_abs = entry.path();
            // Non-recursive syncs index immediate children only.
            let child_recurse = self.opts.recursive;
            if let Err(e) = self.scan_into(index, &child_abs, &child_rel, child_recurse, root_dev)
                && e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO)
            {
                return Err(e);
            }
            // Other errors: entry vanished mid-scan — fine, a hint follows.
        }
        Ok(())
    }

    /// Verify one hinted path against the canonical index.
    fn reconcile(&mut self, rel: &str) -> Result<(), u8> {
        let Some(abs) = resolve_wire_path(&self.root, rel) else {
            return Ok(());
        };
        match stat_meta(&abs) {
            Err(_) => {
                if rel.is_empty() {
                    return Err(FS_CLOSED_ROOT_GONE);
                }
                let gone: Vec<String> = self
                    .canonical
                    .keys()
                    .filter(|k| is_under(k, rel))
                    .cloned()
                    .collect();
                for k in gone {
                    self.canonical.remove(&k);
                }
            }
            Ok(meta) => {
                // Cross-filesystem exclusion (docs/fs-watch.md): mirror
                // scan_into on the hint path. A foreign-device entry is
                // kept only if it is the mount point itself (parent on the
                // root device) — reported but not descended; anything
                // deeper is never indexed, and a stale subtree from a prior
                // cross-fs pass is pruned. Without this, a hint under a
                // mount point would index entries a full rescan then
                // mass-deletes.
                if !self.opts.cross_filesystem
                    && !rel.is_empty()
                    && let Some(root_dev) = self.canonical.get("").map(|m| m.dev_ino.0)
                    && meta.dev_ino.0 != root_dev
                {
                    let parent_on_root = parent_wire(rel)
                        .and_then(|p| self.canonical.get(p))
                        .is_some_and(|m| m.dev_ino.0 == root_dev);
                    if parent_on_root {
                        self.canonical.insert(rel.to_string(), meta);
                        self.check_budget()?;
                    } else {
                        let gone: Vec<String> = self
                            .canonical
                            .keys()
                            .filter(|k| is_under(k, rel))
                            .cloned()
                            .collect();
                        for k in gone {
                            self.canonical.remove(&k);
                        }
                    }
                    return Ok(());
                }
                let known = self.canonical.contains_key(rel);
                let was_dir = self
                    .canonical
                    .get(rel)
                    .map(|m| m.node_type == FS_ENTRY_DIR)
                    .unwrap_or(false);
                let is_dir = meta.node_type == FS_ENTRY_DIR;
                let preserved_hash = self
                    .canonical
                    .get(rel)
                    .and_then(|m| (!m.content_changed(&meta)).then_some(m.hash));
                let mut meta = meta;
                if let Some(h) = preserved_hash {
                    meta.hash = h;
                }
                self.canonical.insert(rel.to_string(), meta);
                self.check_budget()?;
                if is_dir && (!known || !was_dir) {
                    // New (or type-changed) directory: index its subtree and
                    // then rescan once more — children created between the
                    // watch registration and this scan produce duplicate
                    // hints, which reconcile to no-ops.
                    let mut sub = Index::new();
                    match self.scan_into(&mut sub, &abs, rel, self.opts.recursive, None) {
                        Ok(()) => {}
                        Err(e) if e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO) => {
                            return Err(FS_CLOSED_RESOURCE_LIMIT);
                        }
                        // Other errors: entry vanished mid-scan; a hint follows.
                        Err(_) => {}
                    }
                    for (k, v) in sub {
                        self.canonical.insert(k, v);
                    }
                    self.check_budget()?;
                } else if is_dir && self.opts.recursive {
                    // Existing dir: verify immediate children (names may
                    // have appeared/vanished without their own hints on
                    // some backends).
                    self.reconcile_children(&abs, rel)?;
                }
                if was_dir && !is_dir {
                    let gone: Vec<String> = self
                        .canonical
                        .keys()
                        .filter(|k| is_under(k, rel) && k.as_str() != rel)
                        .cloned()
                        .collect();
                    for k in gone {
                        self.canonical.remove(&k);
                    }
                }
            }
        }
        Ok(())
    }

    /// `FS_CLOSED_RESOURCE_LIMIT` once the index grows past the entry
    /// budget. Incremental reconcile must enforce this too, not just the
    /// initial scan (docs/fs-watch.md limits table), or a tree that grows
    /// live past `BLIT_FS_MAX_ENTRIES` would index without bound.
    fn check_budget(&self) -> Result<(), u8> {
        if self.canonical.len() > self.opts.max_entries {
            Err(FS_CLOSED_RESOURCE_LIMIT)
        } else {
            Ok(())
        }
    }

    fn reconcile_children(&mut self, abs: &Path, rel: &str) -> Result<(), u8> {
        let Ok(entries) = fs::read_dir(abs) else {
            return Ok(());
        };
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut new_dirs: Vec<(PathBuf, String)> = Vec::new();
        for entry in entries.flatten() {
            let name = os_to_wire(&entry.file_name());
            let child_rel = join_wire(rel, &name);
            if let Ok(meta) = stat_meta(&entry.path()) {
                let newly_dir = meta.node_type == FS_ENTRY_DIR
                    && self
                        .canonical
                        .get(&child_rel)
                        .map(|m| m.node_type != FS_ENTRY_DIR)
                        .unwrap_or(true);
                let preserved = self
                    .canonical
                    .get(&child_rel)
                    .and_then(|m| (!m.content_changed(&meta)).then_some(m.hash));
                let mut meta = meta;
                if let Some(h) = preserved {
                    meta.hash = h;
                }
                if newly_dir {
                    new_dirs.push((entry.path(), child_rel.clone()));
                }
                self.canonical.insert(child_rel.clone(), meta);
                self.check_budget()?;
            }
            seen.insert(child_rel);
        }
        // Children that disappeared.
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let gone: Vec<String> = self
            .canonical
            .keys()
            .filter(|k| {
                k.starts_with(&prefix)
                    && k.as_str() != rel
                    && !k[prefix.len()..].contains('/')
                    && !seen.contains(*k)
            })
            .cloned()
            .collect();
        for k in gone {
            let subtree: Vec<String> = self
                .canonical
                .keys()
                .filter(|p| is_under(p, &k))
                .cloned()
                .collect();
            for p in subtree {
                self.canonical.remove(&p);
            }
        }
        for (abs, rel) in new_dirs {
            let mut sub = Index::new();
            match self.scan_into(&mut sub, &abs, &rel, self.opts.recursive, None) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO) => {
                    return Err(FS_CLOSED_RESOURCE_LIMIT);
                }
                Err(_) => {}
            }
            for (k, v) in sub {
                self.canonical.insert(k, v);
            }
            self.check_budget()?;
        }
        Ok(())
    }
}

enum Exit {
    ClientGone,
    Closed(u8),
    Stopped,
}

enum ContentRead {
    Stable { hash: u128, data: Arc<Vec<u8>> },
    Unstable,
    Unreadable,
}

/// Per-sync engine: cuts client-specific update series from published
/// snapshots and paces them against the client's ack window.
struct SyncEngine {
    sync_id: u16,
    root: PathBuf,
    opts: SyncOptions,
    rx: Receiver<SyncMsg>,
    outbox: Outbox,
    shared: Arc<SharedRootHandle>,
    sub_id: u64,
    /// Latest published canonical snapshot.
    latest: Arc<Index>,
    /// A snapshot arrived since the last emit.
    snapshot_dirty: bool,
    /// What the client's live map will equal once it applies everything
    /// sent so far (reliable ordered transport ⇒ no acknowledgment needed
    /// for correctness, only for pacing).
    shadow: Arc<Index>,
    pending_since: Option<Instant>,
    next_update_id: u32,
    /// Highest update id ever sent; acking beyond it is a protocol error.
    highest_sent: u32,
    /// (update_id, serialized_bytes) not yet cumulatively acked.
    unacked: std::collections::VecDeque<(u32, usize)>,
    unacked_bytes: usize,
    initial_sent: bool,
    /// Hash of the content the client holds per path (updates are ordered
    /// over a reliable transport, so "sent" is "held"). Basis for delta
    /// encoding and for skipping content the client already has.
    held: std::collections::HashMap<String, u128>,
    /// Files last reported UNSTABLE: re-read on the next settle tick even
    /// though their metadata may not change again.
    retry: std::collections::BTreeSet<String>,
}

impl SyncEngine {
    fn new(
        sync_id: u16,
        shared: Arc<SharedRootHandle>,
        sub_id: u64,
        opts: SyncOptions,
        rx: Receiver<SyncMsg>,
        outbox: Outbox,
    ) -> Self {
        SyncEngine {
            sync_id,
            root: shared.key.path.clone(),
            opts,
            rx,
            outbox,
            shared,
            sub_id,
            latest: Arc::new(Index::new()),
            snapshot_dirty: false,
            shadow: Arc::new(Index::new()),
            pending_since: None,
            next_update_id: 1,
            highest_sent: 0,
            unacked: Default::default(),
            unacked_bytes: 0,
            initial_sent: false,
            held: Default::default(),
            retry: Default::default(),
        }
    }

    fn run(mut self) {
        let exit = self.event_loop();
        let _ = self
            .shared
            .tx
            .send(RootMsg::Unsubscribe { id: self.sub_id });
        match exit {
            Exit::ClientGone => {}
            Exit::Stopped => {
                let _ = (self.outbox)(msg_fs_closed(self.sync_id, FS_CLOSED_CLIENT_REQUEST));
            }
            Exit::Closed(reason) => {
                let _ = (self.outbox)(msg_fs_closed(self.sync_id, reason));
            }
        }
    }

    fn event_loop(&mut self) -> Exit {
        loop {
            // Settle deadline only matters while we hold send credit; when
            // credit-blocked, only an ack (or command) can unblock us, so
            // wait for messages instead of spinning on an expired deadline.
            let timeout = match self.pending_since {
                Some(since) if self.unacked_bytes < self.opts.window_bytes => {
                    (since + self.opts.latency).saturating_duration_since(Instant::now())
                }
                _ => Duration::from_secs(3600),
            };
            match self.rx.recv_timeout(timeout) {
                Ok(SyncMsg::Root(update)) => {
                    if let Err(exit) = self.handle_root(update) {
                        return exit;
                    }
                }
                Ok(SyncMsg::Cmd(Command::Ack(update_id))) => {
                    if let Err(exit) = self.handle_ack(update_id) {
                        return exit;
                    }
                }
                Ok(SyncMsg::Cmd(Command::Fetch { nonce, path })) => {
                    if !self.handle_fetch(nonce, &path) {
                        return Exit::ClientGone;
                    }
                }
                Ok(SyncMsg::Cmd(Command::Write(w))) => {
                    if !self.handle_write(w) {
                        return Exit::ClientGone;
                    }
                }
                Ok(SyncMsg::Cmd(Command::Op(o))) => {
                    if !self.handle_op(o) {
                        return Exit::ClientGone;
                    }
                }
                Ok(SyncMsg::Cmd(Command::Stop)) => return Exit::Stopped,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Exit::ClientGone,
            }
            // Tick when settled and credit allows.
            if let Some(since) = self.pending_since
                && Instant::now().saturating_duration_since(since) >= self.opts.latency
                && self.unacked_bytes < self.opts.window_bytes
                && let Err(exit) = self.tick()
            {
                return exit;
            }
        }
    }

    fn handle_root(&mut self, update: RootUpdate) -> Result<(), Exit> {
        match update {
            RootUpdate::Snapshot { index, settled } => {
                self.latest = index;
                self.snapshot_dirty = true;
                // Settle from when the reconciler's batch began, not now:
                // the reconciler already waited one window, so re-waiting
                // here would double the change-to-wire latency. `None`
                // (already settled) emits at once.
                let due = settled.unwrap_or_else(|| {
                    Instant::now()
                        .checked_sub(self.opts.latency)
                        .unwrap_or_else(Instant::now)
                });
                self.pending_since = Some(match self.pending_since {
                    Some(existing) if existing <= due => existing,
                    _ => due,
                });
                Ok(())
            }
            RootUpdate::Closed(reason) => Err(Exit::Closed(reason)),
        }
    }

    /// Cumulative ack. Comparisons use serial-number (wrap-aware)
    /// arithmetic so acking survives the `update_id` counter wrapping at
    /// 2^32: in-flight ids span at most a few windows, far under 2^31, so
    /// "strictly ahead of the highest sent id" is unambiguous. Acking
    /// genuinely ahead is still a fatal protocol error.
    fn handle_ack(&mut self, update_id: u32) -> Result<(), Exit> {
        let ahead = update_id.wrapping_sub(self.highest_sent);
        if ahead != 0 && ahead < 0x8000_0000 {
            return Err(Exit::Closed(FS_CLOSED_BACKEND_FAILED_COMPAT));
        }
        while let Some(&(id, bytes)) = self.unacked.front() {
            // id is at or before update_id in wrap order.
            if update_id.wrapping_sub(id) < 0x8000_0000 {
                self.unacked.pop_front();
                self.unacked_bytes -= bytes;
            } else {
                break;
            }
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<(), Exit> {
        self.pending_since = None;
        if self.initial_sent && !self.snapshot_dirty && self.retry.is_empty() {
            return Ok(());
        }
        let canonical = self.latest.clone();
        let initial = !self.initial_sent;
        self.snapshot_dirty = false;
        self.emit_updates(&canonical, initial)?;
        self.shadow = canonical;
        self.initial_sent = true;
        // Credit waits may have delivered a newer snapshot mid-emit, and
        // unstable files want another pass: keep the clock running.
        if self.snapshot_dirty || !self.retry.is_empty() {
            self.pending_since = Some(Instant::now());
        }
        Ok(())
    }

    /// Diff shadow vs `canonical` and send updates. `initial` wraps the
    /// series in RESET … SYNC. Batches stream as they are built, each gated
    /// on the ack window — a snapshot of any size holds at most one batch
    /// in memory and never outruns the client's credit.
    fn emit_updates(&mut self, canonical: &Arc<Index>, initial: bool) -> Result<(), Exit> {
        let mut ops = diff(&self.shadow, canonical);
        // A retry entry whose file was renamed this tick must follow the
        // move before we prune against `canonical` (which only knows the
        // new path), or the pending content read is lost forever.
        for op in &ops {
            if let DiffOp::Move { from, to } = op {
                self.rekey_move(from, to);
            }
        }
        self.retry.retain(|path| canonical.contains_key(path));
        // Files awaiting a settled re-read (UNSTABLE or transiently
        // UNREADABLE) re-read even when their metadata is unchanged, so the
        // content still arrives once the file settles.
        let forced: Vec<String> = self
            .retry
            .iter()
            .filter(|p| {
                !ops.iter()
                    .any(|op| matches!(op, DiffOp::Upsert { path, .. } if path == *p))
            })
            .cloned()
            .collect();
        ops.extend(forced.into_iter().map(|path| DiffOp::Upsert {
            path,
            content_changed: true,
        }));
        if ops.is_empty() && !initial {
            return Ok(());
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut reset_pending = initial;
        for op in &ops {
            match op {
                DiffOp::Delete { path } => {
                    self.held.retain(|held_path, _| !is_under(held_path, path));
                    append_fs_record(&mut buf, &FsRecord::Delete { path });
                }
                DiffOp::Move { from, to } => {
                    // held/retry were already rekeyed above.
                    append_fs_record(&mut buf, &FsRecord::Move { from, to });
                }
                DiffOp::Upsert {
                    path,
                    content_changed,
                } => {
                    let Some(meta) = canonical.get(path).cloned() else {
                        continue;
                    };
                    let was_retry = self.retry.remove(path);
                    let mut entry_flags = meta.node_type & FS_ENTRY_TYPE_MASK;
                    let mut hash = meta.hash;
                    let mut full: Option<Arc<Vec<u8>>> = None;
                    let mut delta: Option<Vec<u8>> = None;
                    let mut skip_record = false;
                    // Files and symlinks both carry content — a symlink's
                    // is its target bytes (hash = BLAKE3-128 over them).
                    if matches!(meta.node_type, FS_ENTRY_FILE | FS_ENTRY_SYMLINK) {
                        if !self.opts.content || meta.size > self.opts.inline_max {
                            entry_flags |= FS_ENTRY_NO_CONTENT;
                            self.held.remove(path);
                        } else if *content_changed || meta.hash == 0 {
                            match self.read_content(path, &meta) {
                                ContentRead::Stable {
                                    hash: read_hash,
                                    data,
                                } => {
                                    hash = read_hash;
                                    if self.held.get(path) == Some(&hash) {
                                        // The client already holds exactly
                                        // these bytes (touch, or a rewrite
                                        // with identical content): metadata-
                                        // only upsert, the mirror keeps them.
                                    } else {
                                        // Delta against the content the
                                        // client holds when the base is
                                        // still in the blob store and the
                                        // encoding is clearly smaller.
                                        delta = self
                                            .held
                                            .get(path)
                                            .and_then(|&base_hash| {
                                                blob_store().lock().unwrap().get(base_hash)
                                            })
                                            .map(|base| encode_delta(&base, &data))
                                            .filter(|ops| ops.len() * 8 < data.len() * 7);
                                        if delta.is_none() {
                                            full = Some(data.clone());
                                        }
                                        self.held.insert(path.clone(), hash);
                                    }
                                }
                                ContentRead::Unstable => {
                                    self.held.remove(path);
                                    self.retry.insert(path.clone());
                                    if was_retry {
                                        // Still churning: the client already
                                        // knows; try again next tick.
                                        skip_record = true;
                                    } else {
                                        entry_flags |= FS_ENTRY_UNSTABLE;
                                    }
                                }
                                ContentRead::Unreadable => {
                                    // The read raced a delete/permission
                                    // flip between the reconciler's stat and
                                    // our read. Re-read next tick so a
                                    // transiently unreadable file still
                                    // converges; diff alone would never
                                    // revisit it (stat may be unchanged).
                                    self.held.remove(path);
                                    self.retry.insert(path.clone());
                                    if was_retry {
                                        skip_record = true;
                                    } else {
                                        entry_flags |= FS_ENTRY_UNREADABLE;
                                    }
                                }
                            }
                        }
                        // Metadata-only change on a file whose content the
                        // client already holds: no content section, no
                        // NO_CONTENT flag — the mirror keeps its bytes.
                    }
                    if skip_record {
                        continue;
                    }
                    let content = match (&delta, &full) {
                        (Some(ops), _) => FsContent::Delta(ops),
                        (None, Some(data)) => FsContent::Full(data.as_slice()),
                        (None, None) => FsContent::None,
                    };
                    append_fs_record(
                        &mut buf,
                        &FsRecord::Upsert {
                            path,
                            entry_flags,
                            size: meta.size,
                            mtime_ns: meta.mtime_ns,
                            mode: meta.mode,
                            hash,
                            content,
                        },
                    );
                }
            }
            if buf.len() >= self.opts.batch_target {
                self.send_update(std::mem::take(&mut buf), &mut reset_pending, false)?;
            }
        }
        // Final update: carries the remaining records, and for the initial
        // series the SYNC flag (an empty SYNC-only update is valid and
        // terminates a snapshot whose records all fit earlier batches).
        if !buf.is_empty() || initial {
            self.send_update(buf, &mut reset_pending, initial)?;
        }
        Ok(())
    }

    /// Content for one file: from the blob store when any sync has already
    /// hashed these bytes, from a verified disk read otherwise — feeding
    /// the store and teaching the reconciler the hash so other syncs skip
    /// the read entirely.
    fn read_content(&self, path: &str, meta: &NodeMeta) -> ContentRead {
        if meta.hash != 0
            && let Some(data) = blob_store().lock().unwrap().get(meta.hash)
        {
            return ContentRead::Stable {
                hash: meta.hash,
                data,
            };
        }
        let Some(abs) = resolve_wire_path(&self.root, path) else {
            return ContentRead::Unreadable;
        };
        match read_verified_meta(&abs) {
            ReadMetaOutcome::Stable(data, mut stat) => {
                let hash = blake3_128(&data);
                let data = Arc::new(data);
                blob_store().lock().unwrap().put(hash, data.clone());
                stat.hash = hash;
                // Racily-clean guard (docs/fs-watch.md): a file whose mtime
                // is within one coarse granule of now could be rewritten
                // again inside the same granule without changing its stat.
                // Don't teach the reconciler such a hash, or another sync
                // could later serve stale bytes by it. The blob store still
                // caches the bytes (only reachable via a matching hash).
                if !racily_clean(stat.mtime_ns) {
                    let _ = self.shared.tx.send(RootMsg::HashLearned {
                        path: path.to_string(),
                        meta: stat,
                    });
                }
                ContentRead::Stable { hash, data }
            }
            ReadMetaOutcome::Unstable => ContentRead::Unstable,
            ReadMetaOutcome::Unreadable => ContentRead::Unreadable,
        }
    }

    /// Rename the `from` subtree to `to` in the held-content map and the
    /// retry set, mirroring what a `MOVE` record does to the client's map.
    /// Keeping `retry` in step is essential: a file that was reported
    /// `UNSTABLE` and then renamed within the same settle window must still
    /// be re-read at its new path, or its content never arrives.
    fn rekey_move(&mut self, from: &str, to: &str) {
        let moved: Vec<(String, u128)> = self
            .held
            .iter()
            .filter(|(path, _)| is_under(path, from))
            .map(|(path, &hash)| (path.clone(), hash))
            .collect();
        for (path, _) in &moved {
            self.held.remove(path);
        }
        for (path, hash) in moved {
            self.held.insert(rebase_subtree_path(&path, from, to), hash);
        }
        let retried: Vec<String> = self
            .retry
            .iter()
            .filter(|path| is_under(path, from))
            .cloned()
            .collect();
        for path in &retried {
            self.retry.remove(path);
        }
        for path in retried {
            self.retry.insert(rebase_subtree_path(&path, from, to));
        }
    }

    /// Send one update, first blocking until the ack window has credit.
    fn send_update(
        &mut self,
        records: Vec<u8>,
        reset_pending: &mut bool,
        sync: bool,
    ) -> Result<(), Exit> {
        self.wait_for_credit()?;
        let mut flags = 0u8;
        if *reset_pending {
            flags |= FS_UPDATE_RESET;
            *reset_pending = false;
        }
        if sync {
            flags |= FS_UPDATE_SYNC;
        }
        let update_id = self.next_update_id;
        self.next_update_id = self.next_update_id.wrapping_add(1);
        self.highest_sent = update_id;
        let msg = msg_fs_update(self.sync_id, update_id, flags, &records);
        self.unacked.push_back((update_id, msg.len()));
        self.unacked_bytes += msg.len();
        if !(self.outbox)(msg) {
            return Err(Exit::ClientGone);
        }
        Ok(())
    }

    /// Block until unacked bytes drop under the window. Commands are served
    /// while waiting; snapshots accumulate for the next tick.
    fn wait_for_credit(&mut self) -> Result<(), Exit> {
        while self.unacked_bytes >= self.opts.window_bytes {
            match self.rx.recv() {
                Ok(SyncMsg::Cmd(Command::Ack(id))) => self.handle_ack(id)?,
                Ok(SyncMsg::Cmd(Command::Fetch { nonce, path })) => {
                    if !self.handle_fetch(nonce, &path) {
                        return Err(Exit::ClientGone);
                    }
                }
                Ok(SyncMsg::Cmd(Command::Write(w))) => {
                    if !self.handle_write(w) {
                        return Err(Exit::ClientGone);
                    }
                }
                Ok(SyncMsg::Cmd(Command::Op(o))) => {
                    if !self.handle_op(o) {
                        return Err(Exit::ClientGone);
                    }
                }
                Ok(SyncMsg::Cmd(Command::Stop)) => return Err(Exit::Stopped),
                Ok(SyncMsg::Root(update)) => self.handle_root(update)?,
                Err(_) => return Err(Exit::ClientGone),
            }
        }
        Ok(())
    }

    fn handle_fetch(&mut self, nonce: u16, wire_path: &str) -> bool {
        let msg = match resolve_wire_path(&self.root, wire_path) {
            None => msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[]),
            // Refuse oversized files before reading a byte: an FS_FILE
            // whose decompressed payload exceeds the protocol cap could not
            // be parsed by a compliant client anyway, and reading it would
            // spike transient memory unbounded (docs/fs-watch.md).
            Some(abs)
                if stat_meta(&abs).map(|m| m.size).unwrap_or(0)
                    > blit_remote::fs::FS_MAX_DECOMPRESSED as u64 =>
            {
                msg_fs_file(nonce, blit_remote::fs::FS_FILE_OTHER, &[])
            }
            Some(abs) => match read_verified(&abs) {
                ReadOutcome::Stable(data) => msg_fs_file(nonce, FS_FILE_OK, &data),
                ReadOutcome::Unstable => msg_fs_file(nonce, FS_FILE_UNREADABLE, &[]),
                ReadOutcome::Unreadable => {
                    if abs.exists() {
                        msg_fs_file(nonce, FS_FILE_UNREADABLE, &[])
                    } else {
                        msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[])
                    }
                }
            },
        };
        (self.outbox)(msg)
    }

    fn handle_write(&mut self, w: WriteReq) -> bool {
        let (status, hash, mtime_ns) = self.exec_write(&w);
        (self.outbox)(msg_fs_done(w.nonce, status, hash, mtime_ns))
    }

    /// Land a content write under the target's per-file write lock: confine
    /// the path, enforce the CAS precondition against the freshly re-read
    /// live hash, write atomically (or create-exclusive), then prime the
    /// echo.
    fn exec_write(&mut self, w: &WriteReq) -> (u8, u128, u64) {
        use blit_remote::fs::FS_WRITE_CONTENT_DELTA;
        // v1 accepts only full content (0/1); delta is a v2 encoding.
        if w.content_kind == FS_WRITE_CONTENT_DELTA {
            return (FS_DONE_INVALID, 0, 0);
        }
        if w.content.len() as u64 > fs_write_max() {
            return (FS_DONE_TOO_LARGE, 0, 0);
        }
        if w.flags & FS_WRITE_MKPARENTS != 0
            && let Some(parent) = resolve_wire_path(&self.root, &w.path)
                .and_then(|a| a.parent().map(Path::to_path_buf))
            && let Err(status) = create_parents_confined(&self.root, &parent)
        {
            return (status, 0, 0);
        }
        let policy = if w.flags & FS_WRITE_FOLLOW_SYMLINK != 0 {
            SymlinkPolicy::Follow
        } else {
            SymlinkPolicy::Refuse
        };
        let target = match resolve_write_target(&self.root, &w.path, policy) {
            Ok(t) => t,
            Err(status) => return (status, 0, 0),
        };
        let durable = w.flags & FS_WRITE_DURABLE != 0;
        let no_cas = w.flags & FS_WRITE_NO_CAS != 0;

        // Serialize check-and-write against every other blit writer of this
        // exact file — including ones reaching it through a different root.
        // The guard owns its Arc, leaving `self` free for the `&mut self`
        // echo priming below.
        let lock = path_write_lock(&target);
        let _guard = lock.lock().unwrap();

        // Never clobber a directory with a file.
        if fs::symlink_metadata(&target)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return (FS_DONE_WRONG_TYPE, 0, 0);
        }

        let create_exclusive_mode = !no_cas && w.base == 0;
        if !no_cas {
            if w.base == 0 {
                if target.exists() {
                    return (FS_DONE_CONFLICT, current_hash(&target), 0);
                }
            } else {
                let cur = current_hash(&target);
                if cur != w.base {
                    return (FS_DONE_CONFLICT, cur, 0);
                }
            }
        }

        let hash = blake3_128(&w.content);
        if create_exclusive_mode {
            match create_exclusive(&target, &w.content, w.mode, durable) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return (FS_DONE_CONFLICT, current_hash(&target), 0);
                }
                Err(e) => return (write_io_status(&e), 0, 0),
            }
        } else if let Err(e) = write_atomic(&target, &w.content, w.mode, durable) {
            return (write_io_status(&e), 0, 0);
        }

        let mtime_ns = stat_meta(&target).map(|m| m.mtime_ns).unwrap_or(0);
        // Key the echo by the path the write actually landed under — which
        // is the resolved target, not the client's wire path, when a
        // symlink was followed. Otherwise the two coincide.
        let echo_wire = wire_key_for(&self.root, &target).unwrap_or_else(|| w.path.clone());
        self.prime_echo(&echo_wire, &target, hash, &w.content, mtime_ns);
        (FS_DONE_OK, hash, mtime_ns)
    }

    fn handle_op(&mut self, o: OpReq) -> bool {
        let (status, hash, mtime_ns) = self.exec_op(&o);
        (self.outbox)(msg_fs_done(o.nonce, status, hash, mtime_ns))
    }

    /// Execute a metadata op (mkdir/remove/rename), each under the affected
    /// path's per-file write lock.
    fn exec_op(&mut self, o: &OpReq) -> (u8, u128, u64) {
        match o.op {
            FS_OP_MKDIR => {
                if o.flags & FS_OP_MKPARENTS != 0
                    && let Some(parent) = resolve_wire_path(&self.root, &o.a)
                        .and_then(|a| a.parent().map(Path::to_path_buf))
                    && let Err(status) = create_parents_confined(&self.root, &parent)
                {
                    return (status, 0, 0);
                }
                let target = match resolve_write_target(&self.root, &o.a, SymlinkPolicy::Operate) {
                    Ok(t) => t,
                    Err(status) => return (status, 0, 0),
                };
                let lock = path_write_lock(&target);
                let _guard = lock.lock().unwrap();
                let mut builder = fs::DirBuilder::new();
                #[cfg(unix)]
                if o.mode != 0 {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(o.mode);
                }
                match builder.create(&target) {
                    Ok(()) => {}
                    // Idempotent when the path is already a directory.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        if !target.is_dir() {
                            return (FS_DONE_CONFLICT, 0, 0);
                        }
                    }
                    Err(e) => return (write_io_status(&e), 0, 0),
                }
                let mtime_ns = stat_meta(&target).map(|m| m.mtime_ns).unwrap_or(0);
                self.hint_change(&target);
                (FS_DONE_OK, 0, mtime_ns)
            }
            FS_OP_REMOVE => {
                let target = match resolve_write_target(&self.root, &o.a, SymlinkPolicy::Operate) {
                    Ok(t) => t,
                    Err(status) => return (status, 0, 0),
                };
                let lock = path_write_lock(&target);
                let _guard = lock.lock().unwrap();
                let md = match fs::symlink_metadata(&target) {
                    Ok(m) => m,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        return (FS_DONE_NOT_FOUND, 0, 0);
                    }
                    Err(e) => return (write_io_status(&e), 0, 0),
                };
                // Conditional remove is meaningful only for a regular file.
                if o.flags & FS_OP_NO_CAS == 0 && o.base != 0 {
                    let cur = current_hash(&target);
                    if cur != o.base {
                        return (FS_DONE_CONFLICT, cur, 0);
                    }
                }
                let res = if md.file_type().is_dir() {
                    fs::remove_dir_all(&target)
                } else {
                    // A symlink is unlinked, never followed.
                    fs::remove_file(&target)
                };
                if let Err(e) = res {
                    return (write_io_status(&e), 0, 0);
                }
                self.hint_change(&target);
                (FS_DONE_OK, 0, 0)
            }
            FS_OP_RENAME => {
                if o.flags & FS_OP_MKPARENTS != 0
                    && let Some(parent) = resolve_wire_path(&self.root, &o.b)
                        .and_then(|a| a.parent().map(Path::to_path_buf))
                    && let Err(status) = create_parents_confined(&self.root, &parent)
                {
                    return (status, 0, 0);
                }
                let from = match resolve_write_target(&self.root, &o.a, SymlinkPolicy::Operate) {
                    Ok(t) => t,
                    Err(status) => return (status, 0, 0),
                };
                let lock = path_write_lock(&from);
                let _guard = lock.lock().unwrap();
                if fs::symlink_metadata(&from).is_err() {
                    return (FS_DONE_NOT_FOUND, 0, 0);
                }
                let to = match resolve_write_target(&self.root, &o.b, SymlinkPolicy::Operate) {
                    Ok(t) => t,
                    Err(status) => return (status, 0, 0),
                };
                if let Err(e) = fs::rename(&from, &to) {
                    return (write_io_status(&e), 0, 0);
                }
                self.hint_change(&from);
                self.hint_change(&to);
                (FS_DONE_OK, 0, 0)
            }
            FS_OP_SYMLINK | FS_OP_HARDLINK => self.exec_link(o),
            _ => (FS_DONE_INVALID, 0, 0),
        }
    }

    /// Create a link at `b`: a symlink whose target is the verbatim string
    /// `a` (`SYMLINK`), or a hard link to the regular file at `a`
    /// (`HARDLINK`). `base` CASes on the entry currently at `b` exactly as
    /// a write's `base` does on its path — zero = create-exclusive,
    /// non-zero = replace iff the current content hash matches (a symlink
    /// hashes its target bytes), `NO_CAS` = unconditional. Replacement is
    /// atomic: the new link lands at a sibling temp path and renames over
    /// `b`, so a reader sees the old entry or the new, never neither.
    fn exec_link(&mut self, o: &OpReq) -> (u8, u128, u64) {
        if o.flags & FS_OP_MKPARENTS != 0
            && let Some(parent) =
                resolve_wire_path(&self.root, &o.b).and_then(|b| b.parent().map(Path::to_path_buf))
            && let Err(status) = create_parents_confined(&self.root, &parent)
        {
            return (status, 0, 0);
        }
        // A hard-link source is a confined wire path and must be a regular
        // file (aliasing a symlink or a directory is refused). A symlink
        // target is a verbatim string stored as given: in-tree relative,
        // absolute, and dangling targets are all legitimate symlinks — the
        // read side reports them, never follows (docs/design/fs-watch.md).
        let src = if o.op == FS_OP_HARDLINK {
            let src = match resolve_write_target(&self.root, &o.a, SymlinkPolicy::Operate) {
                Ok(t) => t,
                Err(status) => return (status, 0, 0),
            };
            match fs::symlink_metadata(&src) {
                Ok(md) if md.file_type().is_file() => {}
                Ok(_) => return (FS_DONE_WRONG_TYPE, 0, 0),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return (FS_DONE_NOT_FOUND, 0, 0);
                }
                Err(e) => return (write_io_status(&e), 0, 0),
            }
            Some(src)
        } else {
            if o.a.is_empty() {
                return (FS_DONE_INVALID, 0, 0);
            }
            None
        };
        let link = match resolve_write_target(&self.root, &o.b, SymlinkPolicy::Operate) {
            Ok(t) => t,
            Err(status) => return (status, 0, 0),
        };
        let lock = path_write_lock(&link);
        let _guard = lock.lock().unwrap();
        // Never clobber a directory with a link (a symlink *to* a directory
        // at `b` is itself a link entry and may be replaced).
        if fs::symlink_metadata(&link)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return (FS_DONE_WRONG_TYPE, 0, 0);
        }
        let no_cas = o.flags & FS_OP_NO_CAS != 0;
        let create_exclusive_mode = !no_cas && o.base == 0;
        if !no_cas {
            if o.base == 0 {
                // symlink_metadata, not exists(): a dangling symlink at `b`
                // is an entry and must fail create-exclusive.
                if fs::symlink_metadata(&link).is_ok() {
                    return (FS_DONE_CONFLICT, current_hash(&link), 0);
                }
            } else {
                let cur = current_hash(&link);
                if cur != o.base {
                    return (FS_DONE_CONFLICT, cur, 0);
                }
            }
        }
        let create = |at: &Path| -> io::Result<()> {
            match &src {
                Some(src) => fs::hard_link(src, at),
                None => symlink_at(&o.a, at),
            }
        };
        if create_exclusive_mode {
            // symlink()/link() fail EEXIST natively, so create-exclusive is
            // race-free even against an external creator.
            match create(&link) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return (FS_DONE_CONFLICT, current_hash(&link), 0);
                }
                Err(e) => return (write_io_status(&e), 0, 0),
            }
        } else {
            let tmp = temp_sibling(&link);
            if let Err(e) = create(&tmp) {
                return (write_io_status(&e), 0, 0);
            }
            if let Err(e) = fs::rename(&tmp, &link) {
                let _ = fs::remove_file(&tmp);
                return (write_io_status(&e), 0, 0);
            }
        }
        let mtime_ns = stat_meta(&link).map(|m| m.mtime_ns).unwrap_or(0);
        let echo_wire = wire_key_for(&self.root, &link).unwrap_or_else(|| o.b.clone());
        match &src {
            None => {
                let hash = blake3_128(o.a.as_bytes());
                self.prime_echo(&echo_wire, &link, hash, o.a.as_bytes(), mtime_ns);
                (FS_DONE_OK, hash, mtime_ns)
            }
            Some(src) => {
                // The link's content is the source file's. Hash it for the
                // echo when the bytes are stable and modestly sized; a huge
                // or in-flux source just lets the reconciler learn lazily.
                let small = fs::symlink_metadata(src)
                    .map(|m| m.len() <= fs_write_max())
                    .unwrap_or(false);
                match if small {
                    read_verified(&link)
                } else {
                    ReadOutcome::Unstable
                } {
                    ReadOutcome::Stable(data) => {
                        let hash = blake3_128(&data);
                        self.prime_echo(&echo_wire, &link, hash, &data, mtime_ns);
                        (FS_DONE_OK, hash, mtime_ns)
                    }
                    _ => {
                        self.hint_change(&link);
                        (FS_DONE_OK, 0, mtime_ns)
                    }
                }
            }
        }
    }

    /// Prime the echo of a landed write: cache the bytes by hash, mark this
    /// client as already holding them (so its own UPSERT echo carries
    /// metadata, not a copy), teach the reconciler the hash, and inject a
    /// synchronous dirty hint so the change publishes in one settle window.
    fn prime_echo(&mut self, wire: &str, abs: &Path, hash: u128, bytes: &[u8], mtime_ns: u64) {
        blob_store()
            .lock()
            .unwrap()
            .put(hash, Arc::new(bytes.to_vec()));
        self.held.insert(wire.to_string(), hash);
        if !racily_clean(mtime_ns)
            && let Ok(mut meta) = stat_meta(abs)
        {
            meta.hash = hash;
            let _ = self.shared.tx.send(RootMsg::HashLearned {
                path: wire.to_string(),
                meta,
            });
        }
        self.hint_change(abs);
    }

    /// Inject a synchronous dirty hint for a path and its parent so a write
    /// or op re-enters the mirror in one settle window instead of awaiting
    /// the native watcher (which also fires and reconciles to a no-op).
    fn hint_change(&self, abs: &Path) {
        let _ = self
            .shared
            .tx
            .send(RootMsg::Hint(Hint::Dirty(abs.to_path_buf())));
        if let Some(parent) = abs.parent() {
            let _ = self
                .shared
                .tx
                .send(RootMsg::Hint(Hint::Dirty(parent.to_path_buf())));
        }
    }
}

// Close-reason aliases for readability at use sites above.
const FS_CLOSED_BACKEND_FAILED_COMPAT: u8 = blit_remote::fs::FS_CLOSED_BACKEND_FAILED;
const FS_CLOSED_PERMISSION_LOST_COMPAT: u8 = blit_remote::fs::FS_CLOSED_PERMISSION_LOST;
/// Errno smuggled through io::Error to signal the entry budget was hit.
const RESOURCE_LIMIT_ERRNO: i32 = libc_enfile();

const fn libc_enfile() -> i32 {
    23 // ENFILE everywhere we care about; only used as an internal marker
}

#[cfg(test)]
mod tests {
    use super::*;
    use blit_remote::fs::FsMirror;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blit-fssync-test-{}-{}",
            std::process::id(),
            TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_key(root: &Path) -> RootKey {
        RootKey {
            path: root.to_path_buf(),
            recursive: true,
            cross_filesystem: false,
        }
    }

    #[test]
    fn escape_roundtrip() {
        assert_eq!(escape_bytes(b"plain.txt"), "plain.txt");
        assert_eq!(escape_bytes(b"50%.txt"), "50%25.txt");
        let bad = b"a\xFFb";
        let escaped = escape_bytes(bad);
        assert_eq!(escaped, "a%FFb");
        assert_eq!(unescape_to_bytes(&escaped).unwrap(), bad.to_vec());
        assert_eq!(unescape_to_bytes("50%25.txt").unwrap(), b"50%.txt".to_vec());
    }

    #[test]
    fn wide_escape_roundtrip() {
        // Plain text passes through.
        let plain: Vec<u16> = "file.txt".encode_utf16().collect();
        assert_eq!(escape_wide(&plain), "file.txt");
        assert_eq!(unescape_to_wide("file.txt").unwrap(), plain);
        // Literal '%' escapes so "%u" in a name never collides.
        let percent: Vec<u16> = "50%u.txt".encode_utf16().collect();
        assert_eq!(escape_wide(&percent), "50%25u.txt");
        assert_eq!(unescape_to_wide("50%25u.txt").unwrap(), percent);
        // Valid surrogate pair (U+1D11E) survives as text.
        let clef: Vec<u16> = "𝄞.txt".encode_utf16().collect();
        assert_eq!(escape_wide(&clef), "𝄞.txt");
        assert_eq!(unescape_to_wide("𝄞.txt").unwrap(), clef);
        // Unpaired surrogates become %uXXXX and round-trip exactly.
        let bad = [0xD800u16, 0x0041, 0xDFFF];
        let escaped = escape_wide(&bad);
        assert_eq!(escaped, "%uD800A%uDFFF");
        assert_eq!(unescape_to_wide(&escaped).unwrap(), bad.to_vec());
        // Malformed escapes are rejected.
        assert!(unescape_to_wide("%u12").is_none());
        assert!(unescape_to_wide("%uZZZZ").is_none());
    }

    #[test]
    fn wire_path_traversal_rejected() {
        let root = Path::new("/tmp/root");
        assert!(resolve_wire_path(root, "a/../b").is_none());
        assert!(resolve_wire_path(root, "..").is_none());
        assert!(resolve_wire_path(root, "a//b").is_none());
        assert_eq!(resolve_wire_path(root, ""), Some(root.to_path_buf()));
        assert_eq!(
            resolve_wire_path(root, "a/b"),
            Some(root.join("a").join("b"))
        );
    }

    /// Traversal must be rejected even when the dot-dot or separator is
    /// percent-encoded: the `.`/`..`/empty and embedded-`/` checks run
    /// against the *decoded* component, not the escaped wire text, so a
    /// crafted `FS_FETCH` cannot climb out of the synced root. (A
    /// well-behaved peer never sends these — the server escapes `.` as
    /// `.` and `/` as a separator — but the resolver must not trust the
    /// client's encoding.)
    #[test]
    fn encoded_traversal_rejected() {
        let root = Path::new("/tmp/root");
        // %2E%2E decodes to "..".
        assert!(resolve_wire_path(root, "%2E%2E").is_none());
        assert!(resolve_wire_path(root, "%2e%2e/etc/passwd").is_none());
        // %2E decodes to ".".
        assert!(resolve_wire_path(root, "%2E").is_none());
        // An embedded encoded separator smuggles two components past a
        // per-component check.
        assert!(resolve_wire_path(root, "a%2F..%2Fb").is_none());
        assert!(resolve_wire_path(root, "a%2Fb").is_none());
        // A genuine name that merely contains a percent still resolves.
        assert_eq!(resolve_wire_path(root, "%2525"), Some(root.join("%25")));
    }

    fn meta(node_type: u8, size: u64, mtime: u64, ino: u64) -> NodeMeta {
        NodeMeta {
            node_type,
            size,
            mtime_ns: mtime,
            mode: 0o644,
            hash: 0,
            dev_ino: (1, ino),
        }
    }

    #[test]
    fn diff_detects_directory_move() {
        let mut prev = Index::new();
        prev.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        prev.insert("d".into(), meta(FS_ENTRY_DIR, 0, 0, 2));
        prev.insert("d/f".into(), meta(FS_ENTRY_FILE, 5, 10, 3));
        let mut curr = Index::new();
        curr.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        curr.insert("e".into(), meta(FS_ENTRY_DIR, 0, 0, 2));
        curr.insert("e/f".into(), meta(FS_ENTRY_FILE, 5, 10, 3));
        let ops = diff(&prev, &curr);
        assert_eq!(
            ops,
            vec![DiffOp::Move {
                from: "d".into(),
                to: "e".into()
            }]
        );
    }

    /// A MOVE must not swallow same-window changes inside the moved
    /// subtree: modified, created, and deleted children all need fix-ups.
    #[test]
    fn diff_move_with_same_window_child_changes() {
        let mut prev = Index::new();
        prev.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        prev.insert("d".into(), meta(FS_ENTRY_DIR, 0, 50, 2));
        prev.insert("d/modified".into(), meta(FS_ENTRY_FILE, 5, 10, 3));
        prev.insert("d/deleted".into(), meta(FS_ENTRY_FILE, 5, 10, 4));
        let mut curr = Index::new();
        curr.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        curr.insert("e".into(), meta(FS_ENTRY_DIR, 0, 50, 2));
        curr.insert("e/modified".into(), meta(FS_ENTRY_FILE, 999, 777, 3));
        curr.insert("e/created".into(), meta(FS_ENTRY_FILE, 1, 900, 9));
        let ops = diff(&prev, &curr);
        assert!(ops.contains(&DiffOp::Move {
            from: "d".into(),
            to: "e".into()
        }));
        assert!(
            ops.contains(&DiffOp::Upsert {
                path: "e/modified".into(),
                content_changed: true
            }),
            "modified child swallowed: {ops:?}"
        );
        assert!(
            ops.contains(&DiffOp::Upsert {
                path: "e/created".into(),
                content_changed: true
            }),
            "created child swallowed: {ops:?}"
        );
        assert!(
            ops.contains(&DiffOp::Delete {
                path: "e/deleted".into()
            }),
            "deleted child swallowed: {ops:?}"
        );
    }

    /// Drive one engine over a shared root and apply every update to a
    /// mirror, acking as we go. Returns (mirror, sent-log, handle, hints).
    #[cfg(unix)]
    fn drive_engine(root: &Path) -> (Arc<Mutex<Vec<Vec<u8>>>>, SyncHandle, HintSender) {
        let shared = open_root_unwatched(test_key(root));
        let hint_tx = shared.hint_sender();
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = SyncOptions {
            content: true,
            latency: Duration::from_millis(5),
            ..Default::default()
        };
        let handle = start_sync(
            &shared,
            1,
            opts,
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );
        (sent, handle, hint_tx)
    }

    /// Send a command and block until the `FS_DONE` for `nonce` arrives.
    fn await_done(
        handle: &SyncHandle,
        sent: &Arc<Mutex<Vec<Vec<u8>>>>,
        nonce: u16,
        cmd: Command,
    ) -> (u8, u128, u64) {
        handle.command(cmd);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for msg in sent.lock().unwrap().iter() {
                if let Some((n, s, h, m)) = blit_remote::fs::parse_fs_done(msg)
                    && n == nonce
                {
                    return (s, h, m);
                }
            }
            assert!(Instant::now() < deadline, "no FS_DONE for nonce {nonce}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn write_req(nonce: u16, path: &str, base: u128, flags: u8, content: &[u8]) -> Command {
        Command::Write(WriteReq {
            nonce,
            path: path.into(),
            base,
            mode: 0,
            flags,
            content_kind: 1,
            content: content.to_vec(),
            inflight: None,
        })
    }

    #[test]
    fn write_cas_semantics() {
        // Production always canonicalizes the root (validate_root); the
        // write guard relies on it.
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        // Create-exclusive (base 0): first ok, second conflicts with the
        // current disk hash.
        let (s, hash, _) = await_done(&handle, &sent, 1, write_req(1, "a.txt", 0, 0, b"hello"));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"hello");
        assert_eq!(hash, blake3_128(b"hello"));
        let (s, disk, _) = await_done(&handle, &sent, 2, write_req(2, "a.txt", 0, 0, b"x"));
        assert_eq!(s, FS_DONE_CONFLICT);
        assert_eq!(disk, hash, "conflict carries the live disk hash");
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"hello", "unchanged");

        // CAS overwrite: correct base succeeds, a stale base conflicts.
        let (s, h2, _) = await_done(&handle, &sent, 3, write_req(3, "a.txt", hash, 0, b"world"));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(h2, blake3_128(b"world"));
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"world");
        let (s, _, _) = await_done(&handle, &sent, 4, write_req(4, "a.txt", hash, 0, b"z"));
        assert_eq!(s, FS_DONE_CONFLICT, "stale base rejected");

        // NO_CAS overwrites unconditionally.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            5,
            write_req(5, "a.txt", 0, FS_WRITE_NO_CAS, b"forced"),
        );
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"forced");

        // MKPARENTS creates the chain.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            6,
            write_req(6, "d/e/f.txt", 0, FS_WRITE_MKPARENTS, b"deep"),
        );
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(fs::read(root.join("d/e/f.txt")).unwrap(), b"deep");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_refuses_traversal() {
        // Production always canonicalizes the root (validate_root); the
        // write guard relies on it.
        let root = temp_dir().canonicalize().unwrap();
        let sibling = root.parent().unwrap().join("blit-escape-victim.txt");
        let _ = fs::remove_file(&sibling);
        let (sent, handle, _hint) = drive_engine(&root);

        // Plain and percent-encoded dot-dot both refuse and write nothing.
        for (i, p) in ["../blit-escape-victim.txt", "%2E%2E/blit-escape-victim.txt"]
            .iter()
            .enumerate()
        {
            let (s, _, _) = await_done(
                &handle,
                &sent,
                i as u16 + 1,
                write_req(i as u16 + 1, p, 0, 0, b"pwn"),
            );
            assert_eq!(s, FS_DONE_INVALID, "traversal {p} must be refused");
        }
        assert!(!sibling.exists(), "nothing escaped the root");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn fs_ops_mkdir_rename_remove() {
        // Production always canonicalizes the root (validate_root); the
        // write guard relies on it.
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);
        let op = |nonce: u16, op: u8, a: &str, b: &str, base: u128, flags: u8| {
            Command::Op(OpReq {
                nonce,
                op,
                a: a.into(),
                b: b.into(),
                base,
                mode: 0,
                flags,
                inflight: None,
            })
        };

        // mkdir
        let (s, _, _) = await_done(&handle, &sent, 1, op(1, FS_OP_MKDIR, "sub", "", 0, 0));
        assert_eq!(s, FS_DONE_OK);
        assert!(root.join("sub").is_dir());
        // idempotent
        let (s, _, _) = await_done(&handle, &sent, 2, op(2, FS_OP_MKDIR, "sub", "", 0, 0));
        assert_eq!(s, FS_DONE_OK);

        // write then rename
        let (_, _, _) = await_done(&handle, &sent, 3, write_req(3, "sub/x.txt", 0, 0, b"hi"));
        let (s, _, _) = await_done(
            &handle,
            &sent,
            4,
            op(4, FS_OP_RENAME, "sub/x.txt", "sub/y.txt", 0, 0),
        );
        assert_eq!(s, FS_DONE_OK);
        assert!(!root.join("sub/x.txt").exists());
        assert_eq!(fs::read(root.join("sub/y.txt")).unwrap(), b"hi");

        // rename of a missing source is NOT_FOUND
        let (s, _, _) = await_done(
            &handle,
            &sent,
            5,
            op(5, FS_OP_RENAME, "sub/gone.txt", "sub/z.txt", 0, 0),
        );
        assert_eq!(s, FS_DONE_NOT_FOUND);

        // remove the subtree
        let (s, _, _) = await_done(&handle, &sent, 6, op(6, FS_OP_REMOVE, "sub", "", 0, 0));
        assert_eq!(s, FS_DONE_OK);
        assert!(!root.join("sub").exists());
        // removing a missing path is NOT_FOUND
        let (s, _, _) = await_done(&handle, &sent, 7, op(7, FS_OP_REMOVE, "sub", "", 0, 0));
        assert_eq!(s, FS_DONE_NOT_FOUND);

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// Symlink and hard-link ops: create-exclusive, CAS retarget, conflict
    /// carrying the live target hash, type refusals, and the read side
    /// treating a symlink's target as its content (mirror and FETCH).
    #[cfg(unix)]
    #[test]
    fn fs_ops_symlink_hardlink() {
        // Production always canonicalizes the root (validate_root); the
        // write guard relies on it.
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, hint) = drive_engine(&root);
        let op = |nonce: u16, op: u8, a: &str, b: &str, base: u128, flags: u8| {
            Command::Op(OpReq {
                nonce,
                op,
                a: a.into(),
                b: b.into(),
                base,
                mode: 0,
                flags,
                inflight: None,
            })
        };

        // Create-exclusive symlink; the returned hash covers the target.
        let (s, h, _) = await_done(&handle, &sent, 1, op(1, FS_OP_SYMLINK, "a.txt", "ln", 0, 0));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(fs::read_link(root.join("ln")).unwrap(), Path::new("a.txt"));
        assert_eq!(h, blake3_128(b"a.txt"));
        // An existing entry conflicts, carrying the live target hash.
        let (s, disk, _) = await_done(&handle, &sent, 2, op(2, FS_OP_SYMLINK, "other", "ln", 0, 0));
        assert_eq!(s, FS_DONE_CONFLICT);
        assert_eq!(disk, h);
        // CAS retarget: the correct base wins…
        let (s, h2, _) = await_done(&handle, &sent, 3, op(3, FS_OP_SYMLINK, "b.txt", "ln", h, 0));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(h2, blake3_128(b"b.txt"));
        assert_eq!(fs::read_link(root.join("ln")).unwrap(), Path::new("b.txt"));
        // …and a stale base conflicts.
        let (s, _, _) = await_done(&handle, &sent, 4, op(4, FS_OP_SYMLINK, "c", "ln", h, 0));
        assert_eq!(s, FS_DONE_CONFLICT);
        // NO_CAS replaces unconditionally; a dangling target is legitimate.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            5,
            op(5, FS_OP_SYMLINK, "gone/dangling", "ln", 0, FS_OP_NO_CAS),
        );
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(
            fs::read_link(root.join("ln")).unwrap(),
            Path::new("gone/dangling")
        );
        // A directory at the link path refuses.
        fs::create_dir(root.join("d")).unwrap();
        let (s, _, _) = await_done(
            &handle,
            &sent,
            6,
            op(6, FS_OP_SYMLINK, "x", "d", 0, FS_OP_NO_CAS),
        );
        assert_eq!(s, FS_DONE_WRONG_TYPE);

        // Hard link: same content hash as the source, same inode.
        let (s, fh, _) = await_done(&handle, &sent, 10, write_req(10, "f.txt", 0, 0, b"hello"));
        assert_eq!(s, FS_DONE_OK);
        let (s, lh, _) = await_done(
            &handle,
            &sent,
            11,
            op(11, FS_OP_HARDLINK, "f.txt", "f2.txt", 0, 0),
        );
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(lh, fh);
        assert_eq!(fs::read(root.join("f2.txt")).unwrap(), b"hello");
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(root.join("f.txt")).unwrap().ino(),
                fs::metadata(root.join("f2.txt")).unwrap().ino()
            );
        }
        // Create-exclusive on an existing destination conflicts.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            12,
            op(12, FS_OP_HARDLINK, "f.txt", "f2.txt", 0, 0),
        );
        assert_eq!(s, FS_DONE_CONFLICT);
        // The source must be a regular file; a symlink source refuses.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            13,
            op(13, FS_OP_HARDLINK, "ln", "ln2", 0, 0),
        );
        assert_eq!(s, FS_DONE_WRONG_TYPE);
        // A missing source is NOT_FOUND.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            14,
            op(14, FS_OP_HARDLINK, "nope", "n2", 0, 0),
        );
        assert_eq!(s, FS_DONE_NOT_FOUND);

        // The writer's own echo for "ln" is metadata-only (prime_echo marks
        // it as held), but must still carry the target hash. An externally
        // created symlink syncs with its target as inline content.
        std::os::unix::fs::symlink("ext-target", root.join("ext")).unwrap();
        hint.send(Hint::Dirty(root.join("ext")));
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for msg in sent.lock().unwrap().clone()[seen..].iter() {
                seen += 1;
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let id = mirror.apply_update(msg).unwrap();
                    handle.command(Command::Ack(id));
                }
            }
            if mirror
                .live
                .get("ext")
                .is_some_and(|n| n.content.as_deref() == Some(&b"ext-target"[..]))
                && mirror.live.contains_key("ln")
            {
                break;
            }
            assert!(Instant::now() < deadline, "symlink content never synced");
            std::thread::sleep(Duration::from_millis(2));
        }
        let node = mirror.live.get("ext").unwrap();
        assert_eq!(node.entry_flags & FS_ENTRY_TYPE_MASK, FS_ENTRY_SYMLINK);
        assert_eq!(node.hash, blake3_128(b"ext-target"));
        assert_eq!(node.size, "ext-target".len() as u64);
        let own = mirror.live.get("ln").unwrap();
        assert_eq!(own.entry_flags & FS_ENTRY_TYPE_MASK, FS_ENTRY_SYMLINK);
        assert_eq!(own.hash, blake3_128(b"gone/dangling"));
        handle.command(Command::Fetch {
            nonce: 20,
            path: "ln".into(),
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        'fetch: loop {
            for msg in sent.lock().unwrap().iter() {
                if msg[0] == blit_remote::fs::S2C_FS_FILE
                    && let Some((20, status, data)) = blit_remote::fs::parse_fs_file(msg)
                {
                    assert_eq!(status, FS_FILE_OK);
                    assert_eq!(data, b"gone/dangling");
                    break 'fetch;
                }
            }
            assert!(Instant::now() < deadline, "no FS_FILE for the symlink");
            std::thread::sleep(Duration::from_millis(2));
        }

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// Finding: a transiently-unreadable file must not poison the mirror.
    /// After the read races a permission flip, the retry set re-reads it
    /// once readable, so content still converges.
    #[cfg(unix)]
    #[test]
    fn unreadable_content_recovers_when_readable() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir();
        let file = root.join("secret.txt");
        fs::write(&file, b"classified").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        // Skip under root (chmod 000 doesn't stop root reads).
        if fs::read(&file).is_ok() {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let (sent, handle, _hint) = drive_engine(&root);

        let mut mirror = FsMirror::new();
        let mut acked = 0usize;
        let pump = |mirror: &mut FsMirror, acked: &mut usize| {
            for msg in sent.lock().unwrap().clone()[*acked..].iter() {
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let id = mirror.apply_update(msg).unwrap();
                    handle.command(Command::Ack(id));
                    *acked += 1;
                } else {
                    *acked += 1;
                }
            }
        };
        // Initial snapshot: the file is present but content-less + UNREADABLE.
        for _ in 0..200 {
            pump(&mut mirror, &mut acked);
            if let Some(node) = mirror.live.get("secret.txt")
                && node.entry_flags & FS_ENTRY_UNREADABLE != 0
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let node = mirror.live.get("secret.txt").expect("file present");
        assert_ne!(
            node.entry_flags & FS_ENTRY_UNREADABLE,
            0,
            "expected UNREADABLE"
        );
        assert!(node.content.is_none());

        // Make it readable; the retry set re-reads without any new hint.
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut mirror, &mut acked);
            if mirror.live["secret.txt"].content.as_deref() == Some(&b"classified"[..]) {
                break;
            }
            assert!(Instant::now() < deadline, "content never recovered");
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// Finding: an UNSTABLE/UNREADABLE file's pending re-read must survive a
    /// rename within the same settle window — the retry set is rekeyed by
    /// the MOVE, so content still arrives at the new path.
    #[cfg(unix)]
    #[test]
    fn retry_survives_rename() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_dir();
        let old = root.join("a.txt");
        fs::write(&old, b"payload").unwrap();
        fs::set_permissions(&old, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&old).is_ok() {
            let _ = fs::remove_dir_all(&root);
            return;
        }
        let (sent, handle, hint_tx) = drive_engine(&root);

        let mut mirror = FsMirror::new();
        let mut acked = 0usize;
        let pump = |mirror: &mut FsMirror, acked: &mut usize| {
            for msg in sent.lock().unwrap().clone()[*acked..].iter() {
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let id = mirror.apply_update(msg).unwrap();
                    handle.command(Command::Ack(id));
                }
                *acked += 1;
            }
        };
        // Wait until "a.txt" is known (UNREADABLE, content-less).
        for _ in 0..200 {
            pump(&mut mirror, &mut acked);
            if mirror.live.contains_key("a.txt") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mirror.live["a.txt"].content.is_none());

        // Make readable and rename in the same window; the pending re-read
        // must follow to "b.txt".
        fs::set_permissions(&old, fs::Permissions::from_mode(0o644)).unwrap();
        let new = root.join("b.txt");
        fs::rename(&old, &new).unwrap();
        hint_tx.send(Hint::Dirty(old));
        hint_tx.send(Hint::Dirty(new));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            pump(&mut mirror, &mut acked);
            if mirror.live.get("b.txt").and_then(|n| n.content.as_deref()) == Some(&b"payload"[..])
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "content did not follow the rename: {:?}",
                mirror.live.get("b.txt")
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!mirror.live.contains_key("a.txt"));
        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn diff_plain_changes() {
        let mut prev = Index::new();
        prev.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        prev.insert("a".into(), meta(FS_ENTRY_FILE, 1, 1, 2));
        prev.insert("b".into(), meta(FS_ENTRY_FILE, 1, 1, 3));
        let mut curr = Index::new();
        curr.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        curr.insert("a".into(), meta(FS_ENTRY_FILE, 2, 2, 2)); // grew
        curr.insert("c".into(), meta(FS_ENTRY_FILE, 1, 1, 9)); // new
        let ops = diff(&prev, &curr);
        assert!(ops.contains(&DiffOp::Delete { path: "b".into() }));
        assert!(ops.contains(&DiffOp::Upsert {
            path: "a".into(),
            content_changed: true
        }));
        assert!(ops.contains(&DiffOp::Upsert {
            path: "c".into(),
            content_changed: true
        }));
        assert_eq!(ops.len(), 3);
    }

    /// End-to-end: engine over a real directory with the fake backend;
    /// a mirror applying its updates must converge on the disk state.
    #[test]
    fn engine_converges() {
        let root = temp_dir();
        fs::write(root.join("hello.txt"), b"hello").unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/nested.txt"), b"nested").unwrap();

        let shared = open_root_unwatched(test_key(&root));
        let hint_tx = shared.hint_sender();
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = SyncOptions {
            content: true,
            latency: Duration::from_millis(5),
            ..Default::default()
        };
        let handle = start_sync(
            &shared,
            7,
            opts,
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );

        let wait_updates = |min: usize| {
            for _ in 0..200 {
                if sent.lock().unwrap().len() >= min {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for {min} updates");
        };

        wait_updates(1);
        let mut mirror = FsMirror::new();
        let mut acked = 0usize;
        let apply_all = |mirror: &mut FsMirror, acked: &mut usize| {
            let msgs = sent.lock().unwrap().clone();
            for msg in &msgs[*acked..] {
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let id = mirror.apply_update(msg).expect("valid update");
                    handle.command(Command::Ack(id));
                }
            }
            *acked = msgs.len();
        };
        apply_all(&mut mirror, &mut acked);
        assert_eq!(
            mirror.live["hello.txt"].content.as_deref(),
            Some(&b"hello"[..])
        );
        assert_eq!(
            mirror.live["sub/nested.txt"].content.as_deref(),
            Some(&b"nested"[..])
        );
        assert!(mirror.live.contains_key("")); // the root itself
        assert!(mirror.live.contains_key("sub"));

        // Mutate and hint.
        fs::write(root.join("hello.txt"), b"changed").unwrap();
        fs::remove_file(root.join("sub/nested.txt")).unwrap();
        fs::write(root.join("sub/other.txt"), b"other").unwrap();
        hint_tx.send(Hint::Dirty(root.join("hello.txt")));
        hint_tx.send(Hint::Dirty(root.join("sub")));
        wait_updates(acked + 1);
        std::thread::sleep(Duration::from_millis(30));
        apply_all(&mut mirror, &mut acked);
        assert_eq!(
            mirror.live["hello.txt"].content.as_deref(),
            Some(&b"changed"[..])
        );
        assert!(!mirror.live.contains_key("sub/nested.txt"));
        assert_eq!(
            mirror.live["sub/other.txt"].content.as_deref(),
            Some(&b"other"[..])
        );

        // Rescan hint (overflow path) must also converge, invisibly.
        fs::write(root.join("late.txt"), b"late").unwrap();
        hint_tx.send(Hint::Rescan);
        wait_updates(acked + 1);
        std::thread::sleep(Duration::from_millis(30));
        apply_all(&mut mirror, &mut acked);
        assert_eq!(
            mirror.live["late.txt"].content.as_deref(),
            Some(&b"late"[..])
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// The initial snapshot must not outrun the ack window: with a tiny
    /// window and many files, the engine stalls until acks arrive and the
    /// unacked byte total stays bounded throughout.
    #[test]
    fn snapshot_respects_ack_window() {
        let root = temp_dir();
        for i in 0..50 {
            fs::write(root.join(format!("f{i:02}.txt")), vec![b'x'; 256]).unwrap();
        }
        let shared = open_root_unwatched(test_key(&root));
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let window = 2048usize;
        let opts = SyncOptions {
            content: true,
            latency: Duration::from_millis(5),
            window_bytes: window,
            batch_target: 512,
            ..Default::default()
        };
        let handle = start_sync(
            &shared,
            3,
            opts,
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );

        let mut mirror = FsMirror::new();
        let mut applied = 0usize;
        let mut synced = false;
        for _ in 0..400 {
            std::thread::sleep(Duration::from_millis(5));
            let msgs = sent.lock().unwrap().clone();
            // Unacked bytes may exceed the window by at most one in-flight
            // update (credit is checked before each send).
            let outstanding: usize = msgs[applied..].iter().map(|m| m.len()).sum();
            let max_update = msgs.iter().map(|m| m.len()).max().unwrap_or(0);
            assert!(
                outstanding <= window + max_update,
                "engine outran the window: {outstanding} unacked bytes"
            );
            for msg in &msgs[applied..] {
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let flags = msg[7];
                    let id = mirror.apply_update(msg).expect("valid update");
                    handle.command(Command::Ack(id));
                    if flags & FS_UPDATE_SYNC != 0 {
                        synced = true;
                    }
                }
            }
            applied = msgs.len();
            if synced {
                break;
            }
        }
        assert!(synced, "snapshot never reached SYNC");
        assert_eq!(
            mirror
                .live
                .iter()
                .filter(|(_, n)| n.content.is_some())
                .count(),
            50
        );
        // Multiple bounded updates, not one giant one.
        assert!(
            applied > 5,
            "expected a paced series, got {applied} updates"
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// Full path: real notify backend → hints → engine → mirror.
    #[test]
    fn native_backend_delivers_changes() {
        // Canonicalize like `validate_root` does in production: on macOS the
        // temp dir lives behind the /var → /private/var symlink, and
        // FSEvents reports resolved paths.
        let root = temp_dir().canonicalize().unwrap();
        fs::write(root.join("seed.txt"), b"seed").unwrap();

        // The watcher arms inside open_root, before the initial scan.
        let shared = open_root(test_key(&root)).expect("arm native watch");
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = SyncOptions {
            content: true,
            latency: Duration::from_millis(5),
            ..Default::default()
        };
        let handle = start_sync(
            &shared,
            9,
            opts,
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );

        let mut mirror = FsMirror::new();
        let mut applied = 0usize;
        let apply_all = |mirror: &mut FsMirror, applied: &mut usize| {
            let msgs = sent.lock().unwrap().clone();
            for msg in &msgs[*applied..] {
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let id = mirror.apply_update(msg).expect("valid update");
                    handle.command(Command::Ack(id));
                }
            }
            *applied = msgs.len();
        };

        // Initial snapshot.
        for _ in 0..200 {
            apply_all(&mut mirror, &mut applied);
            if mirror.live.contains_key("seed.txt") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(mirror.live.contains_key("seed.txt"));

        // A change observed purely through the native backend.
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/new.txt"), b"native").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            apply_all(&mut mirror, &mut applied);
            if mirror
                .live
                .get("dir/new.txt")
                .is_some_and(|n| n.content.as_deref() == Some(b"native"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "native backend never delivered the change; live = {:?}",
                mirror.live.keys().collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// The engine's single-property spec: for arbitrary mutation sequences
    /// and arbitrary ack timing, applying updates always yields the final
    /// tree.
    ///
    /// A seeded RNG drives random writes/mkdirs/removes/renames over a small
    /// path universe while the engine runs, hinting like a backend would
    /// (touched path + parent, occasional spurious rescans). Acks are
    /// withheld at random so the engine's credit-blocking path is exercised;
    /// after the last mutation the mirror must converge on exactly the
    /// on-disk tree, content included.
    #[test]
    fn property_random_mutations_converge() {
        for seed in [1u64, 7, 42, 0xdead_beef] {
            property_run(seed);
        }
    }

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn scan_disk(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        fn walk(map: &mut BTreeMap<String, Option<Vec<u8>>>, abs: &Path, rel: &str) {
            let Ok(md) = fs::symlink_metadata(abs) else {
                return;
            };
            if md.is_dir() {
                map.insert(rel.to_string(), None);
                let Ok(entries) = fs::read_dir(abs) else {
                    return;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let child_rel = if rel.is_empty() {
                        name.clone()
                    } else {
                        format!("{rel}/{name}")
                    };
                    walk(map, &entry.path(), &child_rel);
                }
            } else if md.is_file() {
                map.insert(rel.to_string(), fs::read(abs).ok());
            }
        }
        let mut map = BTreeMap::new();
        walk(&mut map, root, "");
        map
    }

    fn mirror_state(mirror: &FsMirror) -> BTreeMap<String, Option<Vec<u8>>> {
        mirror
            .live
            .iter()
            .map(|(path, node)| {
                let content = if node.entry_flags & FS_ENTRY_TYPE_MASK == FS_ENTRY_FILE {
                    node.content.clone()
                } else {
                    None
                };
                (path.clone(), content)
            })
            .collect()
    }

    /// One client of a shared root, with its own mirror and ack schedule.
    struct PropClient {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        handle: SyncHandle,
        mirror: FsMirror,
        applied: usize,
        highest_unacked: Option<u32>,
    }

    impl PropClient {
        fn start(shared: &Arc<SharedRootHandle>, sync_id: u16) -> Self {
            let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
            let sent2 = sent.clone();
            let opts = SyncOptions {
                content: true,
                latency: Duration::from_millis(3),
                window_bytes: 4096,
                batch_target: 1024,
                ..Default::default()
            };
            let handle = start_sync(
                shared,
                sync_id,
                opts,
                Box::new(move |msg| {
                    sent2.lock().unwrap().push(msg);
                    true
                }),
            );
            PropClient {
                sent,
                handle,
                mirror: FsMirror::new(),
                applied: 0,
                highest_unacked: None,
            }
        }

        /// Apply every new message; ack the highest applied id only with
        /// probability 1/2 (cumulative acks make withholding harmless for
        /// correctness — only pacing may stall until the final flush).
        fn pump(&mut self, rng: &mut u64, flush: bool) {
            use blit_remote::fs::S2C_FS_UPDATE;
            let msgs = self.sent.lock().unwrap().clone();
            for msg in &msgs[self.applied..] {
                if msg[0] == S2C_FS_UPDATE {
                    let id = self.mirror.apply_update(msg).expect("valid update");
                    self.highest_unacked = Some(id);
                }
            }
            self.applied = msgs.len();
            if let Some(id) = self.highest_unacked
                && (flush || xorshift(rng).is_multiple_of(2))
            {
                self.handle.command(Command::Ack(id));
                self.highest_unacked = None;
            }
        }
    }

    fn property_run(seed: u64) {
        let root = temp_dir();
        let shared = open_root_unwatched(test_key(&root));
        let hint_tx = shared.hint_sender();
        // Two independently paced clients of one shared root: convergence
        // must hold for both, whatever their ack schedules.
        let mut clients = [
            PropClient::start(&shared, 11),
            PropClient::start(&shared, 12),
        ];

        let mut rng = seed | 1;
        let dirs = ["", "d0", "d1", "d0/d2"];
        let names = ["f0", "f1", "f2", "f3"];

        for _round in 0..25 {
            let mutations = 1 + xorshift(&mut rng) % 3;
            for _ in 0..mutations {
                let dir = dirs[(xorshift(&mut rng) % dirs.len() as u64) as usize];
                let name = names[(xorshift(&mut rng) % names.len() as u64) as usize];
                let rel: PathBuf = if dir.is_empty() {
                    name.into()
                } else {
                    Path::new(dir).join(name)
                };
                let abs = root.join(&rel);
                match xorshift(&mut rng) % 5 {
                    // Write a file (creating parents).
                    0 | 1 => {
                        let _ = fs::create_dir_all(abs.parent().unwrap());
                        let len = (xorshift(&mut rng) % 64) as usize;
                        let byte = (xorshift(&mut rng) & 0xFF) as u8;
                        let _ = fs::write(&abs, vec![byte; len]);
                    }
                    // Make a directory.
                    2 => {
                        let _ = fs::create_dir_all(&abs);
                    }
                    // Remove whatever is there.
                    3 => {
                        if abs.is_dir() {
                            let _ = fs::remove_dir_all(&abs);
                        } else {
                            let _ = fs::remove_file(&abs);
                        }
                    }
                    // Rename to a sibling slot.
                    _ => {
                        let target = abs.with_file_name(
                            names[(xorshift(&mut rng) % names.len() as u64) as usize],
                        );
                        if target != abs {
                            let _ = fs::rename(&abs, &target);
                            hint_tx.send(Hint::Dirty(target));
                        }
                    }
                }
                // Hint like a backend: the touched path and its parent.
                hint_tx.send(Hint::Dirty(abs.clone()));
                hint_tx.send(Hint::Dirty(abs.parent().unwrap().to_path_buf()));
            }
            // Occasional loss signal: everything degrades to a rescan.
            if xorshift(&mut rng).is_multiple_of(16) {
                hint_tx.send(Hint::Rescan);
            }
            for client in &mut clients {
                client.pump(&mut rng, false);
            }
            std::thread::sleep(Duration::from_millis(xorshift(&mut rng) % 8));
        }

        // Convergence: with mutations stopped and acks flushed, every
        // client's mirror must reach exactly the on-disk state.
        let disk = scan_disk(&root);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            for client in &mut clients {
                client.pump(&mut rng, true);
            }
            if clients
                .iter()
                .all(|client| mirror_state(&client.mirror) == disk)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "seed {seed}: mirrors never converged\n first: {:?}\n second: {:?}\n disk: {:?}",
                mirror_state(&clients[0].mirror).keys().collect::<Vec<_>>(),
                mirror_state(&clients[1].mirror).keys().collect::<Vec<_>>(),
                disk.keys().collect::<Vec<_>>(),
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        for client in &clients {
            client.handle.command(Command::Stop);
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// Two opens of the same key share one root (same Arc, one reconciler),
    /// and both clients see live changes.
    #[test]
    fn shared_root_serves_multiple_clients() {
        let root = temp_dir();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        let shared = open_root_unwatched(test_key(&root));
        let joined = open_root_unwatched(test_key(&root));
        assert!(Arc::ptr_eq(&shared, &joined));
        let hint_tx = shared.hint_sender();

        let start = |sync_id: u16| {
            let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
            let sent2 = sent.clone();
            let opts = SyncOptions {
                content: true,
                latency: Duration::from_millis(5),
                ..Default::default()
            };
            let handle = start_sync(
                &shared,
                sync_id,
                opts,
                Box::new(move |msg| {
                    sent2.lock().unwrap().push(msg);
                    true
                }),
            );
            (sent, handle)
        };
        let (sent_a, handle_a) = start(21);
        let (sent_b, handle_b) = start(22);

        let converge = |sent: &Arc<Mutex<Vec<Vec<u8>>>>,
                        handle: &SyncHandle,
                        mirror: &mut FsMirror,
                        applied: &mut usize,
                        path: &str,
                        want: &[u8]| {
            for _ in 0..400 {
                let msgs = sent.lock().unwrap().clone();
                for msg in &msgs[*applied..] {
                    if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                        let id = mirror.apply_update(msg).expect("valid update");
                        handle.command(Command::Ack(id));
                    }
                }
                *applied = msgs.len();
                if mirror
                    .live
                    .get(path)
                    .is_some_and(|n| n.content.as_deref() == Some(want))
                {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("mirror never saw {path}");
        };

        let mut mirror_a = FsMirror::new();
        let mut mirror_b = FsMirror::new();
        let (mut applied_a, mut applied_b) = (0usize, 0usize);
        converge(
            &sent_a,
            &handle_a,
            &mut mirror_a,
            &mut applied_a,
            "a.txt",
            b"alpha",
        );
        converge(
            &sent_b,
            &handle_b,
            &mut mirror_b,
            &mut applied_b,
            "a.txt",
            b"alpha",
        );

        // One mutation, one hint: both clients converge on it.
        fs::write(root.join("b.txt"), b"beta").unwrap();
        hint_tx.send(Hint::Dirty(root.join("b.txt")));
        converge(
            &sent_a,
            &handle_a,
            &mut mirror_a,
            &mut applied_a,
            "b.txt",
            b"beta",
        );
        converge(
            &sent_b,
            &handle_b,
            &mut mirror_b,
            &mut applied_b,
            "b.txt",
            b"beta",
        );

        handle_a.command(Command::Stop);
        handle_b.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delta_roundtrips_through_client_apply() {
        use blit_remote::fs::apply_fs_delta;
        let cases: &[(&[u8], &[u8])] = &[
            (b"hello world", b"hello world and more"),   // append
            (b"hello world", b"say: hello world"),       // prepend
            (b"hello cruel world", b"hello kind world"), // middle edit
            (b"hello world", b"hello"),                  // truncate
            (b"hello", b"goodbye"),                      // rewrite
            (b"", b"from nothing"),                      // create
            (b"to nothing", b""),                        // empty out
            (b"same", b"same"),                          // identical
        ];
        for (base, new) in cases {
            let ops = encode_delta(base, new);
            assert_eq!(
                apply_fs_delta(base, &ops).as_deref(),
                Some(*new),
                "case {:?} -> {:?}",
                base,
                new
            );
        }
        // An append's delta is one COPY plus the tail, far below full size.
        let base = vec![b'x'; 10_000];
        let mut new = base.clone();
        new.extend_from_slice(b"tail");
        let ops = encode_delta(&base, &new);
        assert!(
            ops.len() < 20,
            "append delta should be tiny, got {}",
            ops.len()
        );
        assert_eq!(apply_fs_delta(&base, &ops).unwrap(), new);
    }

    #[test]
    fn blob_store_lru_eviction() {
        let mut store = BlobStore::new(1000);
        let blob = |b: u8| Arc::new(vec![b; 400]);
        store.put(1, blob(1));
        store.put(2, blob(2));
        store.get(1); // refresh: 2 is now the oldest
        store.put(3, blob(3)); // 1200 bytes > budget: evicts 2
        assert!(store.get(2).is_none());
        assert!(store.get(1).is_some());
        assert!(store.get(3).is_some());
        // A blob over the whole budget is refused outright.
        store.put(4, Arc::new(vec![0; 2000]));
        assert!(store.get(4).is_none());
    }

    /// Engine-level: an append to a synced file must arrive as a delta
    /// record (not full content), an identical rewrite as metadata-only,
    /// and the mirror must track disk throughout.
    #[test]
    fn engine_sends_deltas() {
        use blit_remote::fs::{FsContent, FsRecord, fs_records, fs_update_records};

        let root = temp_dir();
        let big = vec![b'x'; 4096];
        fs::write(root.join("log.txt"), &big).unwrap();

        let shared = open_root_unwatched(test_key(&root));
        let hint_tx = shared.hint_sender();
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = SyncOptions {
            content: true,
            latency: Duration::from_millis(5),
            ..Default::default()
        };
        let handle = start_sync(
            &shared,
            13,
            opts,
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );

        let mut mirror = FsMirror::new();
        let mut applied = 0usize;
        // Collect (path, content-kind) for every upsert applied.
        let mut kinds: Vec<(String, &'static str)> = Vec::new();
        let apply_all = |mirror: &mut FsMirror,
                         applied: &mut usize,
                         kinds: &mut Vec<(String, &'static str)>| {
            let msgs = sent.lock().unwrap().clone();
            for msg in &msgs[*applied..] {
                if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                    let records = fs_update_records(msg).expect("decompress");
                    for record in fs_records(&records) {
                        if let FsRecord::Upsert { path, content, .. } = record {
                            let kind = match content {
                                FsContent::None => "none",
                                FsContent::Full(_) => "full",
                                FsContent::Delta(_) => "delta",
                            };
                            kinds.push((path.to_string(), kind));
                        }
                    }
                    let id = mirror.apply_update(msg).expect("valid update");
                    handle.command(Command::Ack(id));
                }
            }
            *applied = msgs.len();
        };

        let wait_for = |sent: &Arc<Mutex<Vec<Vec<u8>>>>, min: usize| {
            for _ in 0..400 {
                if sent.lock().unwrap().len() >= min {
                    std::thread::sleep(Duration::from_millis(20));
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("timed out waiting for {min} messages");
        };

        // Initial snapshot: full content.
        wait_for(&sent, 1);
        apply_all(&mut mirror, &mut applied, &mut kinds);
        assert!(kinds.contains(&("log.txt".into(), "full")));
        assert_eq!(mirror.live["log.txt"].content.as_deref(), Some(&big[..]));

        // Append: must flow as a delta.
        kinds.clear();
        let mut appended = big.clone();
        appended.extend_from_slice(b"appended tail");
        fs::write(root.join("log.txt"), &appended).unwrap();
        hint_tx.send(Hint::Dirty(root.join("log.txt")));
        wait_for(&sent, applied + 1);
        apply_all(&mut mirror, &mut applied, &mut kinds);
        assert!(
            kinds.contains(&("log.txt".into(), "delta")),
            "expected a delta record, got {kinds:?}"
        );
        assert_eq!(
            mirror.live["log.txt"].content.as_deref(),
            Some(&appended[..])
        );

        // Rewrite with identical bytes (mtime changes): metadata-only,
        // the mirror keeps its content.
        kinds.clear();
        std::thread::sleep(Duration::from_millis(10)); // ensure mtime moves
        fs::write(root.join("log.txt"), &appended).unwrap();
        hint_tx.send(Hint::Dirty(root.join("log.txt")));
        wait_for(&sent, applied + 1);
        apply_all(&mut mirror, &mut applied, &mut kinds);
        assert!(
            kinds.contains(&("log.txt".into(), "none")),
            "expected metadata-only, got {kinds:?}"
        );
        assert_eq!(
            mirror.live["log.txt"].content.as_deref(),
            Some(&appended[..])
        );
        assert_eq!(
            mirror.live["log.txt"].entry_flags & blit_remote::fs::FS_ENTRY_NO_CONTENT,
            0
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn read_verified_stable() {
        let root = temp_dir();
        let f = root.join("x");
        fs::write(&f, b"stable").unwrap();
        match read_verified(&f) {
            ReadOutcome::Stable(data) => assert_eq!(data, b"stable"),
            _ => panic!("expected stable read"),
        }
        match read_verified(&root.join("missing")) {
            ReadOutcome::Unreadable => {}
            _ => panic!("expected unreadable"),
        }
        let _ = fs::remove_dir_all(&root);
    }
}
