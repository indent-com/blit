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
    FS_DONE_INVALID, FS_DONE_NOT_FOUND, FS_DONE_OFFSET_MISMATCH, FS_DONE_OK, FS_DONE_OTHER,
    FS_DONE_PERMISSION, FS_DONE_SIZE_MISMATCH, FS_DONE_TOO_LARGE, FS_DONE_UNKNOWN_UPLOAD,
    FS_DONE_WRONG_TYPE, FS_ENTRY_DIR, FS_ENTRY_FILE, FS_ENTRY_FILTERED, FS_ENTRY_LINK_DIR,
    FS_ENTRY_NO_CONTENT, FS_ENTRY_OTHER, FS_ENTRY_SYMLINK, FS_ENTRY_TYPE_MASK, FS_ENTRY_UNREADABLE,
    FS_ENTRY_UNSTABLE, FS_FILE_NOT_FOUND, FS_FILE_OK, FS_FILE_UNREADABLE, FS_OP_HARDLINK,
    FS_OP_MKDIR, FS_OP_MKPARENTS, FS_OP_NO_CAS, FS_OP_REMOVE, FS_OP_RENAME, FS_OP_SYMLINK,
    FS_UPDATE_RESET, FS_UPDATE_SYNC, FS_UPLOAD_DURABLE, FS_UPLOAD_FLAGS_KNOWN,
    FS_UPLOAD_FOLLOW_SYMLINK, FS_UPLOAD_MKPARENTS, FS_UPLOAD_NO_CAS, FS_WRITE_DURABLE,
    FS_WRITE_FOLLOW_SYMLINK, FS_WRITE_MKPARENTS, FS_WRITE_NO_CAS, FsContent, FsRecord,
    append_fs_record, msg_fs_closed, msg_fs_done, msg_fs_file, msg_fs_update,
    msg_fs_upload_begin_result, msg_fs_upload_chunk_result, msg_fs_upload_finish_result,
};

pub mod backend;
pub mod ignores;

pub use ignores::{IgnoreSpec, MAX_PATTERNS as MAX_IGNORE_PATTERNS};

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

/// A chunked-upload begin forwarded to the engine (docs/protocol.md
/// "Filesystem sync"). `path` is the escaped wire path; `flags` are
/// `FS_UPLOAD_*`; `base` is the CAS precondition with `FS_WRITE`'s exact
/// semantics; `size` is the total plaintext bytes the client will send.
/// `upload_id` is the server-allocated per-connection id echoed in replies.
#[derive(Clone, Debug)]
pub struct UploadBeginReq {
    pub nonce: u16,
    pub upload_id: u16,
    pub path: String,
    pub flags: u8,
    pub base: u128,
    pub mode: u32,
    pub size: u64,
    /// Freed (nonce slot released) when this request is dropped after the
    /// engine answers it. `None` in tests and embedders without accounting.
    pub inflight: Option<Arc<InflightGuard>>,
}

/// One in-progress chunked upload: chunks append sequentially to a temp
/// sibling of the target (same directory ⇒ same filesystem ⇒ atomic rename
/// at FINISH). Dropping removes the temp file — cancel, sync stop, engine
/// exit, and connection close all funnel through it.
struct Upload {
    tmp: PathBuf,
    /// Client's wire path, re-resolved at FINISH (the symlink policy
    /// applies then as at BEGIN) and the echo-key fallback.
    wire: String,
    /// `Some` until FINISH takes it for the durable fsync + rename.
    file: Option<fs::File>,
    received: u64,
    size: u64,
    durable: bool,
    /// The CAS precondition, re-verified at FINISH (FS_WRITE semantics:
    /// `no_cas` ignores `base`, `base` 0 is create-exclusive, anything else
    /// must equal the current content hash).
    base: u128,
    no_cas: bool,
    follow_symlink: bool,
    /// Set once the temp has been renamed onto the target.
    landed: bool,
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.landed {
            let _ = fs::remove_file(&self.tmp);
        }
    }
}

/// Commands forwarded from the client connection.
#[derive(Clone, Debug)]
pub enum Command {
    Ack(u32),
    Fetch {
        nonce: u16,
        path: String,
    },
    Write(WriteReq),
    Op(OpReq),
    UploadBegin(UploadBeginReq),
    /// Append one chunk to a live upload; `data` is the plaintext chunk.
    UploadChunk {
        upload_id: u16,
        offset: u64,
        data: Vec<u8>,
    },
    /// Land (or refuse) the upload; terminates it either way.
    UploadFinish {
        nonce: u16,
        upload_id: u16,
        inflight: Option<Arc<InflightGuard>>,
    },
    /// Abort the upload and remove its temp file. No reply.
    UploadCancel {
        upload_id: u16,
    },
    Stop,
}

/// Registration interface a backend exposes to the reconciler so the set
/// of watched directories tracks the set of *indexed* ones (inotify, where
/// a recursive watch is a descriptor per directory and an excluded subtree
/// would otherwise still cost them all). FSEvents/RDCW cover a tree with
/// one object and use the no-op default, as does any unfiltered root.
pub trait BackendHandle: Send {
    /// Arm a watch on a directory about to be enumerated. `false` means
    /// watch descriptors are exhausted: the caller closes the root rather
    /// than serve a mirror with a silently stale subtree in it.
    fn add_dir(&self, _dir: &Path) -> bool {
        true
    }
    /// Arm a directory *outside* the synced tree, because it holds an
    /// ignore source the matcher consulted. Unlike [`BackendHandle::add_dir`]
    /// this is not covered by a recursive root watch, so every filtered
    /// root needs it however the tree itself is watched.
    fn watch_outside(&self, _dir: &Path) {}
    /// Disarm a directory and everything under it — deleted, or newly
    /// excluded.
    fn remove_dir(&self, _dir: &Path) {}
    /// Disarm every armed directory `keep` rejects, after a full rescan
    /// replaces the index wholesale.
    fn retain_dirs(&self, _keep: &dyn Fn(&Path) -> bool) {}
}

pub struct NoopBackend;
impl BackendHandle for NoopBackend {}

// ---------------------------------------------------------------------------
// Shared roots: one native watcher + one canonical index per watched root,
// shared by every sync of that root across all connections.
// ---------------------------------------------------------------------------

/// Identity of a shared root. Enumeration scope is part of the identity:
/// recursive and non-recursive syncs of the same directory index different
/// trees and cannot share a reconciler — and neither do two syncs that
/// exclude different things, for exactly the same reason.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RootKey {
    /// Canonical root path (see [`validate_root`]).
    pub path: PathBuf,
    pub recursive: bool,
    pub cross_filesystem: bool,
    /// What this root excludes from enumeration, watching, hashing, and
    /// records (docs/design/fs-watch.md "Ignoring"). Default excludes
    /// nothing, and an empty spec costs nothing: no matcher is built.
    pub ignores: IgnoreSpec,
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
        /// Keys that differ from the previously published snapshot, so
        /// engines diff only these instead of walking both maps. `None` =
        /// unknown (a subscriber's first snapshot): diff everything.
        changed: Option<Arc<std::collections::BTreeSet<String>>>,
        /// Keys whose stat is unchanged but whose mtime is too recent to
        /// prove it (docs/design/fs-watch.md "Racily-clean entries"). The
        /// reconciler cannot settle these; engines can, by hashing.
        recheck: Arc<std::collections::BTreeSet<String>>,
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
    /// `FS_SYNC_SINGLE` root: `key.path` is a FILE, the index holds exactly
    /// one entry (""), and the native watch sits on the file's parent
    /// directory, non-recursive, filtered to the file's name
    /// (docs/design/fs-watch.md "Single-file sync").
    single: bool,
    tx: Sender<RootMsg>,
    /// Set to the close reason once the reconciler shuts the root down
    /// (root gone, permission lost, resource limit). A closed root is dead
    /// forever; a later `open_root` of the same key must not join it.
    closed: Arc<OnceLock<u8>>,
    /// Hashes engines learned recently, keyed by wire path with the stat
    /// each was verified against. Bridges the hash-publish coalescing
    /// window: a content sync joining while another is still reading the
    /// tree finds the hash here (stat re-checked against its snapshot)
    /// and serves from the blob store instead of re-reading every file.
    /// Coarsely bounded: cleared when over cap — entries only matter
    /// until the next hash publish.
    learned: Mutex<std::collections::HashMap<String, NodeMeta>>,
    /// Keeps the native watch alive for the root's lifetime.
    _backend: Mutex<Option<backend::WatchBackend>>,
    #[cfg(test)]
    worker_done: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
struct MarkDone(Arc<std::sync::atomic::AtomicBool>);

#[cfg(test)]
impl Drop for MarkDone {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

impl SharedRootHandle {
    pub fn key(&self) -> &RootKey {
        &self.key
    }

    /// True for an `FS_SYNC_SINGLE` root (the root is a single file).
    pub fn is_single(&self) -> bool {
        self.single
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

/// Registry key: sharing is per `(RootKey, single)` — the flag set is part
/// of the identity, so a SINGLE sync of a path can never join a directory
/// root of the same path (or vice versa), while two SINGLE syncs of one
/// file share a reconciler and watcher.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RegKey {
    root: RootKey,
    single: bool,
}

type Registry = std::collections::HashMap<RegKey, std::sync::Weak<SharedRootHandle>>;

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Open (or join) the shared root for `key`, arming a native watcher on
/// first open — before the initial enumeration, so nothing slips between
/// scan and event delivery. On failure returns an `FS_STATUS_*` code plus
/// diagnostic, so the server can answer `FS_SYNCED` accurately.
pub fn open_root(key: RootKey) -> Result<Arc<SharedRootHandle>, (u8, String)> {
    open_root_inner(key, false, true)
}

/// Open (or join) a shared root without a native watcher; hints come from
/// [`SharedRootHandle::hint_sender`]. For tests and embedders.
pub fn open_root_unwatched(key: RootKey) -> Arc<SharedRootHandle> {
    open_root_inner(key, false, false).expect("unwatched open cannot fail")
}

/// Open (or join) the shared root for an `FS_SYNC_SINGLE` sync of `path`
/// (a canonical FILE path from [`validate_single_root`]). The native watch
/// arms on the file's PARENT directory, non-recursive — a watch on the
/// file itself would follow its inode and go silent after a delete or a
/// rename-over, exactly the transitions a single-file sync must deliver.
/// `recursive`/`cross_filesystem`/`ignores` do not apply (nothing is
/// enumerated — the client named the one file it wants), so every SINGLE
/// sync of one file shares a single normalized key.
pub fn open_single_root(path: PathBuf) -> Result<Arc<SharedRootHandle>, (u8, String)> {
    open_root_inner(single_root_key(path), true, true)
}

/// [`open_single_root`] without a native watcher; hints come from
/// [`SharedRootHandle::hint_sender`]. For tests and embedders.
pub fn open_single_root_unwatched(path: PathBuf) -> Arc<SharedRootHandle> {
    open_root_inner(single_root_key(path), true, false).expect("unwatched open cannot fail")
}

fn single_root_key(path: PathBuf) -> RootKey {
    RootKey {
        path,
        recursive: false,
        cross_filesystem: false,
        ignores: IgnoreSpec::default(),
    }
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

fn open_root_inner(
    key: RootKey,
    single: bool,
    watched: bool,
) -> Result<Arc<SharedRootHandle>, (u8, String)> {
    let reg_key = RegKey {
        root: key.clone(),
        single,
    };
    // Join an existing live, open root under the lock.
    {
        let mut map = registry().lock().unwrap();
        map.retain(|_, weak| weak.strong_count() > 0);
        if let Some(existing) = map
            .get(&reg_key)
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
        // A SINGLE root watches the file's parent directory, non-recursive:
        // a watch armed on the file itself follows its inode and misses the
        // delete / rename-over / recreate transitions a single-file sync
        // exists to deliver. The reconciler filters hints to the file's
        // name, so sibling churn never reaches the index.
        let (watch_path, recursive) = if single {
            let parent = key
                .path
                .parent()
                .ok_or_else(|| {
                    use blit_remote::fs::FS_STATUS_OTHER;
                    (FS_STATUS_OTHER, "single root has no parent".to_string())
                })?
                .to_path_buf();
            (parent, false)
        } else {
            (key.path.clone(), key.recursive)
        };
        // A filtered root arms per directory so excluded subtrees cost no
        // watch descriptors; the reconciler drives it from enumeration
        // (backend::PerDirWatch).
        let per_dir =
            backend::per_dir_watching_pays(key.recursive, single, !key.ignores.is_empty());
        Some(
            backend::watch(&watch_path, recursive, per_dir, hints)
                .map_err(|e| (watch_error_status(&e), e.to_string()))?,
        )
    } else {
        None
    };
    // The reconciler must not own the watcher: the watch callback owns a
    // sender for `rx`, so a strong registration handle would make the root
    // self-retaining. The weak handle becomes inert as the shared handle
    // drops, at which point the inbox disconnects and the worker exits.
    let registrar: Box<dyn BackendHandle> = match &backend {
        Some(backend) => backend.registrar(),
        None => Box::new(NoopBackend),
    };
    let mut map = registry().lock().unwrap();
    map.retain(|_, weak| weak.strong_count() > 0);
    // Another thread may have created (and armed) the same root while we
    // were arming; prefer theirs and drop our now-redundant watcher.
    if let Some(existing) = map
        .get(&reg_key)
        .and_then(std::sync::Weak::upgrade)
        .filter(|h| !h.is_closed())
    {
        return Ok(existing);
    }
    let closed: Arc<OnceLock<u8>> = Arc::new(OnceLock::new());
    #[cfg(test)]
    let worker_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = Arc::new(SharedRootHandle {
        key: key.clone(),
        single,
        tx,
        closed: closed.clone(),
        learned: Mutex::new(Default::default()),
        _backend: Mutex::new(backend),
        #[cfg(test)]
        worker_done: worker_done.clone(),
    });
    std::thread::Builder::new()
        .name("blit-fsroot".into())
        .spawn(move || {
            #[cfg(test)]
            let _done = MarkDone(worker_done);
            Reconciler::new(key, single, rx, registrar, closed).run()
        })
        .expect("spawn fssync reconciler");
    map.insert(reg_key, Arc::downgrade(&handle));
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
    let err = match fs::canonicalize(path) {
        Ok(p) => return Ok(p),
        Err(e) => e,
    };
    // A root can arrive in either of two encodings, and they are not
    // distinguishable by inspection:
    //
    //   * raw, as a CLI or a user types it (`/tmp/50%.txt`);
    //   * wire-escaped, because FS_SYNCED echoes `escape_path(canonical_root)`
    //     and clients legitimately build further sync roots from that echo
    //     (js/ui/src/ide/session.ts) — where a literal `%` came back as `%25`.
    //
    // Escaping on the way out without decoding on the way in meant any path
    // containing `%` (or non-UTF-8 bytes) could be listed but never re-opened:
    // the round trip returned a string that no longer named the file, and the
    // client reported it as missing. Try the literal reading first so a file
    // genuinely named `50%25.txt` still wins, then the decoded one.
    if err.kind() == io::ErrorKind::NotFound
        && path.contains('%')
        && let Some(decoded) = wire_to_os(path)
        && let Ok(p) = fs::canonicalize(&decoded)
    {
        return Ok(p);
    }
    let status = match err.kind() {
        io::ErrorKind::NotFound => FS_STATUS_NOT_FOUND,
        io::ErrorKind::PermissionDenied => FS_STATUS_PERMISSION_DENIED,
        _ => FS_STATUS_OTHER,
    };
    Err((status, err.to_string()))
}

/// Validate and canonicalize an `FS_SYNC_SINGLE` root: the same
/// canonicalization as [`validate_root`], plus the path must not be a
/// directory — a directory root answers the existing invalid-path error
/// (docs/design/fs-watch.md "Single-file sync"). Canonicalization resolves
/// symlinks, so the returned path is the file itself, never a link to it.
pub fn validate_single_root(path: &str) -> Result<PathBuf, (u8, String)> {
    use blit_remote::fs::FS_STATUS_OTHER;
    let canon = validate_root(path)?;
    match fs::symlink_metadata(&canon) {
        Ok(md) if md.is_dir() => Err((
            FS_STATUS_OTHER,
            "single sync root is a directory".to_string(),
        )),
        _ => Ok(canon),
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
    /// Set when `node_type` is `FS_ENTRY_SYMLINK` and the target is a
    /// directory. Captured at stat time so the send path and the descent gates
    /// don't each have to re-resolve the link.
    pub link_dir: bool,
    /// Set on a directory whose last enumeration skipped an excluded
    /// child, and sent as `FS_ENTRY_FILTERED`. Not a property of the
    /// inode, so `stat_meta` never sets it: only the enumeration that
    /// applied the rules knows, and it writes the answer back onto the
    /// parent it just listed.
    pub filtered: bool,
}

impl NodeMeta {
    /// True when this node's children belong in the index: a real directory, or
    /// a symlink resolving to one. The file browser descends both, so every
    /// gate deciding "should I enumerate this?" must use this rather than
    /// testing `FS_ENTRY_DIR` alone — otherwise a symlinked directory is a dead
    /// end, reporting children it can never list.
    fn enumerable_dir(&self) -> bool {
        self.node_type == FS_ENTRY_DIR || (self.node_type == FS_ENTRY_SYMLINK && self.link_dir)
    }

    fn same_identity(&self, other: &NodeMeta) -> bool {
        self.node_type == other.node_type && self.dev_ino != (0, 0) && self.dev_ino == other.dev_ino
    }

    fn content_changed(&self, prev: &NodeMeta) -> bool {
        self.node_type != prev.node_type
            || self.size != prev.size
            || self.mtime_ns != prev.mtime_ns
            || self.dev_ino != prev.dev_ino
    }

    /// Equality for diffing: everything the client can see, except `hash`,
    /// which is a lazily learned annotation — a hash fill-in alone must not
    /// produce records.
    ///
    /// "Everything the client can see" includes the two flags that are not
    /// properties of the inode: `filtered` and `link_dir` both ride out in
    /// `entry_flags`, so a diff that ignored them could drop the only record
    /// that would have carried a flag flip. That is not hypothetical — it
    /// deadlocked `a_directory_reports_that_it_hid_children` on CI (#124).
    /// A newly excluded child normally bumps its parent directory's mtime,
    /// so the flip travelled as a side effect of a stat change; when the
    /// two writes landed in the same filesystem timestamp tick, mtime
    /// matched, the entry compared equal, and no record was emitted. The
    /// hint path suppresses further nudges once the canonical entry is
    /// `filtered` (one re-list per transition), so nothing retried and the
    /// client never learned. Comparing the flags is what makes the publish
    /// depend on the flag itself rather than on a coincidence of timestamps.
    fn visible_eq(&self, other: &NodeMeta) -> bool {
        self.node_type == other.node_type
            && self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && self.mode == other.mode
            && self.dev_ino == other.dev_ino
            && self.filtered == other.filtered
            && self.link_dir == other.link_dir
    }
}

/// `(dev, ino)` of an already-stat'd node, or `(0, 0)` where the platform does
/// not expose one — which callers must treat as "identity unknown".
fn target_identity(md: &fs::Metadata) -> (u64, u64) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (md.dev(), md.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        (0, 0)
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
        // One extra stat, and only for links: resolving the target is the only
        // way to know whether this entry is enumerable.
        link_dir: ft.is_symlink() && fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false),
        // Enumeration's answer, not the inode's; filled in by whoever
        // lists this directory's children.
        filtered: false,
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

/// Keys at or under `root` in a sorted map: the entry itself plus the
/// contiguous `root/`-prefixed range — O(log n + subtree), never a scan of
/// the whole map.
fn subtree_keys<V>(map: &BTreeMap<String, V>, root: &str) -> Vec<String> {
    if root.is_empty() {
        return map.keys().cloned().collect();
    }
    let mut keys: Vec<String> = Vec::new();
    if map.contains_key(root) {
        keys.push(root.to_string());
    }
    let prefix = format!("{root}/");
    keys.extend(
        map.range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone()),
    );
    keys
}

/// Borrowed variant of [`subtree_keys`]: entries at or under `root`.
fn subtree_entries<'a, V>(
    map: &'a BTreeMap<String, V>,
    root: &str,
) -> impl Iterator<Item = (&'a String, &'a V)> {
    let own = if root.is_empty() {
        None
    } else {
        map.get_key_value(root)
    };
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    own.into_iter().chain(
        map.range(prefix.clone()..)
            .take_while(move |(k, _)| k.starts_with(&prefix)),
    )
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
    diff_classified(prev, curr, removed, added, changed)
}

/// [`diff`] restricted to a known changed-key set: only `changed_keys` are
/// probed against the two indexes, so a small change in a large tree costs
/// O(changed) instead of a walk of both maps. The set must cover every key
/// that differs between `prev` and `curr` (the reconciler's published sets
/// guarantee this); keys that turn out equal are skipped.
fn diff_changed(
    prev: &Index,
    curr: &Index,
    changed_keys: &std::collections::BTreeSet<String>,
) -> Vec<DiffOp> {
    let mut removed: Vec<&String> = Vec::new();
    let mut added: Vec<&String> = Vec::new();
    let mut changed: Vec<(&String, bool)> = Vec::new();
    // BTreeSet iteration is sorted, which the classification relies on.
    for key in changed_keys {
        match (prev.get_key_value(key), curr.get_key_value(key)) {
            (Some((pk, pv)), Some((_, cv))) => {
                if !cv.visible_eq(pv) {
                    changed.push((pk, cv.content_changed(pv)));
                }
            }
            (Some((pk, _)), None) => removed.push(pk),
            (None, Some((ck, _))) => added.push(ck),
            (None, None) => {}
        }
    }
    diff_classified(prev, curr, removed, added, changed)
}

/// Mark `root` and its subtree in a sorted path list: the entry itself and
/// the contiguous `root/` range, located by binary search instead of a
/// whole-list scan.
fn cover_sorted(paths: &[&String], covered: &mut [bool], root: &str) {
    if let Ok(i) = paths.binary_search_by(|p| p.as_str().cmp(root)) {
        covered[i] = true;
    }
    let prefix = format!("{root}/");
    let start = paths.partition_point(|p| p.as_str() < prefix.as_str());
    for i in start..paths.len() {
        if !paths[i].starts_with(&prefix) {
            break;
        }
        covered[i] = true;
    }
}

/// Shared tail of [`diff`] / [`diff_changed`]: move join over the
/// classified removed/added/changed lists (each sorted by path), then op
/// emission.
fn diff_classified(
    prev: &Index,
    curr: &Index,
    removed: Vec<&String>,
    added: Vec<&String>,
    changed: Vec<(&String, bool)>,
) -> Vec<DiffOp> {
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
        cover_sorted(&removed, &mut removed_covered, from);
        cover_sorted(&added, &mut added_covered, to);
        moves.push((from.clone(), to.clone()));
    }

    let mut ops = Vec::new();
    // Moves first (so later deletes of emptied ancestors don't prune them),
    // then deletes, then upserts.
    for (from, to) in &moves {
        ops.push(DiffOp::Move {
            from: from.clone(),
            to: to.clone(),
        });
    }
    // Skip paths whose ancestor is also being deleted; DELETE prunes.
    // Sorted order puts every ancestor before its descendants, so probing a
    // path's ancestor chain against the deletes already emitted replaces
    // the pairwise removed × removed scan.
    let mut emitted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, path) in removed.iter().enumerate() {
        if removed_covered[i] {
            continue;
        }
        let mut ancestor_deleted = false;
        let mut cursor: &str = path;
        while let Some(parent) = parent_wire(cursor) {
            if emitted.contains(parent) {
                ancestor_deleted = true;
                break;
            }
            cursor = parent;
        }
        if !ancestor_deleted {
            emitted.insert(path.as_str());
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
    for (from, to) in &moves {
        for (path, _) in subtree_entries(prev, from) {
            let new_path = rebase_subtree_path(path, from, to);
            if !curr.contains_key(&new_path) {
                ops.push(DiffOp::Delete { path: new_path });
            }
        }
        for (path, new) in subtree_entries(curr, to) {
            let old_path = rebase_subtree_path(path, to, from);
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

/// BLAKE3 truncated to 128 bits, little-endian — the protocol-wide content
/// hash (docs/design/fs-watch.md). `pub` so sibling stores (the KV store,
/// docs/design/kv.md) share the one convention instead of re-deriving it.
pub fn blake3_128(data: &[u8]) -> u128 {
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

/// Total-size cap for one chunked upload (`BLIT_FS_UPLOAD_MAX`, default
/// 1 GiB); `UPLOAD_BEGIN` refuses a larger declared `size` with `TOO_LARGE`.
/// Per-chunk bounds are the frame cap and the decompress guard — this caps
/// the sum.
fn fs_upload_max() -> u64 {
    std::env::var("BLIT_FS_UPLOAD_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024 * 1024 * 1024)
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

/// Why a wire path could not be confined under `root`.
enum ConfineError {
    /// Empty or not a single normal-component path.
    Invalid,
    /// The parent could not be canonicalized (missing, permission, ...).
    Io(io::Error),
    /// The canonical parent lies outside `root`.
    Escapes,
}

/// Component-validate `wire` (the traversal fix), then canonicalize the
/// target's *parent* and re-confirm it is under the already-canonical
/// `root` — defeating an in-tree symlink whose target escapes, which
/// `resolve_wire_path` (no symlink resolution) would miss. The final
/// component is *not* resolved here: callers apply their own symlink
/// policy (a final-component symlink is read/operated on as the link, never
/// followed out of root). Returns the confined absolute path.
fn confine_target(root: &Path, wire: &str) -> Result<PathBuf, ConfineError> {
    let abs = resolve_wire_path(root, wire).ok_or(ConfineError::Invalid)?;
    let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
        return Err(ConfineError::Invalid);
    };
    let canon_parent = fs::canonicalize(parent).map_err(ConfineError::Io)?;
    if !canon_parent.starts_with(root) {
        return Err(ConfineError::Escapes);
    }
    Ok(canon_parent.join(name))
}

/// Resolve and confine a write target via [`confine_target`], then handle a
/// final-component symlink per `policy`. Returns the absolute path to
/// operate on, or an `FS_DONE_*` status on refusal.
fn resolve_write_target(root: &Path, wire: &str, policy: SymlinkPolicy) -> Result<PathBuf, u8> {
    let target = match confine_target(root, wire) {
        Ok(t) => t,
        Err(ConfineError::Invalid) => return Err(FS_DONE_INVALID),
        Err(ConfineError::Io(e)) => return Err(write_io_status(&e)),
        Err(ConfineError::Escapes) => return Err(FS_DONE_PERMISSION),
    };
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

/// Final-component prefix of blit's own staging files (atomic writes and
/// chunked-upload temp siblings). The reconciler never indexes these — a
/// pure name filter like the `.git` exclusion, and unconditional because
/// the files are blit-internal artifacts: an upload's temp lives in the
/// watched directory for the whole transfer, and mirroring it would
/// re-stat — for a content sync, re-read — a growing file on every batch.
const TEMP_FILE_PREFIX: &str = ".blit-tmp-";

/// A unique sibling temp path for atomic replace (same directory ⇒ same
/// filesystem ⇒ atomic `rename`).
fn temp_sibling(target: &Path) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    dir.join(format!("{TEMP_FILE_PREFIX}{}-{n}", std::process::id()))
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
    #[cfg(not(unix))]
    let _ = mode;
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
    match fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => match link_target_bytes(path) {
            Ok(bytes) => blake3_128(&bytes),
            Err(_) => 0,
        },
        // Stream the existing file through a fixed buffer rather than
        // fs::read: the on-disk target is unbounded (the CAS request that
        // triggers this hash is capped, but the file it compares against
        // is not), so a whole-file read would let a tiny request force an
        // arbitrarily large allocation.
        _ => hash_file_streamed(path).unwrap_or(0),
    }
}

/// The write family's CAS precondition against the current on-disk entry,
/// shared by one-shot writes (`exec_write`) and chunked uploads (checked at
/// BEGIN and re-verified at FINISH): `no_cas` passes unconditionally,
/// `base` 0 is create-exclusive, anything else must equal the live content
/// hash. `Err((CONFLICT, hash))` carries the live hash (0 = absent), so
/// the client rebases without a round trip. The caller holds the target's
/// write lock.
fn check_write_precondition(target: &Path, base: u128, no_cas: bool) -> Result<(), (u8, u128)> {
    if no_cas {
        return Ok(());
    }
    if base == 0 {
        // symlink_metadata, not exists(): a dangling symlink at the target
        // is an entry and must fail create-exclusive. (The resolve step has
        // already refused or followed any symlink, so this mirrors
        // exec_write's `target.exists()` in practice.)
        if fs::symlink_metadata(target).is_ok() {
            return Err((FS_DONE_CONFLICT, current_hash(target)));
        }
    } else {
        let cur = current_hash(target);
        if cur != base {
            return Err((FS_DONE_CONFLICT, cur));
        }
    }
    Ok(())
}

/// BLAKE3-128 of a file's bytes, read through a fixed buffer so peak memory
/// stays constant regardless of file size. Same value as `blake3_128` over
/// the full content.
fn hash_file_streamed(path: &Path) -> io::Result<u128> {
    use std::io::Read as _;
    let mut f = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(u128::from_le_bytes(
        hasher.finalize().as_bytes()[..16].try_into().unwrap(),
    ))
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
    /// SINGLE mode (docs/design/fs-watch.md "Single-file sync"): `root` is
    /// a FILE, the index holds at most the one entry "", hints are
    /// filtered to the file (and its parent — some backends report
    /// directory-level events), and nothing is ever enumerated.
    single: bool,
    /// Scan scope from the [`RootKey`] plus env-default budgets; the
    /// per-client knobs in here (content, window…) are unused.
    opts: SyncOptions,
    /// What this root excludes, compiled. `None` when the spec is empty —
    /// an unfiltered sync must not pay a matcher call per entry — and
    /// always `None` in SINGLE mode, where nothing is enumerated.
    ignores: Option<ignores::Ignores>,
    rx: Receiver<RootMsg>,
    backend: Box<dyn BackendHandle>,
    canonical: Index,
    /// Last published snapshot; republished to every new subscriber.
    snapshot: Arc<Index>,
    subs: std::collections::HashMap<u64, (Sender<SyncMsg>, Duration)>,
    /// Settle window: the minimum over subscribers.
    latency: Duration,
    dirty: std::collections::BTreeSet<String>,
    /// Keys whose canonical entry changed since the last publish. Drives
    /// the publish decision (empty = nothing to publish, no compare and no
    /// clone) and ships with the snapshot so engines diff only these.
    changed: std::collections::BTreeSet<String>,
    /// Keys verified this tick whose stat could not prove they are
    /// unchanged, because their mtime falls inside the racy window
    /// (docs/design/fs-watch.md "Racily-clean entries"). Published even
    /// when `changed` is empty — an unchanged snapshot is exactly the
    /// symptom — so the engines can settle it against their content hashes.
    recheck: std::collections::BTreeSet<String>,
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

/// Record every key whose entry differs between `old` and `new` — the
/// changed-key set of a full rescan, computed as a sorted merge.
fn record_merge_changed(
    old: &Index,
    new: &Index,
    changed: &mut std::collections::BTreeSet<String>,
) {
    let mut oi = old.iter().peekable();
    let mut ni = new.iter().peekable();
    loop {
        match (oi.peek(), ni.peek()) {
            (Some((ok, ov)), Some((nk, nv))) => {
                if ok == nk {
                    if ov != nv {
                        changed.insert((*ok).clone());
                    }
                    oi.next();
                    ni.next();
                } else if ok < nk {
                    changed.insert((*ok).clone());
                    oi.next();
                } else {
                    changed.insert((*nk).clone());
                    ni.next();
                }
            }
            (Some((ok, _)), None) => {
                changed.insert((*ok).clone());
                oi.next();
            }
            (None, Some((nk, _))) => {
                changed.insert((*nk).clone());
                ni.next();
            }
            (None, None) => break,
        }
    }
}

impl Reconciler {
    fn new(
        key: RootKey,
        single: bool,
        rx: Receiver<RootMsg>,
        backend: Box<dyn BackendHandle>,
        closed_flag: Arc<OnceLock<u8>>,
    ) -> Self {
        let opts = SyncOptions {
            recursive: key.recursive,
            cross_filesystem: key.cross_filesystem,
            ..Default::default()
        };
        // SINGLE mode enumerates nothing, so an ignore spec has nothing to
        // filter; `single_root_key` normalizes it away, and this mirrors
        // that so the two can never disagree.
        let ignores = (!single && !key.ignores.is_empty())
            .then(|| ignores::Ignores::new(&key.path, &key.ignores));
        Reconciler {
            root: key.path,
            single,
            latency: opts.latency,
            opts,
            ignores,
            rx,
            backend,
            canonical: Index::new(),
            snapshot: Arc::new(Index::new()),
            subs: Default::default(),
            dirty: Default::default(),
            changed: Default::default(),
            recheck: Default::default(),
            full_rescan: false,
            pending_since: None,
            hash_dirty_since: None,
            closed: None,
            closed_flag,
        }
    }

    fn run(mut self) {
        // Ignore sources above the root are read once at construction and
        // no hint from inside the tree could ever report them, so watch
        // the directories holding them — their parents, since a watch on a
        // file follows its inode past the rename-over an editor performs.
        // Armed before the scan like everything else, so an edit racing
        // the initial enumeration is not lost.
        if let Some(ignores) = &self.ignores {
            for dir in ignores.external_watch_dirs() {
                self.backend.watch_outside(&dir);
            }
        }
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
                            changed: None,
                            recheck: Default::default(),
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
                        self.changed.insert(path);
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

    /// Whether the entry at wire path `rel` is excluded from this root
    /// (docs/design/fs-watch.md "Ignoring"). `is_dir` must describe a
    /// *real* directory: git's syntax distinguishes `build` from `build/`,
    /// and a symlink is a file to it even when it resolves to a directory.
    fn ignored(&mut self, rel: &str, is_dir: bool) -> bool {
        // blit's own staging files (see TEMP_FILE_PREFIX) are never part of
        // the tree, whatever the configured rules say.
        if rel
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(TEMP_FILE_PREFIX))
        {
            return true;
        }
        match &mut self.ignores {
            Some(ignores) => ignores.matched(rel, is_dir),
            None => false,
        }
    }

    /// Whether a write to `abs` changed the rules themselves, which costs
    /// a rebuild of the matcher and a full re-enumeration. An ignore file
    /// under an already-excluded directory is not one — nothing ever reads
    /// it (docs/design/fs-watch.md "Ignoring").
    fn ignore_rules_changed(&mut self, abs: &Path, rel: &str) -> bool {
        match &mut self.ignores {
            Some(ignores) => ignores.source_affects_rules(abs, rel),
            None => false,
        }
    }

    /// Disarm every watched directory the canonical index no longer holds
    /// as a directory. A no-op for the recursive backends.
    fn retain_watched_dirs(&self) {
        let root = &self.root;
        let canonical = &self.canonical;
        self.backend.retain_dirs(&|abs| {
            wire_key_for(root, abs)
                .and_then(|key| canonical.get(&key).map(|m| m.node_type == FS_ENTRY_DIR))
                .unwrap_or(false)
        });
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
        if self.single {
            // The watch sits on the parent directory, so hints arrive for
            // every sibling: only the file itself — or the parent, since
            // some backends report directory-level events for changes
            // inside it — re-verifies the one entry. Sibling churn returns
            // here without arming a settle tick, so it never wakes the
            // sync. Rescan degrades to the same single re-stat.
            let relevant = match hint {
                Hint::Rescan => true,
                Hint::Dirty(abs) => abs == self.root || Some(abs.as_path()) == self.root.parent(),
            };
            if relevant {
                self.dirty.insert(String::new());
                if self.pending_since.is_none() {
                    self.pending_since = Some(Instant::now());
                }
            }
            return;
        }
        match hint {
            Hint::Rescan => self.full_rescan = true,
            Hint::Dirty(abs) => {
                let rel = match abs.strip_prefix(&self.root) {
                    Ok(rel) => rel,
                    // Outside the tree — except for the ignore sources
                    // above it, whose watches exist precisely so that a
                    // parent `.gitignore` edit re-classifies this root
                    // instead of going unnoticed for the sync's lifetime.
                    Err(_) => {
                        if self
                            .ignores
                            .as_ref()
                            .is_some_and(|i| i.is_external_source(&abs))
                        {
                            if let Some(ignores) = &mut self.ignores {
                                ignores.invalidate();
                            }
                            self.full_rescan = true;
                            if self.pending_since.is_none() {
                                self.pending_since = Some(Instant::now());
                            }
                        }
                        return;
                    }
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
                // An ignore-source edit re-classifies entries a previous
                // scan baked in — in both directions — so the matcher is
                // rebuilt and the tree re-enumerated rather than trusted.
                // Tested *before* the filter: `$GIT_DIR/info/exclude` sits
                // inside a directory the filter itself excludes, so the
                // other order would drop the hint that its own rules moved.
                if self.ignore_rules_changed(&abs, &wire) {
                    if let Some(ignores) = &mut self.ignores {
                        ignores.invalidate();
                    }
                    self.full_rescan = true;
                } else if self.ignored(&wire, false) {
                    // Excluded: dropped here, so churn under `node_modules`
                    // never reaches a stat, a hash, or a settle tick. A
                    // directory-only pattern (`build/`) does not match the
                    // path as a file, so the hint survives to `reconcile`,
                    // which stats it and excludes it there.
                    //
                    // One exception: a directory reporting no hidden
                    // children just gained one, and `FS_ENTRY_FILTERED`
                    // has to flip. Re-list that directory once — the next
                    // excluded child finds the flag already set and costs
                    // nothing, so this is one listing per transition, not
                    // per event. A parent that is itself excluded is not
                    // in the index and never qualifies.
                    if let Some(parent) = parent_wire(&wire)
                        && self.canonical.get(parent).is_some_and(|m| !m.filtered)
                    {
                        self.dirty.insert(parent.to_string());
                        if self.pending_since.is_none() {
                            self.pending_since = Some(Instant::now());
                        }
                    }
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
                Ok(index) => {
                    // Record the rescan's effect as changed keys (a sorted
                    // merge, O(n) like the scan itself) so engines still
                    // diff incrementally.
                    record_merge_changed(&self.canonical, &index, &mut self.changed);
                    self.canonical = index;
                    // A rescan re-arms everything it enumerates but reports
                    // no removals, so directories that vanished (or became
                    // excluded) since the last one are disarmed here.
                    self.retain_watched_dirs();
                    // Recompiling the rules can change which sources above
                    // the root they read — a `.gitignore` appearing in an
                    // ancestor that had none. Re-arming is a set lookup
                    // per directory when nothing moved.
                    if let Some(ignores) = &self.ignores {
                        for dir in ignores.external_watch_dirs() {
                            self.backend.watch_outside(&dir);
                        }
                    }
                }
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
        // Drop keys that reverted within the window: a tick whose
        // verification found no net change publishes nothing — and pays
        // neither the full-index compare nor the clone.
        let prev_snapshot = self.snapshot.clone();
        self.changed
            .retain(|k| self.canonical.get(k) != prev_snapshot.get(k));
        // A racily-clean entry publishes even when the snapshot is
        // identical: an identical snapshot is precisely the symptom, and
        // only the engines' content hashes can tell it from a real no-op.
        if !self.changed.is_empty() || !self.recheck.is_empty() {
            let changed = Arc::new(std::mem::take(&mut self.changed));
            let recheck = Arc::new(std::mem::take(&mut self.recheck));
            if !changed.is_empty() {
                self.snapshot = Arc::new(self.canonical.clone());
            }
            for (tx, _) in self.subs.values() {
                let _ = tx.send(SyncMsg::Root(RootUpdate::Snapshot {
                    index: self.snapshot.clone(),
                    settled,
                    changed: Some(changed.clone()),
                    recheck: recheck.clone(),
                }));
            }
        }
    }

    fn scan_all(&mut self) -> Result<Index, u8> {
        if self.single {
            return self.scan_single();
        }
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

    /// SINGLE-mode snapshot: stat the one file, never enumerate. The file's
    /// own absence is state, not failure — deletes and recreates flow as
    /// DELETE/UPSERT of "" — but a vanished PARENT means the watch itself
    /// is dead and no recreate could ever be observed, so that closes the
    /// sync (docs/design/fs-watch.md "Single-file sync").
    fn scan_single(&self) -> Result<Index, u8> {
        let mut index = Index::new();
        match stat_meta(&self.root) {
            Ok(meta) => {
                index.insert(String::new(), meta);
            }
            Err(e) => {
                if !self.root.parent().map(Path::exists).unwrap_or(false) {
                    return Err(FS_CLOSED_ROOT_GONE);
                }
                if e.kind() == io::ErrorKind::PermissionDenied {
                    return Err(FS_CLOSED_PERMISSION_LOST_COMPAT);
                }
                // NotFound (or transient): the file is absent right now;
                // the mirror is empty until a hint observes a recreate.
            }
        }
        Ok(index)
    }

    /// SINGLE-mode verification of the one entry, preserving a learned
    /// hash across metadata-only changes exactly as directory reconcile
    /// does. Same absence/parent-gone split as [`Reconciler::scan_single`].
    fn reconcile_single(&mut self) -> Result<(), u8> {
        match stat_meta(&self.root) {
            Ok(meta) => {
                let preserved = self
                    .canonical
                    .get("")
                    .and_then(|m| (!m.content_changed(&meta)).then_some(m.hash));
                let mut meta = meta;
                if let Some(h) = preserved {
                    meta.hash = h;
                    self.note_racy("", &meta);
                }
                self.index_insert(String::new(), meta);
            }
            Err(e) => {
                if !self.root.parent().map(Path::exists).unwrap_or(false) {
                    return Err(FS_CLOSED_ROOT_GONE);
                }
                if e.kind() == io::ErrorKind::PermissionDenied {
                    return Err(FS_CLOSED_PERMISSION_LOST_COMPAT);
                }
                self.index_remove("");
            }
        }
        Ok(())
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
        let mut ancestors = Vec::new();
        self.scan_into_inner(index, abs, rel, recurse, root_dev, &mut ancestors)?;
        Ok(())
    }

    /// `ancestors` holds the `(dev, ino)` identity of every directory on the
    /// current descent path, which is what makes following symlinks safe: a
    /// link whose target is already an ancestor is a cycle and is reported
    /// without being descended. Identity comes from the stat already performed,
    /// so this costs nothing extra for the common case.
    fn scan_into_inner(
        &mut self,
        index: &mut Index,
        abs: &Path,
        rel: &str,
        recurse: bool,
        root_dev: Option<u64>,
        ancestors: &mut Vec<(u64, u64)>,
        // `true` = the entry was excluded rather than indexed, which the
        // caller records on the parent directory as `FS_ENTRY_FILTERED`.
    ) -> io::Result<bool> {
        let meta = stat_meta(abs)?;
        // Excluded: not indexed, not descended, not counted against the
        // entry budget. A symlink to a directory counts as a directory
        // here — git would call it a file, but git also does not descend
        // it, and this sync does: a `build/` that could not exclude a
        // symlinked `build` would leave one hole through which a whole
        // subtree still gets mirrored.
        if !rel.is_empty() && self.ignored(rel, meta.enumerable_dir()) {
            return Ok(true);
        }
        if index.len() >= self.opts.max_entries {
            return Err(io::Error::from_raw_os_error(RESOURCE_LIMIT_ERRNO));
        }
        let node_type = meta.node_type;
        let link_dir = meta.link_dir;
        let self_id = meta.dev_ino;
        let dev = meta.dev_ino.0;
        index.insert(rel.to_string(), meta);

        // A symlink to a directory is still reported as FS_ENTRY_SYMLINK — its
        // content stays the target bytes (docs/design/fs-watch.md "Links") —
        // but it is enumerated so the file browser can descend it. Without
        // this a symlinked directory is a dead end: an entry with children
        // that can never be listed.
        let (descend_id, dev) = if node_type == FS_ENTRY_SYMLINK {
            if !link_dir {
                return Ok(false); // dangling, or a link to a file
            }
            let Ok(target) = fs::metadata(abs) else {
                return Ok(false);
            };
            let id = target_identity(&target);
            if id == (0, 0) {
                // No usable identity means no way to detect a cycle. Report the
                // link, but do not risk descending forever.
                return Ok(false);
            }
            if ancestors.contains(&id) {
                return Ok(false); // the link points back up its own path
            }
            (id, id.0)
        } else if node_type == FS_ENTRY_DIR {
            (self_id, dev)
        } else {
            return Ok(false);
        };

        ancestors.push(descend_id);
        let real_dir = node_type == FS_ENTRY_DIR;
        let result =
            self.scan_children(index, abs, rel, recurse, root_dev, dev, real_dir, ancestors);
        ancestors.pop();
        result.map(|()| false)
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_children(
        &mut self,
        index: &mut Index,
        abs: &Path,
        rel: &str,
        recurse: bool,
        root_dev: Option<u64>,
        dev: u64,
        // `real_dir`: a real directory, not a symlink to one. Only real
        // directories are armed — the recursive watch never followed links
        // either (`backend::watcher`, which explains why an aliased path
        // gets no descriptor), and arming both an alias and its target
        // hands inotify the same descriptor twice.
        real_dir: bool,
        ancestors: &mut Vec<(u64, u64)>,
    ) -> io::Result<()> {
        let root_dev = root_dev.or(Some(dev));
        if !self.opts.cross_filesystem && Some(dev) != root_dev {
            return Ok(()); // report the mount point, don't descend
        }
        // Arm before listing: an entry created in the gap is either listed
        // by the read below or reported by the watch, never neither. Watch
        // exhaustion closes the root — the alternative is a subtree that
        // silently stops updating.
        if real_dir && !self.backend.add_dir(abs) {
            return Err(io::Error::from_raw_os_error(RESOURCE_LIMIT_ERRNO));
        }
        if !recurse && !rel.is_empty() {
            return Ok(());
        }
        let entries = match fs::read_dir(abs) {
            Ok(e) => e,
            Err(_) => return Ok(()), // unreadable dir: node stays, children unknown
        };
        let mut filtered = false;
        for entry in entries.flatten() {
            let name = os_to_wire(&entry.file_name());
            let child_rel = join_wire(rel, &name);
            let child_abs = entry.path();
            // Non-recursive syncs index immediate children only.
            let child_recurse = self.opts.recursive;
            match self.scan_into_inner(
                index,
                &child_abs,
                &child_rel,
                child_recurse,
                root_dev,
                ancestors,
            ) {
                Ok(excluded) => filtered |= excluded,
                Err(e) if e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO) => return Err(e),
                Err(_) => {}
            }
            // Other errors: entry vanished mid-scan — fine, a hint follows.
        }
        // Tell the client this listing is incomplete by design. Written
        // onto the directory *after* its children, since only the walk
        // knows what the rules covered.
        if filtered && let Some(dir) = index.get_mut(rel) {
            dir.filtered = true;
        }
        Ok(())
    }

    /// Flag a verified-but-unprovable entry: its stat matched the one on
    /// record, yet its mtime is recent enough that a rewrite could have
    /// landed in the same timestamp granule and left size, identity and
    /// mtime all untouched (docs/design/fs-watch.md "Racily-clean
    /// entries"). Only content-carrying entries are worth flagging — a
    /// directory's bytes are its children, which get their own hints — and
    /// only engines can settle it, by hashing what they last sent.
    fn note_racy(&mut self, key: &str, meta: &NodeMeta) {
        if matches!(meta.node_type, FS_ENTRY_FILE | FS_ENTRY_SYMLINK) && racily_clean(meta.mtime_ns)
        {
            self.recheck.insert(key.to_string());
        }
    }

    /// Insert `meta` at `key`, recording the key as changed when the entry
    /// actually differs (hash included — an adopted hash must publish).
    fn index_insert(&mut self, key: String, meta: NodeMeta) {
        if self.canonical.get(&key) != Some(&meta) {
            self.changed.insert(key.clone());
            self.canonical.insert(key, meta);
        }
    }

    fn index_remove(&mut self, key: &str) {
        let Some(meta) = self.canonical.remove(key) else {
            return;
        };
        self.changed.insert(key.to_string());
        // A directory leaving the index — deleted, replaced by a file, or
        // newly excluded — takes its watch with it. Every removal path
        // funnels through here, so none of them can leak one.
        if meta.node_type == FS_ENTRY_DIR
            && let Some(abs) = resolve_wire_path(&self.root, key)
        {
            self.backend.remove_dir(&abs);
        }
    }

    /// Remove `rel` and everything under it via a range scan of the sorted
    /// index (never a full-index filter). `keep_root` retains `rel` itself.
    fn remove_index_subtree(&mut self, rel: &str, keep_root: bool) {
        for key in subtree_keys(&self.canonical, rel) {
            if keep_root && key == rel {
                continue;
            }
            self.index_remove(&key);
        }
    }

    /// The device the sync root lives on, which bounds cross-filesystem
    /// descent.
    ///
    /// Reconcile paths must pass this rather than `None`: `scan_children`
    /// re-anchors a `None` bound to whatever device it is handed, and for a
    /// symlink that is the *target's* device — silently lifting the guard for
    /// the whole foreign subtree. The reconcile pre-check cannot catch it
    /// either, because a symlink's own `dev_ino` comes from `lstat` and so
    /// reports the device the link itself lives on, not its target's.
    fn root_device(&self) -> Option<u64> {
        self.canonical.get("").map(|m| m.dev_ino.0)
    }

    /// Verify one hinted path against the canonical index.
    fn reconcile(&mut self, rel: &str) -> Result<(), u8> {
        if self.single {
            // note_hint only ever dirties "" in single mode.
            return self.reconcile_single();
        }
        let Some(abs) = resolve_wire_path(&self.root, rel) else {
            return Ok(());
        };
        match stat_meta(&abs) {
            Err(_) => {
                if rel.is_empty() {
                    return Err(FS_CLOSED_ROOT_GONE);
                }
                self.remove_index_subtree(rel, false);
            }
            Ok(meta) => {
                // Newly excluded — a `.gitignore` grew a line, or a
                // directory-only pattern that the hint stage could not
                // settle without a stat. Whatever the index holds under
                // it goes, so the client sees one DELETE of the subtree.
                if self.ignored(rel, meta.enumerable_dir()) {
                    self.remove_index_subtree(rel, false);
                    // The parent now has a hidden child. Only a listing can
                    // clear this again, which the next enumeration of that
                    // directory does; setting it from one path is the
                    // cheap half of the answer and never the wrong way
                    // round.
                    if let Some(parent) = parent_wire(rel)
                        && let Some(meta) = self.canonical.get(parent)
                        && !meta.filtered
                    {
                        let mut meta = meta.clone();
                        meta.filtered = true;
                        self.index_insert(parent.to_string(), meta);
                    }
                    return Ok(());
                }
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
                        self.index_insert(rel.to_string(), meta);
                        self.check_budget()?;
                    } else {
                        self.remove_index_subtree(rel, false);
                    }
                    return Ok(());
                }
                let known = self.canonical.contains_key(rel);
                let was_dir = self
                    .canonical
                    .get(rel)
                    .map(|m| m.enumerable_dir())
                    .unwrap_or(false);
                let is_dir = meta.enumerable_dir();
                let preserved_hash = self
                    .canonical
                    .get(rel)
                    .and_then(|m| (!m.content_changed(&meta)).then_some(m.hash));
                // `filtered` is the enumeration's answer, not the inode's,
                // so a fresh stat knows nothing about it: carry it forward
                // rather than clearing it on every re-verification. The
                // listing below (or the next one) is what corrects it.
                let was_filtered = self.canonical.get(rel).is_some_and(|m| m.filtered);
                let mut meta = meta;
                meta.filtered = was_filtered;
                if let Some(h) = preserved_hash {
                    meta.hash = h;
                    self.note_racy(rel, &meta);
                }
                self.index_insert(rel.to_string(), meta);
                self.check_budget()?;
                if is_dir && (!known || !was_dir) {
                    // New (or type-changed) directory: index its subtree and
                    // then rescan once more — children created between the
                    // watch registration and this scan produce duplicate
                    // hints, which reconcile to no-ops.
                    let mut sub = Index::new();
                    let bound = self.root_device();
                    match self.scan_into(&mut sub, &abs, rel, self.opts.recursive, bound) {
                        Ok(()) => {}
                        Err(e) if e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO) => {
                            return Err(FS_CLOSED_RESOURCE_LIMIT);
                        }
                        // Other errors: entry vanished mid-scan; a hint follows.
                        Err(_) => {}
                    }
                    for (k, v) in sub {
                        self.index_insert(k, v);
                    }
                    self.check_budget()?;
                } else if is_dir && self.opts.recursive {
                    // Existing dir: verify immediate children (names may
                    // have appeared/vanished without their own hints on
                    // some backends).
                    self.reconcile_children(&abs, rel)?;
                }
                if was_dir && !is_dir {
                    self.remove_index_subtree(rel, true);
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
        let mut filtered = false;
        for entry in entries.flatten() {
            let name = os_to_wire(&entry.file_name());
            let child_rel = join_wire(rel, &name);
            if let Ok(meta) = stat_meta(&entry.path()) {
                // An excluded child is neither indexed nor marked seen, so
                // whatever the index still holds under it is pruned with
                // the vanished ones below — the transition a path makes
                // when an ignore rule starts covering it.
                if self.ignored(&child_rel, meta.enumerable_dir()) {
                    filtered = true;
                    continue;
                }
                let newly_dir = meta.enumerable_dir()
                    && self
                        .canonical
                        .get(&child_rel)
                        .map(|m| !m.enumerable_dir())
                        .unwrap_or(true);
                let preserved = self
                    .canonical
                    .get(&child_rel)
                    .and_then(|m| (!m.content_changed(&meta)).then_some(m.hash));
                let mut meta = meta;
                // As in `reconcile`: a stat cannot see what a listing of
                // *this child's* children found, so keep the last answer.
                meta.filtered = self.canonical.get(&child_rel).is_some_and(|m| m.filtered);
                if let Some(h) = preserved {
                    meta.hash = h;
                    self.note_racy(&child_rel, &meta);
                }
                if newly_dir {
                    new_dirs.push((entry.path(), child_rel.clone()));
                }
                self.index_insert(child_rel.clone(), meta);
                self.check_budget()?;
            }
            seen.insert(child_rel);
        }
        // This listing saw every child, so it is also the authority on
        // whether any were excluded — including when the answer flips back
        // to `false` because the last one was deleted or un-ignored.
        if let Some(dir) = self.canonical.get(rel)
            && dir.filtered != filtered
        {
            let mut meta = dir.clone();
            meta.filtered = filtered;
            self.index_insert(rel.to_string(), meta);
        }
        // Children that disappeared, with their subtrees: one range walk
        // over `rel`'s subtree keyed by first component, instead of a
        // full-index scan per gone child.
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let gone: Vec<String> = self
            .canonical
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| {
                k.as_str() != rel && {
                    let rest = &k[prefix.len()..];
                    let child_end = prefix.len() + rest.find('/').unwrap_or(rest.len());
                    !seen.contains(&k[..child_end])
                }
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            self.index_remove(&k);
        }
        let bound = self.root_device();
        for (abs, rel) in new_dirs {
            let mut sub = Index::new();
            match self.scan_into(&mut sub, &abs, &rel, self.opts.recursive, bound) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(RESOURCE_LIMIT_ERRNO) => {
                    return Err(FS_CLOSED_RESOURCE_LIMIT);
                }
                Err(_) => {}
            }
            for (k, v) in sub {
                self.index_insert(k, v);
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

/// Backoff state for one file awaiting a settled re-read.
struct RetryEntry {
    /// Consecutive UNSTABLE/UNREADABLE outcomes.
    failures: u32,
    /// Earliest instant the next re-read may run.
    due: Instant,
}

/// Delay before the next re-read of an UNSTABLE/UNREADABLE entry: one
/// settle window after the first failure, doubling per consecutive
/// failure, capped so a file that never settles (an actively appended log
/// under a content sync) costs a bounded re-read rate instead of up to two
/// full reads every tick forever.
fn retry_backoff(failures: u32, latency: Duration) -> Duration {
    const RETRY_BACKOFF_CAP: Duration = Duration::from_secs(2);
    latency
        .saturating_mul(
            1u32.checked_shl(failures.saturating_sub(1))
                .unwrap_or(u32::MAX),
        )
        .min(RETRY_BACKOFF_CAP)
}

/// Per-sync engine: cuts client-specific update series from published
/// snapshots and paces them against the client's ack window.
struct SyncEngine {
    sync_id: u16,
    root: PathBuf,
    /// SINGLE sync: `root` is a FILE and the only addressable wire path —
    /// for fetches and the write family alike — is "" (the root itself).
    single: bool,
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
    /// Files last reported UNSTABLE/UNREADABLE: re-read once their backoff
    /// expires even though their metadata may not change again.
    retry: BTreeMap<String, RetryEntry>,
    /// Keys changed across the snapshots received since the last emit; the
    /// incremental diff probes only these.
    pending_changed: std::collections::BTreeSet<String>,
    /// Keys the reconciler could not settle from stat alone; this engine
    /// settles them by hashing (docs/design/fs-watch.md "Racily-clean
    /// entries").
    pending_recheck: std::collections::BTreeSet<String>,
    /// A snapshot arrived without a changed set: the next emit must fall
    /// back to the full two-map walk.
    full_diff: bool,
    /// Live chunked uploads, keyed by the server-allocated per-connection
    /// id. Dropped (temp files removed) when the engine exits.
    uploads: std::collections::HashMap<u16, Upload>,
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
            single: shared.single,
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
            pending_changed: Default::default(),
            pending_recheck: Default::default(),
            full_diff: false,
            uploads: Default::default(),
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
                self.drain_pending_commands();
                let _ = (self.outbox)(msg_fs_closed(self.sync_id, FS_CLOSED_CLIENT_REQUEST));
            }
            Exit::Closed(reason) => {
                self.drain_pending_commands();
                let _ = (self.outbox)(msg_fs_closed(self.sync_id, reason));
            }
        }
    }

    /// Answer every request still queued when the engine exits so the
    /// family's one-reply-per-nonce invariant holds on the close path too.
    /// Client Commands and reconciler RootUpdates share one inbox, so a
    /// Write/Op/Fetch enqueued behind (or racing) the Closed message would
    /// otherwise be dropped with its InflightGuard and never answered.
    /// Requests arriving after the engine's receiver drops instead see
    /// `SyncHandle::command` return false, and the server answers them.
    fn drain_pending_commands(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                SyncMsg::Cmd(Command::Write(w)) => {
                    let _ = (self.outbox)(msg_fs_done(w.nonce, FS_DONE_OTHER, 0, 0));
                }
                SyncMsg::Cmd(Command::Op(o)) => {
                    let _ = (self.outbox)(msg_fs_done(o.nonce, FS_DONE_OTHER, 0, 0));
                }
                SyncMsg::Cmd(Command::UploadBegin(b)) => {
                    let _ =
                        (self.outbox)(msg_fs_upload_begin_result(b.nonce, FS_DONE_OTHER, 0, 0, 0));
                }
                SyncMsg::Cmd(Command::UploadFinish { nonce, .. }) => {
                    let _ = (self.outbox)(msg_fs_upload_finish_result(nonce, FS_DONE_OTHER, 0, 0));
                }
                SyncMsg::Cmd(Command::Fetch { nonce, .. }) => {
                    let _ = (self.outbox)(msg_fs_file(nonce, blit_remote::fs::FS_FILE_OTHER, &[]));
                }
                SyncMsg::Cmd(
                    Command::Ack(_)
                    | Command::UploadChunk { .. }
                    | Command::UploadCancel { .. }
                    | Command::Stop,
                )
                | SyncMsg::Root(_) => {}
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
                Ok(SyncMsg::Cmd(Command::UploadBegin(b))) => {
                    if !self.handle_upload_begin(b) {
                        return Exit::ClientGone;
                    }
                }
                Ok(SyncMsg::Cmd(Command::UploadChunk {
                    upload_id,
                    offset,
                    data,
                })) => {
                    if !self.handle_upload_chunk(upload_id, offset, data) {
                        return Exit::ClientGone;
                    }
                }
                Ok(SyncMsg::Cmd(Command::UploadFinish {
                    nonce, upload_id, ..
                })) => {
                    if !self.handle_upload_finish(nonce, upload_id) {
                        return Exit::ClientGone;
                    }
                }
                Ok(SyncMsg::Cmd(Command::UploadCancel { upload_id })) => {
                    self.handle_upload_cancel(upload_id);
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
            RootUpdate::Snapshot {
                index,
                settled,
                changed,
                recheck,
            } => {
                self.latest = index;
                self.snapshot_dirty = true;
                // Independent of `changed`/`full_diff`: a full two-map walk
                // is just as blind to a same-stat rewrite as the
                // changed-key probe is.
                self.pending_recheck.extend(recheck.iter().cloned());
                match changed {
                    // Per-snapshot sets cover consecutive publishes, so
                    // their union covers shadow → latest exactly.
                    Some(set) if !self.full_diff => {
                        self.pending_changed.extend(set.iter().cloned());
                    }
                    Some(_) => {}
                    None => {
                        self.full_diff = true;
                        self.pending_changed.clear();
                    }
                }
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
        let full = std::mem::take(&mut self.full_diff);
        let changed = std::mem::take(&mut self.pending_changed);
        let recheck = std::mem::take(&mut self.pending_recheck);
        self.emit_updates(&canonical, initial, full, &changed, &recheck)?;
        self.shadow = canonical;
        self.initial_sent = true;
        // Credit waits may have delivered a newer snapshot mid-emit, and
        // unstable files want another pass: keep the clock running — for
        // retries, only until the earliest backoff expires (the tick fires
        // one latency window after `pending_since`).
        if self.snapshot_dirty {
            self.pending_since = Some(Instant::now());
        } else if let Some(due) = self.retry.values().map(|e| e.due).min() {
            self.pending_since = Some(
                due.checked_sub(self.opts.latency)
                    .unwrap_or_else(Instant::now),
            );
        }
        Ok(())
    }

    /// Diff shadow vs `canonical` and send updates. `initial` wraps the
    /// series in RESET … SYNC; `full` forces the two-map walk, otherwise
    /// only `changed` keys are probed. Batches stream as they are built,
    /// each gated on the ack window — a snapshot of any size holds at most
    /// one batch in memory and never outruns the client's credit.
    fn emit_updates(
        &mut self,
        canonical: &Arc<Index>,
        initial: bool,
        full: bool,
        changed: &std::collections::BTreeSet<String>,
        recheck: &std::collections::BTreeSet<String>,
    ) -> Result<(), Exit> {
        if initial {
            // The initial series carries every file's content, so nothing
            // pending can be stale.
            return self.emit_initial(canonical);
        }
        let mut ops = if full {
            diff(&self.shadow, canonical)
        } else {
            diff_changed(&self.shadow, canonical, changed)
        };
        // A retry entry whose file was renamed this tick must follow the
        // move before we prune against `canonical` (which only knows the
        // new path), or the pending content read is lost forever.
        for op in &ops {
            if let DiffOp::Move { from, to } = op {
                self.rekey_move(from, to);
            }
        }
        self.retry.retain(|path, _| canonical.contains_key(path));
        // Files awaiting a settled re-read (UNSTABLE or transiently
        // UNREADABLE) re-read even when their metadata is unchanged, so the
        // content still arrives once the file settles — but only once each
        // entry's backoff has expired.
        let now = Instant::now();
        let forced: Vec<String> = self
            .retry
            .iter()
            .filter(|(path, entry)| {
                entry.due <= now
                    && !ops
                        .iter()
                        .any(|op| matches!(op, DiffOp::Upsert { path: p, .. } if p == *path))
            })
            .map(|(path, _)| path.clone())
            .collect();
        ops.extend(forced.into_iter().map(|path| DiffOp::Upsert {
            path,
            content_changed: true,
        }));
        // Entries the reconciler could not settle from stat alone: hash
        // them against what the client holds, which is the only thing that
        // can tell a same-granule rewrite from a genuine no-op. Bytes just
        // written are in page cache, and a matching hash emits nothing.
        let racy: Vec<String> = recheck
            .iter()
            .filter(|path| {
                !ops.iter()
                    .any(|op| matches!(op, DiffOp::Upsert { path: p, .. } if p == *path))
                    && self.content_diverged(path, canonical)
            })
            .cloned()
            .collect();
        ops.extend(racy.into_iter().map(|path| DiffOp::Upsert {
            path,
            content_changed: true,
        }));
        if ops.is_empty() {
            return Ok(());
        }

        let mut buf: Vec<u8> = Vec::new();
        let mut reset_pending = false;
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
                    if let Some(meta) = canonical.get(path) {
                        self.append_upsert(&mut buf, path, meta, *content_changed);
                    }
                }
            }
            if buf.len() >= self.opts.batch_target {
                self.send_update(std::mem::take(&mut buf), &mut reset_pending, false)?;
            }
        }
        if !buf.is_empty() {
            self.send_update(buf, &mut reset_pending, false)?;
        }
        Ok(())
    }

    /// Stream the initial `RESET … SYNC` series straight off the snapshot:
    /// every entry is an upsert, so the series borrows paths and metadata
    /// from the index instead of materializing a whole-tree op list. The
    /// final update carries SYNC (an empty RESET|SYNC update is valid and
    /// terminates an empty tree's snapshot).
    fn emit_initial(&mut self, canonical: &Arc<Index>) -> Result<(), Exit> {
        let mut buf: Vec<u8> = Vec::new();
        let mut reset_pending = true;
        let index: &Index = canonical;
        for (path, meta) in index.iter() {
            self.append_upsert(&mut buf, path, meta, true);
            if buf.len() >= self.opts.batch_target {
                self.send_update(std::mem::take(&mut buf), &mut reset_pending, false)?;
            }
        }
        self.send_update(buf, &mut reset_pending, true)?;
        Ok(())
    }

    /// Append one upsert record for `path`/`meta`, attaching content per
    /// the sync's options and maintaining the held/retry maps. Emits
    /// nothing for a still-churning retry the client already knows about.
    fn append_upsert(
        &mut self,
        buf: &mut Vec<u8>,
        path: &str,
        meta: &NodeMeta,
        content_changed: bool,
    ) {
        let prior_failures = self.retry.remove(path).map(|e| e.failures).unwrap_or(0);
        let was_retry = prior_failures > 0;
        let mut entry_flags = meta.node_type & FS_ENTRY_TYPE_MASK;
        if meta.link_dir {
            entry_flags |= FS_ENTRY_LINK_DIR;
        }
        if meta.filtered {
            entry_flags |= FS_ENTRY_FILTERED;
        }
        let mut hash = meta.hash;
        let mut full: Option<Arc<Vec<u8>>> = None;
        let mut delta: Option<Vec<u8>> = None;
        // Files and symlinks both carry content — a symlink's is its
        // target bytes (hash = BLAKE3-128 over them).
        if matches!(meta.node_type, FS_ENTRY_FILE | FS_ENTRY_SYMLINK) {
            // An inlined file's bytes ride an FS_UPDATE, whose
            // decompressed payload a compliant client refuses above
            // FS_MAX_DECOMPRESSED — so never inline past that cap
            // regardless of the (client-supplied) inline_max, else
            // the update is undecodable and the sync wedges.
            let inline_cap = self
                .opts
                .inline_max
                .min(blit_remote::fs::FS_MAX_DECOMPRESSED as u64);
            if !self.opts.content || meta.size > inline_cap {
                entry_flags |= FS_ENTRY_NO_CONTENT;
                self.held.remove(path);
            } else if content_changed || meta.hash == 0 {
                match self.read_content(path, meta) {
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
                                .and_then(|&base_hash| blob_store().lock().unwrap().get(base_hash))
                                .map(|base| encode_delta(&base, &data))
                                .filter(|ops| ops.len() * 8 < data.len() * 7);
                            if delta.is_none() {
                                full = Some(data.clone());
                            }
                            self.held.insert(path.to_string(), hash);
                        }
                    }
                    ContentRead::Unstable => {
                        self.held.remove(path);
                        self.note_retry(path, prior_failures);
                        if was_retry {
                            // Still churning: the client already
                            // knows; try again after the backoff.
                            return;
                        }
                        entry_flags |= FS_ENTRY_UNSTABLE;
                    }
                    ContentRead::Unreadable => {
                        // The read raced a delete/permission
                        // flip between the reconciler's stat and
                        // our read. Re-read after the backoff so a
                        // transiently unreadable file still
                        // converges; diff alone would never
                        // revisit it (stat may be unchanged).
                        self.held.remove(path);
                        self.note_retry(path, prior_failures);
                        if was_retry {
                            return;
                        }
                        entry_flags |= FS_ENTRY_UNREADABLE;
                    }
                }
            }
            // Metadata-only change on a file whose content the
            // client already holds: no content section, no
            // NO_CONTENT flag — the mirror keeps its bytes.
        }
        let content = match (&delta, &full) {
            (Some(ops), _) => FsContent::Delta(ops),
            (None, Some(data)) => FsContent::Full(data.as_slice()),
            (None, None) => FsContent::None,
        };
        append_fs_record(
            buf,
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

    /// Schedule the next re-read of a failed content read, doubling the
    /// per-entry delay per consecutive failure.
    fn note_retry(&mut self, path: &str, prior_failures: u32) {
        let failures = prior_failures + 1;
        self.retry.insert(
            path.to_string(),
            RetryEntry {
                failures,
                due: Instant::now() + retry_backoff(failures, self.opts.latency),
            },
        );
    }

    /// Whether `path`'s bytes on disk differ from the bytes the client
    /// holds — the question the reconciler could not answer for a
    /// racily-clean entry (docs/design/fs-watch.md).
    ///
    /// Reads the file rather than going through [`Self::read_content`]: the
    /// blob store and the learned-hash map are both keyed on the stat that
    /// is under suspicion here, so consulting either would answer with the
    /// stale bytes it is our job to catch. Files the client has no content
    /// for — a metadata-only sync, or one over the inline cap — answer
    /// "no": there is nothing of theirs that could be stale.
    fn content_diverged(&self, path: &str, canonical: &Index) -> bool {
        if !self.opts.content {
            return false;
        }
        let Some(&held) = self.held.get(path) else {
            return false;
        };
        let Some(meta) = canonical.get(path) else {
            return false;
        };
        if !matches!(meta.node_type, FS_ENTRY_FILE | FS_ENTRY_SYMLINK) {
            return false;
        }
        let Some(abs) = resolve_wire_path(&self.root, path) else {
            return false;
        };
        match read_verified_meta(&abs) {
            ReadMetaOutcome::Stable(data, _) => blake3_128(&data) != held,
            // Churning or unreadable: the ordinary retry path owns it, and
            // guessing "changed" here would resend on every tick.
            ReadMetaOutcome::Unstable | ReadMetaOutcome::Unreadable => false,
        }
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
        // The snapshot may predate another sync's hash learning (hash
        // publishes coalesce): consult the shared learned map, validated
        // against this snapshot's stat exactly as the reconciler validates
        // HashLearned, so a concurrent content sync serves from the blob
        // store instead of re-reading the tree.
        if meta.hash == 0
            && let Some(learned) = self.shared.learned.lock().unwrap().get(path).cloned()
            && learned.hash != 0
            && learned.node_type == meta.node_type
            && learned.dev_ino == meta.dev_ino
            && learned.size == meta.size
            && learned.mtime_ns == meta.mtime_ns
            && let Some(data) = blob_store().lock().unwrap().get(learned.hash)
        {
            return ContentRead::Stable {
                hash: learned.hash,
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
                    self.teach_hash(path, stat);
                }
                ContentRead::Stable { hash, data }
            }
            ReadMetaOutcome::Unstable => ContentRead::Unstable,
            ReadMetaOutcome::Unreadable => ContentRead::Unreadable,
        }
    }

    /// Teach the reconciler (and, immediately, sibling engines via the
    /// shared learned map) a verified content hash.
    fn teach_hash(&self, path: &str, meta: NodeMeta) {
        {
            let mut learned = self.shared.learned.lock().unwrap();
            // Coarse bound: entries only bridge the hash-publish window,
            // so dropping them all merely costs a re-read.
            if learned.len() >= 65536 {
                learned.clear();
            }
            learned.insert(path.to_string(), meta.clone());
        }
        let _ = self.shared.tx.send(RootMsg::HashLearned {
            path: path.to_string(),
            meta,
        });
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
        for path in subtree_keys(&self.retry, from) {
            if let Some(entry) = self.retry.remove(&path) {
                self.retry
                    .insert(rebase_subtree_path(&path, from, to), entry);
            }
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
                Ok(SyncMsg::Cmd(Command::UploadBegin(b))) => {
                    if !self.handle_upload_begin(b) {
                        return Err(Exit::ClientGone);
                    }
                }
                Ok(SyncMsg::Cmd(Command::UploadChunk {
                    upload_id,
                    offset,
                    data,
                })) => {
                    if !self.handle_upload_chunk(upload_id, offset, data) {
                        return Err(Exit::ClientGone);
                    }
                }
                Ok(SyncMsg::Cmd(Command::UploadFinish {
                    nonce, upload_id, ..
                })) => {
                    if !self.handle_upload_finish(nonce, upload_id) {
                        return Err(Exit::ClientGone);
                    }
                }
                Ok(SyncMsg::Cmd(Command::UploadCancel { upload_id })) => {
                    self.handle_upload_cancel(upload_id);
                }
                Ok(SyncMsg::Cmd(Command::Stop)) => return Err(Exit::Stopped),
                Ok(SyncMsg::Root(update)) => self.handle_root(update)?,
                Err(_) => return Err(Exit::ClientGone),
            }
        }
        Ok(())
    }

    fn handle_fetch(&mut self, nonce: u16, wire_path: &str) -> bool {
        if self.single {
            // A SINGLE sync's namespace holds exactly one path, "" — the
            // root file itself, already canonical, so `confine_target`'s
            // parent-of-target check (built for paths *under* a directory
            // root) does not apply. Anything else does not exist.
            let msg = if wire_path.is_empty() {
                let root = self.root.clone();
                self.fetch_confined(nonce, &root)
            } else {
                msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[])
            };
            return (self.outbox)(msg);
        }
        // Confine exactly as the write path does: resolve_wire_path alone
        // validates components but performs no symlink resolution, so an
        // in-tree symlink in an intermediate component would let fs::read
        // follow it out of root (arbitrary file read). Canonicalizing the
        // parent and re-checking starts_with(root) closes that.
        let msg = match confine_target(&self.root, wire_path) {
            Err(ConfineError::Invalid) => msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[]),
            Err(ConfineError::Io(e)) if e.kind() == io::ErrorKind::NotFound => {
                msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[])
            }
            Err(ConfineError::Io(_)) => msg_fs_file(nonce, FS_FILE_UNREADABLE, &[]),
            Err(ConfineError::Escapes) => msg_fs_file(nonce, blit_remote::fs::FS_FILE_OTHER, &[]),
            Ok(abs) => self.fetch_confined(nonce, &abs),
        };
        (self.outbox)(msg)
    }

    /// Read a confined fetch target. Only regular files and symlinks carry
    /// fetchable content (a symlink's is its own target bytes, never the
    /// file it points at); refusing fifos/devices/sockets keeps `fs::read`
    /// from blocking the engine thread forever on a device node reached
    /// through the tree.
    fn fetch_confined(&self, nonce: u16, abs: &Path) -> Vec<u8> {
        let md = match fs::symlink_metadata(abs) {
            Ok(md) => md,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[]);
            }
            Err(_) => return msg_fs_file(nonce, FS_FILE_UNREADABLE, &[]),
        };
        let ft = md.file_type();
        if !ft.is_file() && !ft.is_symlink() {
            return msg_fs_file(nonce, blit_remote::fs::FS_FILE_OTHER, &[]);
        }
        // Refuse oversized files before reading a byte: an FS_FILE whose
        // decompressed payload exceeds the protocol cap could not be parsed
        // by a compliant client anyway, and reading it would spike transient
        // memory unbounded (docs/fs-watch.md).
        if ft.is_file() && md.len() > blit_remote::fs::FS_MAX_DECOMPRESSED as u64 {
            return msg_fs_file(nonce, blit_remote::fs::FS_FILE_OTHER, &[]);
        }
        match read_verified(abs) {
            ReadOutcome::Stable(data) => msg_fs_file(nonce, FS_FILE_OK, &data),
            ReadOutcome::Unstable => msg_fs_file(nonce, FS_FILE_UNREADABLE, &[]),
            ReadOutcome::Unreadable => {
                if abs.exists() {
                    msg_fs_file(nonce, FS_FILE_UNREADABLE, &[])
                } else {
                    msg_fs_file(nonce, FS_FILE_NOT_FOUND, &[])
                }
            }
        }
    }

    /// Resolve a write-family target for this sync. A SINGLE sync's
    /// namespace is exactly one path — the empty wire path, naming the
    /// root file — so any other path answers INVALID, and the
    /// final-component symlink policy applies to the root itself (a
    /// symlink can be renamed over it after validation). A followed
    /// symlink's resolution necessarily leaves the one-file namespace, so
    /// Follow refuses it exactly as an out-of-root target under a
    /// directory sync.
    fn resolve_target(&self, wire: &str, policy: SymlinkPolicy) -> Result<PathBuf, u8> {
        if !self.single {
            return resolve_write_target(&self.root, wire, policy);
        }
        if !wire.is_empty() {
            return Err(FS_DONE_INVALID);
        }
        match fs::symlink_metadata(&self.root) {
            Ok(md) if md.file_type().is_symlink() => match policy {
                SymlinkPolicy::Refuse => Err(FS_DONE_PERMISSION),
                SymlinkPolicy::Operate => Ok(self.root.clone()),
                SymlinkPolicy::Follow => {
                    let resolved = fs::canonicalize(&self.root).map_err(|e| write_io_status(&e))?;
                    if resolved == self.root {
                        Ok(resolved)
                    } else {
                        Err(FS_DONE_PERMISSION)
                    }
                }
            },
            _ => Ok(self.root.clone()),
        }
    }

    fn handle_write(&mut self, w: WriteReq) -> bool {
        let (status, hash, mtime_ns) = self.exec_write(&w);
        (self.outbox)(msg_fs_done(w.nonce, status, hash, mtime_ns))
    }

    /// Land a content write under the target's per-file write lock: confine
    /// the path, enforce the CAS precondition against the freshly re-read
    /// live hash — for `FS_WRITE_CONTENT_DELTA`, apply the instruction
    /// stream against the verified base bytes — write atomically (or
    /// create-exclusive), then prime the echo.
    fn exec_write(&mut self, w: &WriteReq) -> (u8, u128, u64) {
        use blit_remote::fs::{FS_WRITE_CONTENT_DELTA, FS_WRITE_CONTENT_FULL, apply_fs_delta};
        let is_delta = w.content_kind == FS_WRITE_CONTENT_DELTA;
        // Kinds 0/1 are full bytes, 2 a delta; anything else is a future
        // encoding this server does not speak.
        if !is_delta && w.content_kind != 0 && w.content_kind != FS_WRITE_CONTENT_FULL {
            return (FS_DONE_INVALID, 0, 0);
        }
        let no_cas = w.flags & FS_WRITE_NO_CAS != 0;
        // A delta applies against the exact bytes the CAS `base` names
        // (docs/design/fs-write.md "Wire"): NO_CAS has no precondition and
        // a zero base means "absent", so neither can anchor one.
        if is_delta && (no_cas || w.base == 0) {
            return (FS_DONE_INVALID, 0, 0);
        }
        if w.content.len() as u64 > fs_write_max() {
            return (FS_DONE_TOO_LARGE, 0, 0);
        }
        // A SINGLE sync's target is the root file itself: its parent exists
        // by construction (the watch sits on it), so MKPARENTS is a no-op.
        if !self.single
            && w.flags & FS_WRITE_MKPARENTS != 0
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
        let target = match self.resolve_target(&w.path, policy) {
            Ok(t) => t,
            Err(status) => return (status, 0, 0),
        };
        let durable = w.flags & FS_WRITE_DURABLE != 0;

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
        // CAS check — and, for a delta, base production. The two are one
        // verified read: the target's current content must hash to `base`
        // (else CONFLICT carrying the live hash, exactly as a full write),
        // and those verified bytes ARE the delta base. A base the server
        // cannot produce is therefore precisely a failed precondition —
        // a corrupted apply is impossible by construction.
        let applied: Option<Vec<u8>> = if is_delta {
            // Bound the base read like the write payload: the on-disk file
            // is unbounded, and an unbounded `fs::read` would let a tiny
            // request force an arbitrarily large allocation (full-write
            // CAS streams its hash for the same reason).
            match fs::symlink_metadata(&target) {
                Ok(md) if md.len() > fs_write_max() => return (FS_DONE_TOO_LARGE, 0, 0),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Absent target: a non-zero base cannot match; the
                    // conflict hash is the "absent" zero sentinel.
                    return (FS_DONE_CONFLICT, 0, 0);
                }
                _ => {}
            }
            let base = match read_verified_meta(&target) {
                ReadMetaOutcome::Stable(data, _) => data,
                // Actively churning under an external writer: the
                // precondition cannot be confirmed, and applying blind
                // could corrupt.
                ReadMetaOutcome::Unstable => return (FS_DONE_OTHER, 0, 0),
                ReadMetaOutcome::Unreadable => {
                    return if target.exists() {
                        (FS_DONE_OTHER, 0, 0)
                    } else {
                        (FS_DONE_CONFLICT, 0, 0)
                    };
                }
            };
            let cur = blake3_128(&base);
            if cur != w.base {
                return (FS_DONE_CONFLICT, cur, 0);
            }
            let Some(applied) = apply_fs_delta(&base, &w.content) else {
                // Malformed instruction stream.
                return (FS_DONE_INVALID, 0, 0);
            };
            if applied.len() as u64 > fs_write_max() {
                return (FS_DONE_TOO_LARGE, 0, 0);
            }
            Some(applied)
        } else {
            if let Err((status, hash)) = check_write_precondition(&target, w.base, no_cas) {
                return (status, hash, 0);
            }
            None
        };
        let content: &[u8] = applied.as_deref().unwrap_or(&w.content);

        let hash = blake3_128(content);
        if create_exclusive_mode {
            match create_exclusive(&target, content, w.mode, durable) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return (FS_DONE_CONFLICT, current_hash(&target), 0);
                }
                Err(e) => return (write_io_status(&e), 0, 0),
            }
        } else if let Err(e) = write_atomic(&target, content, w.mode, durable) {
            return (write_io_status(&e), 0, 0);
        }

        let mtime_ns = stat_meta(&target).map(|m| m.mtime_ns).unwrap_or(0);
        // Key the echo by the path the write actually landed under — which
        // is the resolved target, not the client's wire path, when a
        // symlink was followed. Otherwise the two coincide.
        let echo_wire = wire_key_for(&self.root, &target).unwrap_or_else(|| w.path.clone());
        self.prime_echo(&echo_wire, &target, hash, content, mtime_ns);
        (FS_DONE_OK, hash, mtime_ns)
    }

    fn handle_op(&mut self, o: OpReq) -> bool {
        let (status, hash, mtime_ns) = self.exec_op(&o);
        (self.outbox)(msg_fs_done(o.nonce, status, hash, mtime_ns))
    }

    fn handle_upload_begin(&mut self, b: UploadBeginReq) -> bool {
        let (status, hash) = self.exec_upload_begin(&b);
        (self.outbox)(msg_fs_upload_begin_result(
            b.nonce,
            status,
            b.upload_id,
            hash,
            0,
        ))
    }

    /// Validate an upload begin and stage its temp sibling. The CAS
    /// precondition (`base`, FS_WRITE semantics) is checked here — fail
    /// fast, before any bytes flow — and re-verified at FINISH. The id is
    /// server-allocated and unique per connection, so a duplicate here means
    /// a dispatcher bug and is refused rather than silently replacing the
    /// live upload (whose temp its Drop would remove).
    fn exec_upload_begin(&mut self, b: &UploadBeginReq) -> (u8, u128) {
        if b.flags & !FS_UPLOAD_FLAGS_KNOWN != 0 {
            return (FS_DONE_INVALID, 0);
        }
        if b.size > fs_upload_max() {
            return (FS_DONE_TOO_LARGE, 0);
        }
        // A SINGLE sync's parent exists by construction; MKPARENTS is a
        // no-op there exactly as for FS_WRITE.
        if !self.single
            && b.flags & FS_UPLOAD_MKPARENTS != 0
            && let Some(parent) = resolve_wire_path(&self.root, &b.path)
                .and_then(|a| a.parent().map(Path::to_path_buf))
            && let Err(status) = create_parents_confined(&self.root, &parent)
        {
            return (status, 0);
        }
        let policy = if b.flags & FS_UPLOAD_FOLLOW_SYMLINK != 0 {
            SymlinkPolicy::Follow
        } else {
            SymlinkPolicy::Refuse
        };
        let target = match self.resolve_target(&b.path, policy) {
            Ok(t) => t,
            Err(status) => return (status, 0),
        };
        if self.uploads.contains_key(&b.upload_id) {
            return (FS_DONE_INVALID, 0);
        }
        // Serialize check-and-stage against every other blit writer of this
        // exact file, as a one-shot write's check-and-write does.
        let lock = path_write_lock(&target);
        let _guard = lock.lock().unwrap();
        // Never clobber a directory with a file (re-checked at FINISH under
        // the same lock — the target may change type mid-upload).
        if fs::symlink_metadata(&target)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return (FS_DONE_WRONG_TYPE, 0);
        }
        if let Err(conflict) =
            check_write_precondition(&target, b.base, b.flags & FS_UPLOAD_NO_CAS != 0)
        {
            return conflict;
        }
        let tmp = temp_sibling(&target);
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        if b.mode != 0 {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(b.mode);
        }
        let f = match opts.open(&tmp) {
            Ok(f) => f,
            Err(e) => return (write_io_status(&e), 0),
        };
        // mode 0 preserves the replaced file's mode, as write_atomic does.
        apply_mode(&f, &target, b.mode);
        self.uploads.insert(
            b.upload_id,
            Upload {
                tmp,
                wire: b.path.clone(),
                file: Some(f),
                received: 0,
                size: b.size,
                durable: b.flags & FS_UPLOAD_DURABLE != 0,
                base: b.base,
                no_cas: b.flags & FS_UPLOAD_NO_CAS != 0,
                follow_symlink: b.flags & FS_UPLOAD_FOLLOW_SYMLINK != 0,
                landed: false,
            },
        );
        (FS_DONE_OK, 0)
    }

    /// Append one chunk. Transports are ordered, so the chunk's offset must
    /// equal the bytes accepted so far; on mismatch the ack reports the
    /// resume point. A failed write keeps `received` where it was, so the
    /// client can retry the same offset.
    fn handle_upload_chunk(&mut self, upload_id: u16, offset: u64, data: Vec<u8>) -> bool {
        let (status, received) = match self.uploads.get_mut(&upload_id) {
            None => (FS_DONE_UNKNOWN_UPLOAD, 0),
            Some(up) => {
                if offset != up.received {
                    (FS_DONE_OFFSET_MISMATCH, up.received)
                } else if up.received + data.len() as u64 > up.size {
                    // Past the declared total: refuse the chunk whole, never
                    // truncate it — a partial append would corrupt the file.
                    (FS_DONE_TOO_LARGE, up.received)
                } else {
                    use std::io::Write as _;
                    let file = up.file.as_mut().expect("live upload has a file");
                    match file.write_all(&data) {
                        Ok(()) => {
                            up.received += data.len() as u64;
                            (FS_DONE_OK, up.received)
                        }
                        Err(e) => (write_io_status(&e), up.received),
                    }
                }
            }
        };
        (self.outbox)(msg_fs_upload_chunk_result(upload_id, status, received))
    }

    fn handle_upload_finish(&mut self, nonce: u16, upload_id: u16) -> bool {
        let (status, hash, mtime_ns) = self.exec_upload_finish(upload_id);
        (self.outbox)(msg_fs_upload_finish_result(nonce, status, hash, mtime_ns))
    }

    /// Land an upload: verify the byte count, re-resolve the wire path and
    /// re-verify the CAS precondition under the target's write lock (the
    /// BEGIN-time checks are TOCTOU-stale after a long transfer), fsync
    /// when DURABLE, rename the temp over the target, then prime the echo
    /// exactly as a one-shot write does. FINISH terminates the upload
    /// whatever happens — on failure `up`'s Drop removes the temp.
    fn exec_upload_finish(&mut self, upload_id: u16) -> (u8, u128, u64) {
        let Some(mut up) = self.uploads.remove(&upload_id) else {
            return (FS_DONE_UNKNOWN_UPLOAD, 0, 0);
        };
        if up.received != up.size {
            return (FS_DONE_SIZE_MISMATCH, 0, 0);
        }
        // Re-resolve as at BEGIN: the entry the wire path names may have
        // changed type, grown a symlink, or been retargeted mid-upload.
        let policy = if up.follow_symlink {
            SymlinkPolicy::Follow
        } else {
            SymlinkPolicy::Refuse
        };
        let target = match self.resolve_target(&up.wire, policy) {
            Ok(t) => t,
            Err(status) => return (status, 0, 0),
        };
        let lock = path_write_lock(&target);
        let _guard = lock.lock().unwrap();
        if fs::symlink_metadata(&target)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            return (FS_DONE_WRONG_TYPE, 0, 0);
        }
        if let Err((status, hash)) = check_write_precondition(&target, up.base, up.no_cas) {
            return (status, hash, 0);
        }
        // Create-exclusive caveat: unlike FS_WRITE's O_EXCL create, the
        // rename below cannot fail on an entry an external creator lands in
        // this window — the path lock only serializes blit writers.
        let file = up.file.take().expect("live upload has a file");
        if up.durable
            && let Err(e) = file.sync_all()
        {
            return (write_io_status(&e), 0, 0);
        }
        // Closed before the rename, as write_atomic does (and as Windows
        // requires).
        drop(file);
        if let Err(e) = fs::rename(&up.tmp, &target) {
            return (write_io_status(&e), 0, 0);
        }
        up.landed = true;
        #[cfg(unix)]
        if up.durable
            && let Ok(d) = fs::File::open(target.parent().unwrap_or_else(|| Path::new(".")))
        {
            let _ = d.sync_all();
        }
        let mtime_ns = stat_meta(&target).map(|m| m.mtime_ns).unwrap_or(0);
        let echo_wire = wire_key_for(&self.root, &target).unwrap_or_else(|| up.wire.clone());
        // Small files prime the full echo from their bytes, like a write;
        // large ones stream the hash and skip the blob store — re-reading
        // up to a gigabyte to cache content the reconciler will not inline
        // anyway would double the I/O for nothing.
        if up.size <= fs_write_max() {
            match fs::read(&target) {
                Ok(bytes) => {
                    let hash = blake3_128(&bytes);
                    self.prime_echo(&echo_wire, &target, hash, &bytes, mtime_ns);
                    (FS_DONE_OK, hash, mtime_ns)
                }
                Err(e) => (write_io_status(&e), 0, 0),
            }
        } else {
            match hash_file_streamed(&target) {
                Ok(hash) => {
                    self.prime_echo_unstored(&echo_wire, &target, hash, mtime_ns);
                    (FS_DONE_OK, hash, mtime_ns)
                }
                Err(e) => (write_io_status(&e), 0, 0),
            }
        }
    }

    fn handle_upload_cancel(&mut self, upload_id: u16) {
        // Drop removes the temp file. Unknown ids are a no-op, matching
        // FS_STOP of an unknown sync.
        self.uploads.remove(&upload_id);
    }

    /// Execute a metadata op (mkdir/remove/rename), each under the affected
    /// path's per-file write lock.
    fn exec_op(&mut self, o: &OpReq) -> (u8, u128, u64) {
        match o.op {
            FS_OP_MKDIR => {
                if !self.single
                    && o.flags & FS_OP_MKPARENTS != 0
                    && let Some(parent) = resolve_wire_path(&self.root, &o.a)
                        .and_then(|a| a.parent().map(Path::to_path_buf))
                    && let Err(status) = create_parents_confined(&self.root, &parent)
                {
                    return (status, 0, 0);
                }
                let target = match self.resolve_target(&o.a, SymlinkPolicy::Operate) {
                    Ok(t) => t,
                    Err(status) => return (status, 0, 0),
                };
                let lock = path_write_lock(&target);
                let _guard = lock.lock().unwrap();
                let builder = fs::DirBuilder::new();
                #[cfg(unix)]
                let mut builder = builder;
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
                let target = match self.resolve_target(&o.a, SymlinkPolicy::Operate) {
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
                if !self.single
                    && o.flags & FS_OP_MKPARENTS != 0
                    && let Some(parent) = resolve_wire_path(&self.root, &o.b)
                        .and_then(|a| a.parent().map(Path::to_path_buf))
                    && let Err(status) = create_parents_confined(&self.root, &parent)
                {
                    return (status, 0, 0);
                }
                let from = match self.resolve_target(&o.a, SymlinkPolicy::Operate) {
                    Ok(t) => t,
                    Err(status) => return (status, 0, 0),
                };
                let lock = path_write_lock(&from);
                let _guard = lock.lock().unwrap();
                if fs::symlink_metadata(&from).is_err() {
                    return (FS_DONE_NOT_FOUND, 0, 0);
                }
                let to = match self.resolve_target(&o.b, SymlinkPolicy::Operate) {
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
        if !self.single
            && o.flags & FS_OP_MKPARENTS != 0
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
            let src = match self.resolve_target(&o.a, SymlinkPolicy::Operate) {
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
        let link = match self.resolve_target(&o.b, SymlinkPolicy::Operate) {
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
            self.teach_hash(wire, meta);
        }
        self.hint_change(abs);
    }

    /// [`prime_echo`] without the blob-store insert, for files too large to
    /// be worth caching (chunked uploads over the inline cap): the echo and
    /// hash teaching behave the same, only cross-client content serving
    /// falls back to reading the file.
    fn prime_echo_unstored(&mut self, wire: &str, abs: &Path, hash: u128, mtime_ns: u64) {
        self.held.insert(wire.to_string(), hash);
        if !racily_clean(mtime_ns)
            && let Ok(mut meta) = stat_meta(abs)
        {
            meta.hash = hash;
            self.teach_hash(wire, meta);
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
            ignores: IgnoreSpec::default(),
        }
    }

    /// [`test_key`] with an ignore spec — a *different* shared root, since
    /// the spec is part of the key.
    fn test_key_ignoring(root: &Path, ignores: IgnoreSpec) -> RootKey {
        RootKey {
            ignores,
            ..test_key(root)
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
            link_dir: false,
            filtered: false,
        }
    }

    /// A flag flip with an otherwise identical stat still produces a record.
    ///
    /// `filtered` and `link_dir` are the two wire-visible bits that are not
    /// properties of the inode, so nothing about a fresh stat implies them.
    /// In the field the flip usually rides along with a stat change — the
    /// excluded child that set `filtered` also bumped its parent's mtime —
    /// which is why a diff that ignored the flags looked correct until two
    /// writes shared one timestamp tick. That is the CI deadlock in #124:
    /// no record, and the hint path stops nudging once the canonical entry
    /// is `filtered`, so the client never hears about it. Asserting on the
    /// mechanism keeps this deterministic instead of timestamp-dependent.
    #[test]
    fn diff_reports_a_flag_flip_under_an_unchanged_stat() {
        for (label, flip) in [
            (
                "filtered",
                (|m: &mut NodeMeta| m.filtered = true) as fn(&mut NodeMeta),
            ),
            ("link_dir", |m: &mut NodeMeta| m.link_dir = true),
        ] {
            let mut prev = Index::new();
            prev.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
            prev.insert("d".into(), meta(FS_ENTRY_DIR, 0, 0, 2));
            let mut curr = prev.clone();
            flip(curr.get_mut("d").unwrap());

            let changed = std::collections::BTreeSet::from(["d".to_string()]);
            for (how, ops) in [
                ("diff", diff(&prev, &curr)),
                ("diff_changed", diff_changed(&prev, &curr, &changed)),
            ] {
                let [
                    DiffOp::Upsert {
                        path,
                        content_changed,
                    },
                ] = &ops[..]
                else {
                    panic!("{label} via {how}: expected one Upsert, got {ops:?}");
                };
                assert_eq!(path, "d", "{label} via {how}");
                // A flag is metadata: the client re-reads flags, not bytes.
                assert!(!content_changed, "{label} via {how} asked for content");
            }
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
        drive_engine_keyed(test_key(root))
    }

    fn drive_engine_keyed(key: RootKey) -> (Arc<Mutex<Vec<Vec<u8>>>>, SyncHandle, HintSender) {
        let shared = open_root_unwatched(key);
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

    /// Drive one engine over a SINGLE (one-file) shared root, hint-driven.
    fn drive_single_engine(file: &Path) -> (Arc<Mutex<Vec<Vec<u8>>>>, SyncHandle, HintSender) {
        let shared = open_single_root_unwatched(file.to_path_buf());
        assert!(shared.is_single());
        let hint_tx = shared.hint_sender();
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = SyncOptions {
            content: true,
            recursive: false,
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

    fn count_updates(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> usize {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|m| m[0] == blit_remote::fs::S2C_FS_UPDATE)
            .count()
    }

    fn count_closed(sent: &Arc<Mutex<Vec<Vec<u8>>>>) -> usize {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|m| m[0] == blit_remote::fs::S2C_FS_CLOSED)
            .count()
    }

    /// Apply every unseen FS_UPDATE to `mirror`, acking as the client would.
    fn pump_mirror(
        sent: &Arc<Mutex<Vec<Vec<u8>>>>,
        handle: &SyncHandle,
        mirror: &mut FsMirror,
        seen: &mut usize,
    ) {
        let msgs = sent.lock().unwrap().clone();
        for msg in &msgs[*seen..] {
            if msg[0] == blit_remote::fs::S2C_FS_UPDATE {
                let id = mirror.apply_update(msg).expect("valid update");
                handle.command(Command::Ack(id));
            }
        }
        *seen = msgs.len();
    }

    /// Pump until `pred(mirror)` holds or the deadline passes.
    fn pump_until(
        sent: &Arc<Mutex<Vec<Vec<u8>>>>,
        handle: &SyncHandle,
        mirror: &mut FsMirror,
        seen: &mut usize,
        what: &str,
        pred: impl Fn(&FsMirror) -> bool,
    ) {
        pump_until_nudging(sent, handle, mirror, seen, what, || {}, pred)
    }

    /// `pump_until`, re-sending `nudge` on every poll.
    ///
    /// For the waits that hang off a *single* engine-side transition — an
    /// excluded child re-listing its parent once, and only once, so the flag
    /// flip costs one listing rather than one per event — a lone hint has to
    /// win against the write becoming visible to that listing. A backend
    /// keeps hinting as long as anything moves; these tests do not, so they
    /// re-send rather than depend on one delivery landing in the right order.
    /// The generous deadline is for a loaded machine (the coverage job runs
    /// every crate's tests at once, instrumented), not for a slow engine:
    /// when nothing is wrong these return in milliseconds.
    fn pump_until_nudging(
        sent: &Arc<Mutex<Vec<Vec<u8>>>>,
        handle: &SyncHandle,
        mirror: &mut FsMirror,
        seen: &mut usize,
        what: &str,
        nudge: impl Fn(),
        pred: impl Fn(&FsMirror) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            pump_mirror(sent, handle, mirror, seen);
            if pred(mirror) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; live = {:?}",
                mirror.live.keys().collect::<Vec<_>>()
            );
            nudge();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// SINGLE sync lifecycle (docs/design/fs-watch.md "Single-file sync"):
    /// the initial snapshot is exactly one entry keyed "", external
    /// modifications flow, sibling churn never wakes the sync,
    /// delete/recreate and rename-away/rename-back flow as DELETE/UPSERT
    /// of "" without closing, same-file opens share one root, and FS_STOP
    /// tears down with FS_CLOSED(client request).
    #[test]
    fn single_sync_lifecycle() {
        let dir = temp_dir().canonicalize().unwrap();
        let file = dir.join("note.txt");
        let sibling = dir.join("sibling.txt");
        fs::write(&file, b"v1").unwrap();
        fs::write(&sibling, b"noise").unwrap();

        // Same-file opens share one root; a directory open of the parent
        // coexists without joining it (the flag set is part of the key).
        let shared = open_single_root_unwatched(file.clone());
        assert!(Arc::ptr_eq(
            &shared,
            &open_single_root_unwatched(file.clone())
        ));
        let dir_root = open_root_unwatched(test_key(&dir));
        assert!(!Arc::ptr_eq(&shared, &dir_root));
        drop(dir_root);
        drop(shared);

        let (sent, handle, hint) = drive_single_engine(&file);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;

        // Initial snapshot: exactly the one "" entry, content attached.
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial ''", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"v1"[..]))
        });
        assert_eq!(mirror.live.len(), 1, "mirror holds exactly the root");
        let node = &mirror.live[""];
        assert_eq!(node.entry_flags & FS_ENTRY_TYPE_MASK, FS_ENTRY_FILE);
        assert_eq!(node.hash, blake3_128(b"v1"));

        // Sibling churn must not wake the sync: no FS_UPDATE flows.
        let quiet = count_updates(&sent);
        fs::write(&sibling, b"more noise").unwrap();
        fs::write(dir.join("new-sibling.txt"), b"x").unwrap();
        hint.send(Hint::Dirty(sibling.clone()));
        hint.send(Hint::Dirty(dir.join("new-sibling.txt")));
        std::thread::sleep(Duration::from_millis(120));
        pump_mirror(&sent, &handle, &mut mirror, &mut seen);
        assert_eq!(
            count_updates(&sent),
            quiet,
            "sibling churn woke the single sync"
        );
        assert_eq!(mirror.live[""].content.as_deref(), Some(&b"v1"[..]));

        // An external modify flows (file-level hint).
        fs::write(&file, b"v2").unwrap();
        hint.send(Hint::Dirty(file.clone()));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "v2", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"v2"[..]))
        });

        // A parent-level hint (directory-granular backends) also re-verifies.
        fs::write(&file, b"v3").unwrap();
        hint.send(Hint::Dirty(dir.clone()));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "v3", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"v3"[..]))
        });

        // Delete flows as DELETE of "" — the sync stays open.
        fs::remove_file(&file).unwrap();
        hint.send(Hint::Dirty(file.clone()));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "delete", |m| {
            m.live.is_empty()
        });
        assert_eq!(count_closed(&sent), 0, "delete must not close the sync");

        // Recreate flows back as an UPSERT of "".
        fs::write(&file, b"v4").unwrap();
        hint.send(Hint::Dirty(file.clone()));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "recreate", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"v4"[..]))
        });

        // Rename away (the watch survives on the parent), then back.
        let away = dir.join("renamed.txt");
        fs::rename(&file, &away).unwrap();
        hint.send(Hint::Dirty(file.clone()));
        hint.send(Hint::Dirty(away.clone()));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "rename away", |m| {
            m.live.is_empty()
        });
        fs::rename(&away, &file).unwrap();
        hint.send(Hint::Dirty(file.clone()));
        hint.send(Hint::Dirty(away.clone()));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "rename back", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"v4"[..]))
        });
        assert_eq!(count_closed(&sent), 0);

        // Teardown: FS_CLOSED(client request).
        handle.command(Command::Stop);
        let deadline = Instant::now() + Duration::from_secs(5);
        while count_closed(&sent) == 0 {
            assert!(Instant::now() < deadline, "no FS_CLOSED after Stop");
            std::thread::sleep(Duration::from_millis(2));
        }
        let closed = sent
            .lock()
            .unwrap()
            .iter()
            .find(|m| m[0] == blit_remote::fs::S2C_FS_CLOSED)
            .unwrap()
            .clone();
        assert_eq!(closed[3], FS_CLOSED_CLIENT_REQUEST);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A SINGLE root's validation: directories answer the invalid-path
    /// With `cross_filesystem` off (the default), a symlink to a directory on
    /// another mount is reported but never descended — on the initial scan and,
    /// the case that actually regressed, on an incremental reconcile.
    ///
    /// The reconcile pre-check cannot catch this one: a symlink's `dev_ino`
    /// comes from `lstat`, so it reports the device the *link* lives on (the
    /// root's), sailing past the guard that stops real foreign-device
    /// directories. `scan_into` was then called with `root_dev: None`, which
    /// re-anchored the bound to the target's device and indexed the whole
    /// cross-device subtree.
    ///
    /// Uses /dev/shm as the second filesystem; skipped when it is absent, not
    /// writable, or happens to share a device with the temp dir.
    #[cfg(target_os = "linux")]
    #[test]
    fn cross_device_symlink_is_not_descended_on_reconcile() {
        use std::os::unix::fs::MetadataExt;

        let dir = temp_dir().canonicalize().unwrap();
        let Ok(shm) = std::path::Path::new("/dev/shm").canonicalize() else {
            return;
        };
        let foreign = shm.join(format!("blit-xdev-{}", std::process::id()));
        if fs::create_dir_all(foreign.join("inner")).is_err() {
            return;
        }
        // Guard the premise: without two devices this proves nothing.
        let (Ok(a), Ok(b)) = (fs::metadata(&dir), fs::metadata(&foreign)) else {
            let _ = fs::remove_dir_all(&foreign);
            return;
        };
        if a.dev() == b.dev() {
            let _ = fs::remove_dir_all(&foreign);
            return;
        }
        fs::write(foreign.join("inner/secret.txt"), b"elsewhere").unwrap();
        fs::write(dir.join("local.txt"), b"here").unwrap();

        // cross_filesystem defaults to false in test_key.
        let (sent, handle, hint) = drive_engine(&dir);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("local.txt")
        });

        // Create the link *after* the first snapshot, so the hint takes the
        // "new (or type-changed) directory" branch — the trigger.
        std::os::unix::fs::symlink(&foreign, dir.join("far")).unwrap();
        hint.send(Hint::Dirty(dir.join("far")));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "link entry", |m| {
            m.live.contains_key("far")
        });

        // The link is reported — it is the boundary, like a mount point — but
        // nothing beyond it is indexed. Snapshot the verdict, then tear down
        // *before* asserting: /dev/shm is RAM, and a failing assert would
        // otherwise leave the fixture behind.
        let leaked: Vec<String> = mirror
            .live
            .keys()
            .filter(|k| k.starts_with("far/"))
            .cloned()
            .collect();
        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&foreign);

        assert!(
            leaked.is_empty(),
            "cross_filesystem is off: a symlink to another device must not be \
             descended, found {leaked:?}"
        );
    }

    /// A filtered root arms one watch per indexed directory instead of one
    /// recursive watch over everything, so an excluded subtree costs no
    /// descriptors (docs/design/fs-watch.md "Ignoring"). The risk that
    /// buys is a lost event, so this drives the *real* backend: changes
    /// several levels down, in directories created after the initial scan,
    /// must still arrive — while the excluded subtree stays absent and
    /// unarmed.
    #[cfg(target_os = "linux")]
    #[test]
    fn per_directory_watching_still_delivers_every_change() {
        let root = temp_dir().canonicalize().unwrap();
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        fs::write(root.join("src/deep/seed.txt"), b"seed").unwrap();

        let key = test_key_ignoring(
            &root,
            IgnoreSpec {
                gitignore: true,
                dot_ignore: true,
                exclude_git: true,
                patterns: Vec::new(),
            },
        );
        let shared = open_root(key).expect("arm native watch");
        assert!(
            shared
                ._backend
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|b| b.watches.is_per_dir()),
            "a filtered root on Linux arms per directory"
        );
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let handle = start_sync(
            &shared,
            9,
            SyncOptions {
                content: true,
                latency: Duration::from_millis(5),
                ..Default::default()
            },
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("src/deep/seed.txt")
        });

        // A write two levels down, seen only through the watch armed on
        // that directory during the scan.
        fs::write(root.join("src/deep/seed.txt"), b"changed").unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "deep write", |m| {
            m.live
                .get("src/deep/seed.txt")
                .is_some_and(|n| n.content.as_deref() == Some(&b"changed"[..]))
        });

        // A directory created *after* the scan has to be armed by the
        // reconcile path, and its children reported through that new watch
        // — the arm-before-list contract, one level down.
        fs::create_dir(root.join("src/fresh")).unwrap();
        fs::write(root.join("src/fresh/a.txt"), b"a").unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "fresh dir", |m| {
            m.live.contains_key("src/fresh/a.txt")
        });
        fs::write(root.join("src/fresh/b.txt"), b"b").unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "fresh child", |m| {
            m.live.contains_key("src/fresh/b.txt")
        });

        // Deleting it disarms; recreating re-arms and still delivers.
        fs::remove_dir_all(root.join("src/fresh")).unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "dir gone", |m| {
            !m.live.contains_key("src/fresh")
        });
        fs::create_dir(root.join("src/fresh")).unwrap();
        fs::write(root.join("src/fresh/c.txt"), b"c").unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "re-armed", |m| {
            m.live.contains_key("src/fresh/c.txt")
        });

        // Meanwhile the excluded subtree was never armed and never seen.
        fs::write(root.join("node_modules/pkg/index.js"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        pump_mirror(&sent, &handle, &mut mirror, &mut seen);
        assert!(
            !mirror.live.keys().any(|k| k.starts_with("node_modules")),
            "live = {:?}",
            mirror.live.keys().collect::<Vec<_>>()
        );
        handle.command(Command::Stop);
    }

    /// docs/design/fs-watch.md "Ignoring": excluded paths are absent from
    /// the mirror rather than filtered out of it, churn under them
    /// produces no update at all, and an edit to an ignore source
    /// re-classifies the tree in both directions.
    #[test]
    fn excluded_paths_never_reach_the_client() {
        let dir = temp_dir().canonicalize().unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join(".git/config"), b"[core]").unwrap();
        fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        fs::write(dir.join("node_modules/pkg/index.js"), b"x").unwrap();
        fs::create_dir_all(dir.join("target/debug")).unwrap();
        fs::write(dir.join("target/debug/bin"), b"x").unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.rs"), b"fn main() {}").unwrap();
        fs::write(dir.join(".gitignore"), "target/\nnode_modules/\n").unwrap();

        let key = test_key_ignoring(
            &dir,
            IgnoreSpec {
                gitignore: true,
                dot_ignore: true,
                exclude_git: true,
                patterns: Vec::new(),
            },
        );
        let (sent, handle, hint) = drive_engine_keyed(key);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("src/a.rs")
        });
        assert_eq!(
            mirror.live.keys().cloned().collect::<Vec<_>>(),
            ["", ".gitignore", "src", "src/a.rs"],
            "the whole checkout, and nothing the exclusions cover"
        );

        // Churn under an excluded path yields nothing; a visible write in
        // the same batch proves the pipeline was live while it did.
        let quiet = count_updates(&sent);
        fs::write(dir.join("target/debug/fresh.bin"), b"y").unwrap();
        fs::write(dir.join(".git/HEAD"), b"ref: refs/heads/main").unwrap();
        hint.send(Hint::Dirty(dir.join("target/debug/fresh.bin")));
        hint.send(Hint::Dirty(dir.join(".git/HEAD")));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(count_updates(&sent), quiet, "excluded churn woke the sync");

        fs::write(dir.join("src/b.rs"), b"pub fn b() {}").unwrap();
        hint.send(Hint::Dirty(dir.join("src/b.rs")));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "src/b.rs", |m| {
            m.live.contains_key("src/b.rs")
        });
        assert!(
            !mirror
                .live
                .keys()
                .any(|k| k.starts_with("target") || k.starts_with(".git/")),
            "live = {:?}",
            mirror.live.keys().collect::<Vec<_>>()
        );

        // A new rule arrives as a DELETE of what it now covers…
        fs::write(dir.join(".gitignore"), "target/\nnode_modules/\nsrc/a.rs\n").unwrap();
        hint.send(Hint::Dirty(dir.join(".gitignore")));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "a.rs gone", |m| {
            !m.live.contains_key("src/a.rs")
        });
        assert!(mirror.live.contains_key("src/b.rs"), "only the rule's path");

        // …and removing it as an UPSERT of what it uncovers.
        fs::write(dir.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        hint.send(Hint::Dirty(dir.join(".gitignore")));
        pump_until(&sent, &handle, &mut mirror, &mut seen, "a.rs back", |m| {
            m.live.contains_key("src/a.rs")
        });
        handle.command(Command::Stop);
    }

    /// An ignore file *above* the root re-classifies the tree when it is
    /// edited. Nothing inside the root could ever hint at it, so the
    /// reconciler watches the directories holding those sources; without
    /// that, a sync of `repo/crates` kept `repo/.gitignore` as it read it
    /// at open, for the life of the sync.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_edit_to_an_ignore_file_above_the_root_reaches_the_client() {
        let top = temp_dir().canonicalize().unwrap();
        fs::create_dir_all(top.join(".git")).unwrap();
        let root = top.join("crates");
        fs::create_dir_all(&root).unwrap();
        fs::write(top.join(".gitignore"), "*.bak\n").unwrap();
        fs::write(root.join("a.rs"), b"x").unwrap();
        fs::write(root.join("old.bak"), b"x").unwrap();

        let key = test_key_ignoring(
            &root,
            IgnoreSpec {
                gitignore: true,
                ..Default::default()
            },
        );
        let shared = open_root(key).expect("arm native watch");
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let handle = start_sync(
            &shared,
            9,
            SyncOptions {
                latency: Duration::from_millis(5),
                ..Default::default()
            },
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("a.rs")
        });
        assert!(!mirror.live.contains_key("old.bak"), "inherited from above");

        // Relax the parent rule: what it hid must come back, with no hint
        // from inside the tree to prompt it.
        fs::write(top.join(".gitignore"), "*.tmp\n").unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "uncovered", |m| {
            m.live.contains_key("old.bak")
        });

        // And tighten it again, this time covering a file that was visible.
        fs::write(top.join(".gitignore"), "*.rs\n").unwrap();
        pump_until(&sent, &handle, &mut mirror, &mut seen, "covered", |m| {
            !m.live.contains_key("a.rs")
        });
        handle.command(Command::Stop);
    }

    /// A `build/` pattern excludes a *symlinked* directory too. This sync
    /// enumerates through such a link (docs/design/fs-watch.md § Links),
    /// unlike git, so treating it as git does — a file, unmatchable by a
    /// directory-only pattern — would leave the one hole through which a
    /// whole excluded subtree still reaches the client.
    #[test]
    fn a_directory_pattern_excludes_a_symlinked_directory_and_its_subtree() {
        let dir = temp_dir().canonicalize().unwrap();
        fs::create_dir_all(dir.join("real/inner")).unwrap();
        fs::write(dir.join("real/inner/heavy.bin"), b"x").unwrap();
        fs::write(dir.join("keep.txt"), b"k").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("build")).unwrap();

        let key = test_key_ignoring(
            &dir,
            IgnoreSpec {
                patterns: vec!["build/".into()],
                ..Default::default()
            },
        );
        let (sent, handle, _hint) = drive_engine_keyed(key);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("keep.txt")
        });
        assert!(
            !mirror.live.keys().any(|k| k.starts_with("build")),
            "the link and everything enumerated through it; live = {:?}",
            mirror.live.keys().collect::<Vec<_>>()
        );
        // The real path is untouched by the pattern — only the alias matched.
        assert!(mirror.live.contains_key("real/inner/heavy.bin"));
        handle.command(Command::Stop);
    }

    /// An excluded path is absent, not marked, so a client cannot tell an
    /// empty directory from a filtered one. `FS_ENTRY_FILTERED` on the
    /// *parent* is that signal — what lets a file tree say "some items
    /// hidden" — and it tracks the directory's real state as rules and
    /// contents change.
    #[test]
    fn a_directory_reports_that_it_hid_children() {
        let dir = temp_dir().canonicalize().unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::create_dir_all(dir.join("plain")).unwrap();
        fs::write(dir.join("src/a.rs"), b"x").unwrap();
        fs::write(dir.join("src/a.tmp"), b"x").unwrap();
        fs::write(dir.join("plain/b.rs"), b"x").unwrap();

        let key = test_key_ignoring(
            &dir,
            IgnoreSpec {
                patterns: vec!["*.tmp".into()],
                ..Default::default()
            },
        );
        let (sent, handle, hint) = drive_engine_keyed(key);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("plain/b.rs")
        });
        let filtered = |m: &FsMirror, path: &str| {
            m.live
                .get(path)
                .is_some_and(|n| n.entry_flags & FS_ENTRY_FILTERED != 0)
        };
        assert!(filtered(&mirror, "src"), "src hid a.tmp");
        assert!(!filtered(&mirror, "plain"), "plain hid nothing");
        assert!(!filtered(&mirror, ""), "nor did the root");

        // A newly excluded child sets it on a directory that had none.
        fs::write(dir.join("plain/c.tmp"), b"x").unwrap();
        hint.send(Hint::Dirty(dir.join("plain/c.tmp")));
        pump_until_nudging(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "plain hides",
            || {
                hint.send(Hint::Dirty(dir.join("plain/c.tmp")));
            },
            |m| {
                m.live
                    .get("plain")
                    .is_some_and(|n| n.entry_flags & FS_ENTRY_FILTERED != 0)
            },
        );

        // …and removing the last one clears it again, on the next listing
        // of that directory.
        fs::remove_file(dir.join("plain/c.tmp")).unwrap();
        hint.send(Hint::Dirty(dir.join("plain")));
        pump_until_nudging(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "plain clears",
            || {
                hint.send(Hint::Dirty(dir.join("plain")));
            },
            |m| {
                m.live
                    .get("plain")
                    .is_some_and(|n| n.entry_flags & FS_ENTRY_FILTERED == 0)
            },
        );
        assert!(filtered(&mirror, "src"), "src still hides a.tmp");
        handle.command(Command::Stop);
    }

    /// Client patterns outrank the ignore files (`!keep.log` re-includes
    /// what `*.log` hid), and the exclusion set is part of the shared
    /// root's identity: syncs excluding different things index different
    /// trees and cannot share one reconciler.
    #[test]
    fn client_patterns_outrank_ignore_files_and_key_the_root() {
        let dir = temp_dir().canonicalize().unwrap();
        fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        fs::write(dir.join("a.log"), b"x").unwrap();
        fs::write(dir.join("keep.log"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();

        let spec = IgnoreSpec {
            gitignore: true,
            dot_ignore: true,
            exclude_git: false,
            patterns: IgnoreSpec::parse_patterns("!keep.log\nnotes.txt"),
        };
        let (sent, handle, _hint) = drive_engine_keyed(test_key_ignoring(&dir, spec.clone()));
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("keep.log")
        });
        assert_eq!(
            mirror.live.keys().cloned().collect::<Vec<_>>(),
            ["", ".gitignore", "keep.log"]
        );

        let same = open_root_unwatched(test_key_ignoring(&dir, spec.clone()));
        let again = open_root_unwatched(test_key_ignoring(&dir, spec));
        assert!(Arc::ptr_eq(&same, &again), "one spec, one shared root");
        let unfiltered = open_root_unwatched(test_key(&dir));
        assert!(
            !Arc::ptr_eq(&same, &unfiltered),
            "an unfiltered sync indexes a different tree"
        );
        handle.command(Command::Stop);
    }

    /// A root whose name contains `%` survives the FS_SYNCED round trip. The
    /// echo is `escape_path(canonical_root)`, so a literal `%` comes back as
    /// `%25`; clients build further sync roots from that echo, and without a
    /// decode on the way in such a path could be listed but never re-opened.
    /// A file genuinely named `50%25.txt` still takes precedence over the
    /// decoded reading of `50%.txt`.
    #[test]
    fn percent_in_root_survives_the_wire_round_trip() {
        let dir = temp_dir().canonicalize().unwrap();
        let literal = dir.join("50%.txt");
        fs::write(&literal, b"x").unwrap();

        // What the server echoes for this path.
        let echoed = escape_path(&literal);
        assert!(echoed.ends_with("50%25.txt"), "echo escapes the percent");

        // Both the raw form a CLI types and the escaped echo must resolve.
        assert_eq!(
            validate_root(&literal.to_string_lossy()).unwrap(),
            literal.canonicalize().unwrap(),
            "a raw path containing % still works"
        );
        assert_eq!(
            validate_root(&echoed).unwrap(),
            literal.canonicalize().unwrap(),
            "the escaped echo resolves back to the same file"
        );

        // A file actually named `50%25.txt` wins the literal reading.
        let ambiguous = dir.join("50%25.txt");
        fs::write(&ambiguous, b"y").unwrap();
        assert_eq!(
            validate_root(&echoed).unwrap(),
            ambiguous.canonicalize().unwrap(),
            "literal match takes precedence over the decoded one"
        );
    }

    /// A symlinked directory is enumerated like a real one, flagged
    /// `FS_ENTRY_LINK_DIR` so a client knows it is expandable, and a link that
    /// points back into its own tree stops instead of recursing forever.
    /// Previously the scan reported the link as a childless entry, which made
    /// the file browser a dead end at every symlinked directory.
    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_traversed_and_cycle_safe() {
        let dir = temp_dir().canonicalize().unwrap();
        // real/inner/deep.txt, plus link -> real, and a cycle real/loop -> real
        fs::create_dir_all(dir.join("real/inner")).unwrap();
        fs::write(dir.join("real/inner/deep.txt"), b"payload").unwrap();
        fs::write(dir.join("real/top.txt"), b"top").unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("real/loop")).unwrap();
        // A link to a file, and a dangling one: neither is enumerable.
        std::os::unix::fs::symlink(dir.join("real/top.txt"), dir.join("tolink")).unwrap();
        std::os::unix::fs::symlink(dir.join("nope"), dir.join("dangling")).unwrap();

        let (sent, handle, _hint) = drive_engine(&dir);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "link subtree",
            |m| m.live.contains_key("link/inner/deep.txt"),
        );

        // The link is reported as a symlink, but flagged as enumerable.
        let link = &mirror.live["link"];
        assert_eq!(link.entry_flags & FS_ENTRY_TYPE_MASK, FS_ENTRY_SYMLINK);
        assert_ne!(
            link.entry_flags & FS_ENTRY_LINK_DIR,
            0,
            "a symlinked directory must advertise that it can be expanded"
        );
        // Its contents are reachable through the link's own path.
        assert!(mirror.live.contains_key("link/top.txt"));
        assert_eq!(
            mirror.live["link/inner/deep.txt"].content.as_deref(),
            Some(&b"payload"[..])
        );

        // A link to a file is not enumerable, and neither is a dangling one.
        assert_eq!(mirror.live["tolink"].entry_flags & FS_ENTRY_LINK_DIR, 0);
        assert_eq!(mirror.live["dangling"].entry_flags & FS_ENTRY_LINK_DIR, 0);

        // `real/loop` points at its own ancestor, so it is reported but never
        // descended — no redundant copy of the subtree, no recursion.
        assert!(mirror.live.contains_key("real/loop"));
        assert!(
            !mirror.live.keys().any(|k| k.starts_with("real/loop/")),
            "a link to an ancestor must not be descended: {:?}",
            mirror.live.keys().collect::<Vec<_>>()
        );
        // The same link reached through `link` is equally bounded.
        assert!(mirror.live.contains_key("link/loop"));
        assert!(
            !mirror.live.keys().any(|k| k.starts_with("link/loop/")),
            "cycle detection must hold through a symlinked path too: {:?}",
            mirror.live.keys().collect::<Vec<_>>()
        );
        handle.command(Command::Stop);
    }

    /// error, files canonicalize, missing paths keep their status.
    #[test]
    fn single_root_validation() {
        use blit_remote::fs::{FS_STATUS_NOT_FOUND, FS_STATUS_OTHER};
        let dir = temp_dir();
        let file = dir.join("f.txt");
        fs::write(&file, b"x").unwrap();
        assert_eq!(
            validate_single_root(&file.to_string_lossy()).unwrap(),
            file.canonicalize().unwrap()
        );
        let (status, _) = validate_single_root(&dir.to_string_lossy()).unwrap_err();
        assert_eq!(status, FS_STATUS_OTHER, "directory root refused");
        let (status, _) =
            validate_single_root(&dir.join("missing.txt").to_string_lossy()).unwrap_err();
        assert_eq!(status, FS_STATUS_NOT_FOUND);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Copy `from`'s mtime onto `to`, so a rewrite can be made
    /// indistinguishable by stat — what a coarse filesystem clock does on
    /// its own when two writes land in the same granule.
    fn copy_mtime(from: &Path, to: &Path) {
        let status = std::process::Command::new("touch")
            .arg("-r")
            .arg(from)
            .arg(to)
            .status()
            .expect("touch");
        assert!(status.success(), "touch -r failed");
        assert_eq!(
            stat_meta(from).unwrap().mtime_ns,
            stat_meta(to).unwrap().mtime_ns,
            "mtimes must be identical for the test to mean anything"
        );
    }

    /// A same-size rewrite inside one filesystem timestamp granule
    /// (docs/design/fs-watch.md "Racily-clean entries"): size, identity and
    /// mtime are all unchanged, so only content distinguishes the two
    /// versions and the new bytes must still reach the client.
    ///
    /// Ubuntu's coarse inode clock produces exactly this from two ordinary
    /// `write`s a millisecond apart; `touch -r` reproduces it everywhere.
    #[test]
    fn single_sync_same_stat_rewrite() {
        let dir = temp_dir().canonicalize().unwrap();
        let reference = dir.join("reference");
        let file = dir.join("note.txt");
        fs::write(&reference, b"").unwrap();
        fs::write(&file, b"one").unwrap();
        copy_mtime(&reference, &file);

        let (sent, handle, hint) = drive_single_engine(&file);
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"one"[..]))
        });

        fs::write(&file, b"two").unwrap();
        copy_mtime(&reference, &file);
        hint.send(Hint::Dirty(file.clone()));
        pump_until(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "same-stat rewrite",
            |m| {
                m.live
                    .get("")
                    .is_some_and(|n| n.content.as_deref() == Some(&b"two"[..]))
            },
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Writes through a SINGLE sync address the empty path: CAS write-
    /// through works, non-"" paths are INVALID, create-exclusive on the
    /// existing root conflicts, and a conditional REMOVE of "" lands.
    #[test]
    fn single_sync_write_through() {
        let dir = temp_dir().canonicalize().unwrap();
        let file = dir.join("doc.txt");
        fs::write(&file, b"hello").unwrap();
        let (sent, handle, _hint) = drive_single_engine(&file);

        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("")
        });
        let base = mirror.live[""].hash;
        assert_eq!(base, blake3_128(b"hello"));

        // CAS write-through at "".
        let (s, h, _) = await_done(&handle, &sent, 1, write_req(1, "", base, 0, b"world"));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(h, blake3_128(b"world"));
        assert_eq!(fs::read(&file).unwrap(), b"world");
        // The echo re-enters the writer's own mirror (metadata-only, hash
        // updated — self-echo suppression keeps the bytes it wrote).
        pump_until(&sent, &handle, &mut mirror, &mut seen, "echo", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.hash == blake3_128(b"world"))
        });

        // A stale base conflicts, carrying the live hash.
        let (s, disk, _) = await_done(&handle, &sent, 2, write_req(2, "", base, 0, b"x"));
        assert_eq!(s, FS_DONE_CONFLICT);
        assert_eq!(disk, blake3_128(b"world"));

        // Non-"" paths do not exist in a SINGLE sync's namespace.
        let (s, _, _) = await_done(&handle, &sent, 3, write_req(3, "other.txt", 0, 0, b"no"));
        assert_eq!(s, FS_DONE_INVALID);

        // Create-exclusive on the existing root conflicts.
        let (s, _, _) = await_done(&handle, &sent, 4, write_req(4, "", 0, 0, b"no"));
        assert_eq!(s, FS_DONE_CONFLICT);

        // Conditional REMOVE of "" deletes the file; the mirror empties.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            5,
            Command::Op(OpReq {
                nonce: 5,
                op: FS_OP_REMOVE,
                a: String::new(),
                b: String::new(),
                base: blake3_128(b"world"),
                mode: 0,
                flags: 0,
                inflight: None,
            }),
        );
        assert_eq!(s, FS_DONE_OK);
        assert!(!file.exists());
        pump_until(&sent, &handle, &mut mirror, &mut seen, "removed", |m| {
            m.live.is_empty()
        });
        assert_eq!(count_closed(&sent), 0, "REMOVE of '' must not close");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&dir);
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

    // ── Chunked uploads (docs/protocol.md "Filesystem sync") ──────────────

    /// Poll the sent log for the `n`th (0-based) message with `opcode`.
    fn await_opcode(sent: &Arc<Mutex<Vec<Vec<u8>>>>, opcode: u8, n: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(m) = sent
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m[0] == opcode)
                .nth(n)
                .cloned()
            {
                return m;
            }
            assert!(
                Instant::now() < deadline,
                "no reply #{n} with opcode {opcode:#x}"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn upload_begin(
        nonce: u16,
        upload_id: u16,
        path: &str,
        flags: u8,
        base: u128,
        size: u64,
    ) -> Command {
        Command::UploadBegin(UploadBeginReq {
            nonce,
            upload_id,
            path: path.into(),
            flags,
            base,
            mode: 0,
            size,
            inflight: None,
        })
    }

    fn upload_chunk(upload_id: u16, offset: u64, data: &[u8]) -> Command {
        Command::UploadChunk {
            upload_id,
            offset,
            data: data.to_vec(),
        }
    }

    fn upload_finish(nonce: u16, upload_id: u16) -> Command {
        Command::UploadFinish {
            nonce,
            upload_id,
            inflight: None,
        }
    }

    /// Any staging file left in `root`?
    fn temp_files_in(root: &Path) -> Vec<String> {
        fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(TEMP_FILE_PREFIX))
            .collect()
    }

    #[test]
    fn upload_happy_path_lands_exact_bytes() {
        use blit_remote::fs::{
            S2C_FS_UPLOAD_BEGIN, S2C_FS_UPLOAD_CHUNK, S2C_FS_UPLOAD_FINISH,
            parse_fs_upload_begin_result, parse_fs_upload_chunk_result,
            parse_fs_upload_finish_result,
        };
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);
        let content: Vec<u8> = (0..100_000u32).flat_map(|i| i.to_le_bytes()).collect();

        handle.command(upload_begin(1, 0, "big.bin", 0, 0, content.len() as u64));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_BEGIN, 0);
        assert_eq!(
            parse_fs_upload_begin_result(&m),
            Some((1, FS_DONE_OK, 0, 0, 0))
        );

        let half = content.len() / 2;
        handle.command(upload_chunk(0, 0, &content[..half]));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_CHUNK, 0);
        assert_eq!(
            parse_fs_upload_chunk_result(&m),
            Some((0, FS_DONE_OK, half as u64))
        );
        handle.command(upload_chunk(0, half as u64, &content[half..]));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_CHUNK, 1);
        assert_eq!(
            parse_fs_upload_chunk_result(&m),
            Some((0, FS_DONE_OK, content.len() as u64))
        );

        handle.command(upload_finish(2, 0));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_FINISH, 0);
        let (nonce, status, hash, mtime_ns) = parse_fs_upload_finish_result(&m).unwrap();
        assert_eq!((nonce, status), (2, FS_DONE_OK));
        assert_eq!(hash, blake3_128(&content));
        assert_ne!(mtime_ns, 0);
        assert_eq!(fs::read(root.join("big.bin")).unwrap(), content);
        assert_eq!(temp_files_in(&root), Vec::<String>::new());

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_offset_mismatch_reports_resume_point() {
        use blit_remote::fs::{
            FS_DONE_OFFSET_MISMATCH, S2C_FS_UPLOAD_CHUNK, parse_fs_upload_chunk_result,
        };
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        handle.command(upload_begin(1, 0, "a.txt", 0, 0, 11));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);

        // A gap is refused; the ack says where to resume.
        handle.command(upload_chunk(0, 4, b"ello"));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_CHUNK, 0);
        assert_eq!(
            parse_fs_upload_chunk_result(&m),
            Some((0, FS_DONE_OFFSET_MISMATCH, 0))
        );
        // A replayed (overlapping) offset is refused the same way.
        handle.command(upload_chunk(0, 0, b"hello "));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_CHUNK, 1);
        assert_eq!(parse_fs_upload_chunk_result(&m), Some((0, FS_DONE_OK, 6)));
        handle.command(upload_chunk(0, 0, b"hello "));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_CHUNK, 2);
        assert_eq!(
            parse_fs_upload_chunk_result(&m),
            Some((0, FS_DONE_OFFSET_MISMATCH, 6))
        );
        handle.command(upload_chunk(0, 6, b"world"));
        let m = await_opcode(&sent, S2C_FS_UPLOAD_CHUNK, 3);
        assert_eq!(parse_fs_upload_chunk_result(&m), Some((0, FS_DONE_OK, 11)));

        handle.command(upload_finish(2, 0));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, _, _)| (n, s)),
            Some((2, FS_DONE_OK))
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"hello world");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_finish_size_mismatch_drops_upload() {
        use blit_remote::fs::{FS_DONE_SIZE_MISMATCH, FS_DONE_UNKNOWN_UPLOAD};
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        handle.command(upload_begin(1, 0, "a.txt", 0, 0, 10));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        handle.command(upload_chunk(0, 0, b"12345"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);

        handle.command(upload_finish(2, 0));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, _, _)| (n, s)),
            Some((2, FS_DONE_SIZE_MISMATCH))
        );
        // A failed FINISH terminates the upload: the temp is gone, the
        // target never appeared, and the id no longer routes.
        assert!(!root.join("a.txt").exists());
        assert_eq!(temp_files_in(&root), Vec::<String>::new());
        handle.command(upload_chunk(0, 5, b"67890"));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 1);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_chunk_result(&m),
            Some((0, FS_DONE_UNKNOWN_UPLOAD, 0))
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_cancel_removes_temp() {
        use blit_remote::fs::FS_DONE_UNKNOWN_UPLOAD;
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        handle.command(upload_begin(1, 0, "a.txt", 0, 0, 10));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        handle.command(upload_chunk(0, 0, b"12345"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        assert_eq!(temp_files_in(&root).len(), 1, "staging file exists");

        handle.command(Command::UploadCancel { upload_id: 0 });
        // No reply is defined for cancel; the next chunk proves the upload
        // is gone, and the temp file must be too.
        handle.command(upload_chunk(0, 5, b"67890"));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 1);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_chunk_result(&m),
            Some((0, FS_DONE_UNKNOWN_UPLOAD, 0))
        );
        assert_eq!(temp_files_in(&root), Vec::<String>::new());
        assert!(!root.join("a.txt").exists());

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_overwrites_existing_file() {
        let root = temp_dir().canonicalize().unwrap();
        fs::write(root.join("a.txt"), b"old contents").unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        handle.command(upload_begin(1, 0, "a.txt", FS_UPLOAD_NO_CAS, 0, 3));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        handle.command(upload_chunk(0, 0, b"new"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        handle.command(upload_finish(2, 0));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, h, _)| (n, s, h)),
            Some((2, FS_DONE_OK, blake3_128(b"new")))
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_mkparents_creates_directories() {
        use blit_remote::fs::{FS_DONE_NOT_FOUND, FS_UPLOAD_MKPARENTS};
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        // Missing parents without MKPARENTS: refused at begin.
        handle.command(upload_begin(1, 0, "d/e/f.txt", 0, 0, 4));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_begin_result(&m),
            Some((1, FS_DONE_NOT_FOUND, 0, 0, 0))
        );

        handle.command(upload_begin(2, 0, "d/e/f.txt", FS_UPLOAD_MKPARENTS, 0, 4));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 1);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_begin_result(&m),
            Some((2, FS_DONE_OK, 0, 0, 0))
        );
        handle.command(upload_chunk(0, 0, b"deep"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        handle.command(upload_finish(3, 0));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, _, _)| (n, s)),
            Some((3, FS_DONE_OK))
        );
        assert_eq!(fs::read(root.join("d/e/f.txt")).unwrap(), b"deep");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn upload_unknown_id_size_cap_and_flags() {
        use blit_remote::fs::{FS_DONE_TOO_LARGE, FS_DONE_UNKNOWN_UPLOAD};
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        // Chunk/finish on an id no BEGIN opened.
        handle.command(upload_chunk(7, 0, b"x"));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_chunk_result(&m),
            Some((7, FS_DONE_UNKNOWN_UPLOAD, 0))
        );
        handle.command(upload_finish(9, 7));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, _, _)| (n, s)),
            Some((9, FS_DONE_UNKNOWN_UPLOAD))
        );

        // Over the BLIT_FS_UPLOAD_MAX default (1 GiB): refused at begin.
        handle.command(upload_begin(1, 0, "huge.bin", 0, 0, 2 * 1024 * 1024 * 1024));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_begin_result(&m),
            Some((1, FS_DONE_TOO_LARGE, 0, 0, 0))
        );

        // Unknown flags are INVALID.
        handle.command(upload_begin(2, 0, "a.txt", 0x80, 0, 1));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 1);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_begin_result(&m),
            Some((2, FS_DONE_INVALID, 0, 0, 0))
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// An upload's staging file sits in the watched directory for the whole
    /// transfer; the reconciler's name filter keeps it out of the mirror
    /// (TEMP_FILE_PREFIX), so clients never see the temp appear, grow, and
    /// move onto the target.
    #[test]
    fn upload_temp_file_is_never_mirrored() {
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, hint) = drive_engine(&root);
        let mut mirror = FsMirror::new();
        let mut seen = 0;
        let no_temp = |m: &FsMirror| !m.live.keys().any(|k| k.contains(TEMP_FILE_PREFIX));

        handle.command(upload_begin(1, 0, "a.txt", 0, 0, 11));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        handle.command(upload_chunk(0, 0, b"hello "));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        assert_eq!(temp_files_in(&root).len(), 1);
        // Re-list the root while the temp exists. Give the reconciler many
        // settle windows; the temp must never surface.
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            hint.send(Hint::Dirty(root.clone()));
            pump_mirror(&sent, &handle, &mut mirror, &mut seen);
            assert!(
                no_temp(&mirror),
                "temp file mirrored: {:?}",
                mirror.live.keys()
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        handle.command(upload_chunk(0, 6, b"world"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 1);
        handle.command(upload_finish(2, 0));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        pump_until(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "uploaded file",
            |m| m.live.contains_key("a.txt"),
        );
        assert!(no_temp(&mirror));

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// A SINGLE sync's one addressable path is "" — uploads land on the
    /// root file itself, exactly as writes do.
    #[test]
    fn upload_into_single_file_sync() {
        let dir = temp_dir().canonicalize().unwrap();
        let file = dir.join("note.txt");
        fs::write(&file, b"v1").unwrap();
        let (sent, handle, _hint) = drive_single_engine(&file);

        handle.command(upload_begin(1, 0, "other.txt", 0, 0, 2));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_begin_result(&m),
            Some((1, FS_DONE_INVALID, 0, 0, 0))
        );

        handle.command(upload_begin(2, 0, "", FS_UPLOAD_NO_CAS, 0, 2));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 1);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_begin_result(&m),
            Some((2, FS_DONE_OK, 0, 0, 0))
        );
        handle.command(upload_chunk(0, 0, b"v2"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        handle.command(upload_finish(3, 0));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, _, _)| (n, s)),
            Some((3, FS_DONE_OK))
        );
        assert_eq!(fs::read(&file).unwrap(), b"v2");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The FS_WRITE precondition, checked at BEGIN: create-exclusive fails
    /// on an existing target with CONFLICT + the live hash, CAS fails on a
    /// stale base the same way and passes on the live one, a missing target
    /// fails CAS with the "absent" zero hash, and NO_CAS overwrites
    /// unconditionally.
    #[test]
    fn upload_precondition_at_begin() {
        use blit_remote::fs::{FS_DONE_CONFLICT, FS_UPLOAD_NO_CAS};
        let root = temp_dir().canonicalize().unwrap();
        fs::write(root.join("a.txt"), b"old").unwrap();
        let old_hash = blake3_128(b"old");
        let (sent, handle, _hint) = drive_engine(&root);
        let begin_result = |n: usize| {
            blit_remote::fs::parse_fs_upload_begin_result(&await_opcode(
                &sent,
                blit_remote::fs::S2C_FS_UPLOAD_BEGIN,
                n,
            ))
        };

        // Create-exclusive on an existing file: CONFLICT with the live hash.
        handle.command(upload_begin(1, 0, "a.txt", 0, 0, 3));
        assert_eq!(begin_result(0), Some((1, FS_DONE_CONFLICT, 0, old_hash, 0)));
        // CAS with a stale base: same answer.
        handle.command(upload_begin(2, 0, "a.txt", 0, 0xdead, 3));
        assert_eq!(begin_result(1), Some((2, FS_DONE_CONFLICT, 0, old_hash, 0)));
        // CAS on a missing target: CONFLICT with the "absent" zero hash,
        // exactly as FS_WRITE (not NOT_FOUND).
        handle.command(upload_begin(3, 0, "gone.txt", 0, 0xdead, 1));
        assert_eq!(begin_result(2), Some((3, FS_DONE_CONFLICT, 0, 0, 0)));
        // Create-exclusive on a missing target: staged.
        handle.command(upload_begin(4, 0, "new.txt", 0, 0, 1));
        assert_eq!(begin_result(3), Some((4, FS_DONE_OK, 0, 0, 0)));
        handle.command(upload_chunk(0, 0, b"x"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        handle.command(upload_finish(5, 0));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"x");

        // CAS with the live base lands.
        handle.command(upload_begin(6, 0, "a.txt", 0, old_hash, 3));
        assert_eq!(begin_result(4), Some((6, FS_DONE_OK, 0, 0, 0)));
        handle.command(upload_chunk(0, 0, b"new"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 1);
        handle.command(upload_finish(7, 0));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 1);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new");

        // NO_CAS overwrites unconditionally, base ignored.
        handle.command(upload_begin(8, 0, "a.txt", FS_UPLOAD_NO_CAS, 0xdead, 5));
        assert_eq!(begin_result(5), Some((8, FS_DONE_OK, 0, 0, 0)));
        handle.command(upload_chunk(0, 0, b"force"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 2);
        handle.command(upload_finish(9, 0));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 2);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"force");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// The precondition is re-verified at FINISH under the write lock: a
    /// file changed (or created) by someone else between BEGIN and FINISH
    /// turns the landing into CONFLICT with the now-current hash, and the
    /// uploaded bytes are dropped with the temp.
    #[test]
    fn upload_finish_reverifies_precondition() {
        use blit_remote::fs::FS_DONE_CONFLICT;
        let root = temp_dir().canonicalize().unwrap();
        fs::write(root.join("a.txt"), b"v1").unwrap();
        let (sent, handle, _hint) = drive_engine(&root);
        let finish_result = |n: usize| {
            blit_remote::fs::parse_fs_upload_finish_result(&await_opcode(
                &sent,
                blit_remote::fs::S2C_FS_UPLOAD_FINISH,
                n,
            ))
        };

        // CAS upload begins clean...
        handle.command(upload_begin(1, 0, "a.txt", 0, blake3_128(b"v1"), 2));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 0);
        handle.command(upload_chunk(0, 0, b"v2"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        // ...then an external writer moves the target mid-upload.
        fs::write(root.join("a.txt"), b"changed").unwrap();
        handle.command(upload_finish(2, 0));
        assert_eq!(
            finish_result(0),
            Some((2, FS_DONE_CONFLICT, blake3_128(b"changed"), 0))
        );
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"changed");
        assert_eq!(temp_files_in(&root), Vec::<String>::new());

        // Create-exclusive: the target appears between BEGIN and FINISH.
        handle.command(upload_begin(3, 0, "fresh.txt", 0, 0, 1));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_BEGIN, 1);
        handle.command(upload_chunk(0, 0, b"y"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 1);
        fs::write(root.join("fresh.txt"), b"sneak").unwrap();
        handle.command(upload_finish(4, 0));
        assert_eq!(
            finish_result(1),
            Some((4, FS_DONE_CONFLICT, blake3_128(b"sneak"), 0))
        );
        assert_eq!(fs::read(root.join("fresh.txt")).unwrap(), b"sneak");

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// FOLLOW_SYMLINK writes through a final-component symlink whose target
    /// stays under the root (same meaning as FS_WRITE's); the default
    /// refuses one, and a link escaping the root is refused even with it.
    #[test]
    fn upload_follow_symlink() {
        use blit_remote::fs::{FS_DONE_PERMISSION, FS_UPLOAD_FOLLOW_SYMLINK};
        let root = temp_dir().canonicalize().unwrap();
        fs::write(root.join("real.txt"), b"v1").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link")).unwrap();
        let outside = temp_dir().canonicalize().unwrap();
        fs::write(outside.join("out.txt"), b"out").unwrap();
        std::os::unix::fs::symlink(outside.join("out.txt"), root.join("esc")).unwrap();
        let (sent, handle, _hint) = drive_engine(&root);
        let begin_result = |n: usize| {
            blit_remote::fs::parse_fs_upload_begin_result(&await_opcode(
                &sent,
                blit_remote::fs::S2C_FS_UPLOAD_BEGIN,
                n,
            ))
        };

        // Default: a final-component symlink is refused.
        handle.command(upload_begin(1, 0, "link", 0, blake3_128(b"v1"), 2));
        assert_eq!(begin_result(0), Some((1, FS_DONE_PERMISSION, 0, 0, 0)));
        // FOLLOW_SYMLINK: CAS applies to the resolved target's content and
        // the bytes land on it; the link itself survives.
        handle.command(upload_begin(
            2,
            0,
            "link",
            FS_UPLOAD_FOLLOW_SYMLINK,
            blake3_128(b"v1"),
            2,
        ));
        assert_eq!(begin_result(1), Some((2, FS_DONE_OK, 0, 0, 0)));
        handle.command(upload_chunk(0, 0, b"v2"));
        await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_CHUNK, 0);
        handle.command(upload_finish(3, 0));
        let m = await_opcode(&sent, blit_remote::fs::S2C_FS_UPLOAD_FINISH, 0);
        assert_eq!(
            blit_remote::fs::parse_fs_upload_finish_result(&m).map(|(n, s, _, _)| (n, s)),
            Some((3, FS_DONE_OK))
        );
        assert_eq!(fs::read(root.join("real.txt")).unwrap(), b"v2");
        assert!(
            fs::symlink_metadata(root.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        // A link resolving outside the root is refused even with FOLLOW.
        handle.command(upload_begin(4, 0, "esc", FS_UPLOAD_FOLLOW_SYMLINK, 0, 1));
        assert_eq!(begin_result(2), Some((4, FS_DONE_PERMISSION, 0, 0, 0)));

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    fn delta_req(nonce: u16, path: &str, base: u128, flags: u8, ops: &[u8]) -> Command {
        Command::Write(WriteReq {
            nonce,
            path: path.into(),
            base,
            mode: 0,
            flags,
            content_kind: blit_remote::fs::FS_WRITE_CONTENT_DELTA,
            content: ops.to_vec(),
            inflight: None,
        })
    }

    /// C2S delta writes (docs/design/fs-write.md content_kind 2): the ops
    /// apply against the exact bytes `base` names; a stale base answers
    /// CONFLICT with the live hash and never a corrupted apply; a delta
    /// without a CAS anchor (NO_CAS or zero base) and a malformed stream
    /// are INVALID.
    #[test]
    fn write_delta_applies_against_cas_base() {
        let root = temp_dir().canonicalize().unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        // Seed via a full write.
        let old = b"hello world".as_slice();
        let (s, h1, _) = await_done(&handle, &sent, 1, write_req(1, "a.txt", 0, 0, old));
        assert_eq!(s, FS_DONE_OK);

        // Full-file delta round-trip through a sync write.
        let new = b"hello brave world".as_slice();
        let ops = encode_delta(old, new);
        assert_eq!(
            blit_remote::fs::apply_fs_delta(old, &ops).as_deref(),
            Some(new)
        );
        let (s, h2, _) = await_done(&handle, &sent, 2, delta_req(2, "a.txt", h1, 0, &ops));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(h2, blake3_128(new));
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), new);

        // A stale base rejects with the live hash; the file is untouched.
        let (s, disk, _) = await_done(&handle, &sent, 3, delta_req(3, "a.txt", h1, 0, &ops));
        assert_eq!(s, FS_DONE_CONFLICT, "stale delta base must conflict");
        assert_eq!(disk, h2, "conflict carries the live disk hash");
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), new);

        // No CAS anchor: NO_CAS and the zero (absent) base are INVALID.
        let (s, _, _) = await_done(
            &handle,
            &sent,
            4,
            delta_req(4, "a.txt", h2, FS_WRITE_NO_CAS, &ops),
        );
        assert_eq!(s, FS_DONE_INVALID);
        let (s, _, _) = await_done(&handle, &sent, 5, delta_req(5, "a.txt", 0, 0, &ops));
        assert_eq!(s, FS_DONE_INVALID);

        // A malformed instruction stream is INVALID and writes nothing.
        let (s, _, _) = await_done(&handle, &sent, 6, delta_req(6, "a.txt", h2, 0, &[0xFF, 1]));
        assert_eq!(s, FS_DONE_INVALID);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), new);

        // A delta against a missing file conflicts with the absent (zero)
        // sentinel — the base cannot be produced.
        let (s, disk, _) = await_done(&handle, &sent, 7, delta_req(7, "gone.txt", h2, 0, &ops));
        assert_eq!(s, FS_DONE_CONFLICT);
        assert_eq!(disk, 0);

        // The writer's mirror converges on the applied bytes' hash.
        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "delta echo", |m| {
            m.live.get("a.txt").is_some_and(|n| n.hash == h2)
        });

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&root);
    }

    /// A delta write through a SINGLE sync addresses "" like any other
    /// write, and chains off the returned hash.
    #[test]
    fn write_delta_on_single_sync() {
        let dir = temp_dir().canonicalize().unwrap();
        let file = dir.join("buf.txt");
        fs::write(&file, b"alpha").unwrap();
        let (sent, handle, _hint) = drive_single_engine(&file);

        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live.contains_key("")
        });
        let h0 = mirror.live[""].hash;

        let ops = encode_delta(b"alpha", b"alpha beta");
        let (s, h1, _) = await_done(&handle, &sent, 1, delta_req(1, "", h0, 0, &ops));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(h1, blake3_128(b"alpha beta"));
        assert_eq!(fs::read(&file).unwrap(), b"alpha beta");

        // Chain a second delta off the *returned* hash (the fs-write.md
        // rapid-save rule), while the echo is still in flight.
        let ops2 = encode_delta(b"alpha beta", b"alpha beta gamma");
        let (s, h2, _) = await_done(&handle, &sent, 2, delta_req(2, "", h1, 0, &ops2));
        assert_eq!(s, FS_DONE_OK);
        assert_eq!(h2, blake3_128(b"alpha beta gamma"));
        assert_eq!(fs::read(&file).unwrap(), b"alpha beta gamma");

        // Non-"" delta targets are INVALID on a SINGLE sync.
        let (s, _, _) = await_done(&handle, &sent, 3, delta_req(3, "x.txt", h2, 0, &ops2));
        assert_eq!(s, FS_DONE_INVALID);

        pump_until(&sent, &handle, &mut mirror, &mut seen, "echo", |m| {
            m.live.get("").is_some_and(|n| n.hash == h2)
        });

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&dir);
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

    /// FS_FETCH must confine exactly like the write path: an in-tree symlink
    /// whose target escapes the root cannot be used to read a file outside
    /// it (resolve_wire_path alone does no symlink resolution).
    #[cfg(unix)]
    #[test]
    fn fetch_refuses_symlink_escape() {
        let root = temp_dir().canonicalize().unwrap();
        // A secret outside the root, plus an in-tree symlink pointing at the
        // root's parent so `pub/<name>` resolves out of the confinement.
        let secret = root.parent().unwrap().join("blit-fetch-secret.txt");
        fs::write(&secret, b"top secret").unwrap();
        std::os::unix::fs::symlink(root.parent().unwrap(), root.join("pub")).unwrap();
        let (sent, handle, _hint) = drive_engine(&root);

        let await_file = |nonce: u16| -> (u8, Vec<u8>) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                for msg in sent.lock().unwrap().iter() {
                    if msg[0] == blit_remote::fs::S2C_FS_FILE
                        && let Some((n, status, data)) = blit_remote::fs::parse_fs_file(msg)
                        && n == nonce
                    {
                        return (status, data.to_vec());
                    }
                }
                assert!(Instant::now() < deadline, "no FS_FILE for nonce {nonce}");
                std::thread::sleep(Duration::from_millis(2));
            }
        };

        handle.command(Command::Fetch {
            nonce: 1,
            path: "pub/blit-fetch-secret.txt".into(),
        });
        let (status, data) = await_file(1);
        assert_ne!(status, FS_FILE_OK, "escape must be refused");
        assert!(data.is_empty(), "no bytes leak past the confinement");

        handle.command(Command::Stop);
        let _ = fs::remove_file(&secret);
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

    /// Mass deletes prune to the shallowest removed ancestors — including
    /// the sort-order trap where "a!x" and "ab" interleave with "a"'s
    /// subtree ('!' < '/' < 'b').
    #[test]
    fn diff_mass_delete_prunes_to_ancestors() {
        let mut prev = Index::new();
        prev.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        prev.insert("a".into(), meta(FS_ENTRY_DIR, 0, 0, 2));
        prev.insert("a!x".into(), meta(FS_ENTRY_FILE, 1, 1, 3));
        prev.insert("a/b".into(), meta(FS_ENTRY_DIR, 0, 0, 4));
        prev.insert("a/b/c".into(), meta(FS_ENTRY_FILE, 1, 1, 5));
        prev.insert("ab".into(), meta(FS_ENTRY_FILE, 1, 1, 6));
        let mut curr = Index::new();
        curr.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        let mut deleted: Vec<String> = diff(&prev, &curr)
            .into_iter()
            .map(|op| match op {
                DiffOp::Delete { path } => path,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        deleted.sort();
        assert_eq!(deleted, ["a", "a!x", "ab"].map(String::from));
    }

    /// The changed-key diff must agree with the full walk — moves, subtree
    /// deletes, adds, metadata changes — and skip keys that turn out equal
    /// (a hash fill-in rides the changed set but must emit nothing).
    #[test]
    fn diff_changed_matches_full_diff() {
        let mut prev = Index::new();
        prev.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        prev.insert("d".into(), meta(FS_ENTRY_DIR, 0, 0, 2));
        prev.insert("d/f".into(), meta(FS_ENTRY_FILE, 5, 10, 3));
        prev.insert("gone".into(), meta(FS_ENTRY_DIR, 0, 0, 4));
        prev.insert("gone/x".into(), meta(FS_ENTRY_FILE, 1, 1, 5));
        prev.insert("same".into(), meta(FS_ENTRY_FILE, 2, 2, 6));
        prev.insert("touched".into(), meta(FS_ENTRY_FILE, 3, 3, 7));
        let mut curr = Index::new();
        curr.insert("".into(), meta(FS_ENTRY_DIR, 0, 0, 1));
        curr.insert("e".into(), meta(FS_ENTRY_DIR, 0, 0, 2));
        curr.insert("e/f".into(), meta(FS_ENTRY_FILE, 5, 10, 3));
        curr.insert("same".into(), meta(FS_ENTRY_FILE, 2, 2, 6));
        curr.insert("touched".into(), meta(FS_ENTRY_FILE, 9, 9, 7));
        curr.insert("new".into(), meta(FS_ENTRY_FILE, 1, 1, 8));
        let changed: std::collections::BTreeSet<String> = [
            "d", "d/f", "e", "e/f", "gone", "gone/x", "touched", "new", "same",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let full = diff(&prev, &curr);
        assert_eq!(diff_changed(&prev, &curr, &changed), full);
        assert!(full.contains(&DiffOp::Move {
            from: "d".into(),
            to: "e".into()
        }));
        assert!(full.contains(&DiffOp::Delete {
            path: "gone".into()
        }));
        assert!(!full.iter().any(
            |op| matches!(op, DiffOp::Upsert { path, .. } | DiffOp::Delete { path } if path == "same")
        ));
    }

    #[test]
    fn retry_backoff_doubles_and_caps() {
        let latency = Duration::from_millis(20);
        assert_eq!(retry_backoff(1, latency), Duration::from_millis(20));
        assert_eq!(retry_backoff(2, latency), Duration::from_millis(40));
        assert_eq!(retry_backoff(5, latency), Duration::from_millis(320));
        assert_eq!(retry_backoff(8, latency), Duration::from_secs(2));
        // Shift overflow saturates at the cap instead of wrapping.
        assert_eq!(retry_backoff(64, latency), Duration::from_secs(2));
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

    /// A watched root must not keep itself alive through
    /// reconciler -> watcher -> callback sender -> reconciler. This used to
    /// strand one `blit-fsroot` plus notify's worker threads for every root
    /// ever opened by a connection.
    #[test]
    fn watched_root_worker_exits_when_last_handle_drops() {
        let root = temp_dir().canonicalize().unwrap();
        fs::write(root.join("seed.txt"), b"seed").unwrap();

        let shared = open_root(test_key(&root)).expect("arm native watch");
        let done = shared.worker_done.clone();
        drop(shared);

        let deadline = Instant::now() + Duration::from_secs(2);
        while !done.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = fs::remove_dir_all(&root);
        assert!(
            done.load(Ordering::Acquire),
            "reconciler still owns its native watcher after the root handle dropped"
        );
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

    /// SINGLE + real backend: the watch sits on the file's PARENT, so
    /// modify, delete, and recreate of the file all flow with no manual
    /// hints — including delete, which a watch armed on the file itself
    /// (inode-following) would go silent after.
    #[test]
    fn single_native_backend_follows_file() {
        let dir = temp_dir().canonicalize().unwrap();
        let file = dir.join("watched.txt");
        fs::write(&file, b"one").unwrap();

        let shared = open_single_root(file.clone()).expect("arm native watch on parent");
        assert!(shared.is_single());
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = SyncOptions {
            content: true,
            recursive: false,
            latency: Duration::from_millis(5),
            ..Default::default()
        };
        let handle = start_sync(
            &shared,
            15,
            opts,
            Box::new(move |msg| {
                sent2.lock().unwrap().push(msg);
                true
            }),
        );

        let mut mirror = FsMirror::new();
        let mut seen = 0usize;
        pump_until(&sent, &handle, &mut mirror, &mut seen, "initial", |m| {
            m.live
                .get("")
                .is_some_and(|n| n.content.as_deref() == Some(&b"one"[..]))
        });
        assert_eq!(mirror.live.len(), 1);

        fs::write(&file, b"two").unwrap();
        pump_until(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "native modify",
            |m| {
                m.live
                    .get("")
                    .is_some_and(|n| n.content.as_deref() == Some(&b"two"[..]))
            },
        );

        fs::remove_file(&file).unwrap();
        pump_until(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "native delete",
            |m| m.live.is_empty(),
        );
        assert_eq!(count_closed(&sent), 0, "delete must not close the sync");

        fs::write(&file, b"three").unwrap();
        pump_until(
            &sent,
            &handle,
            &mut mirror,
            &mut seen,
            "native recreate",
            |m| {
                m.live
                    .get("")
                    .is_some_and(|n| n.content.as_deref() == Some(&b"three"[..]))
            },
        );

        handle.command(Command::Stop);
        let _ = fs::remove_dir_all(&dir);
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
