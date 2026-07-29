//! The `GIT_STATE` engine (docs/design/git.md): one thread per watched
//! repository owning the mutable-state stream. Engines are shared across
//! opens — a crate-level registry keyed by canonical gitdir attaches every
//! `start_state` of one repo to the same engine, so N opens cost one
//! thread, one repository handle, and one set of watchers. The engine cuts
//! each snapshot once, at the superset of subscriber demands, and runs at
//! the minimum requested settle window; per-open state (requested flags,
//! ack window, identical-snapshot suppression) lives on each subscriber,
//! whose snapshots are filtered from the shared computation. Every
//! snapshot is complete — the client obligation is "replace the map" —
//! and pacing is coalescing per subscriber: at most one snapshot in
//! flight, the latest state wins once acked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use blit_remote::git::{
    GIT_CLOSED_BACKEND_FAILED, GIT_CLOSED_RESOURCE_LIMIT, GIT_HEAD_DETACHED, GIT_HEAD_UNBORN,
    GIT_OID_NONE, GIT_OP_BISECT, GIT_OP_CHERRY_PICK, GIT_OP_MERGE, GIT_OP_REBASE, GIT_OP_REVERT,
    GIT_REF_PEELED_VALID, GIT_REF_SYMBOLIC, GIT_STATE_RECORD_STATUS, GIT_STATE_REFS_TRUNCATED,
    GIT_STATE_STATUS_TRUNCATED, GIT_STATUS_OK, GIT_UPSTREAM_COUNTS_VALID, GIT_UPSTREAM_GONE,
    GitStateRecord, append_git_state_record, msg_git_closed, msg_git_state,
};

use crate::{Budgets, Outbox, RepoHandle, oid_bytes};

/// `GIT_CLOSED` reason for a native-watch arming failure: resource limit
/// for descriptor/watch exhaustion, backend failure otherwise.
fn watch_close_reason(err: &notify::Error) -> u8 {
    match &err.kind {
        notify::ErrorKind::MaxFilesWatch => GIT_CLOSED_RESOURCE_LIMIT,
        notify::ErrorKind::Io(e) => match e.raw_os_error() {
            Some(23) | Some(24) | Some(28) => GIT_CLOSED_RESOURCE_LIMIT,
            _ => GIT_CLOSED_BACKEND_FAILED,
        },
        _ => GIT_CLOSED_BACKEND_FAILED,
    }
}

/// Per-open state-stream options (`GIT_OPEN` flags + settle windows).
#[derive(Clone, Debug)]
pub struct StateOptions {
    /// Emit `GIT_STATE` snapshots. False for a log-only open made
    /// solely to drive `GIT_LOG_WATCH` subscriptions.
    pub wants_state: bool,
    pub status: bool,
    pub untracked: bool,
    pub ignored: bool,
    pub tracking: bool,
    pub refs_latency: Duration,
    pub status_latency: Duration,
}

impl Default for StateOptions {
    fn default() -> Self {
        StateOptions {
            wants_state: true,
            status: false,
            untracked: false,
            ignored: false,
            tracking: false,
            refs_latency: crate::env_latency("BLIT_GIT_REFS_LATENCY_MS", 50, 1000),
            status_latency: crate::env_latency("BLIT_GIT_STATUS_LATENCY_MS", 500, 10_000),
        }
    }
}

enum EngineMsg {
    Attach {
        sub_id: u64,
        repo_id: u16,
        opts: StateOptions,
        outbox: Outbox,
    },
    Detach {
        sub_id: u64,
    },
    Ack {
        sub_id: u64,
        state_id: u32,
    },
    /// Raw watcher event paths, classified on the engine thread (where the
    /// exclude stack lives).
    Event {
        paths: Vec<PathBuf>,
    },
    /// Subscribe one open to a live log of `spec`.
    WatchLog {
        sub_id: u64,
        log_id: u16,
        flags: u8,
        limit: u16,
        spec: String,
    },
    UnwatchLog {
        sub_id: u64,
        log_id: u16,
    },
    LogAck {
        sub_id: u64,
        log_id: u16,
        update_id: u32,
    },
    Stop,
}

// ---------------------------------------------------------------------------
// Engine registry: one engine per canonical gitdir, refcounted by handles
// ---------------------------------------------------------------------------

/// Live engines by canonical gitdir. Handles hold the strong refs, so the
/// map never keeps an engine alive on its own.
type EngineRegistry = Mutex<HashMap<PathBuf, Weak<EngineRef>>>;

fn engines() -> &'static EngineRegistry {
    static ENGINES: OnceLock<EngineRegistry> = OnceLock::new();
    ENGINES.get_or_init(Default::default)
}

/// The shared engine's inbox plus its registry key. Every `StateHandle`
/// holds one; the last drop is the teardown edge.
struct EngineRef {
    tx: Sender<EngineMsg>,
    key: Arc<PathBuf>,
}

impl Drop for EngineRef {
    fn drop(&mut self) {
        // Last subscriber out (docs/design/git.md: refcounted teardown):
        // clear the registry slot — unless a fresh engine already replaced
        // it — then stop the thread; the watchers drop with it.
        {
            let mut reg = engines().lock().unwrap();
            if let Some(slot) = reg.get(self.key.as_ref())
                && slot.upgrade().is_none()
            {
                reg.remove(self.key.as_ref());
            }
        }
        let _ = self.tx.send(EngineMsg::Stop);
    }
}

/// Live `StateHandle` count on the shared engine for `gitdir` — `None`
/// when no engine exists. Test/diagnostic hook.
#[doc(hidden)]
pub fn debug_engine_refs(gitdir: &Path) -> Option<usize> {
    let reg = engines().lock().unwrap();
    let engine = reg.get(gitdir)?.upgrade()?;
    // Minus this function's own upgraded ref.
    Some(Arc::strong_count(&engine) - 1)
}

/// Full status-pipeline runs for the engine keyed by canonical `gitdir`;
/// memo hits and ignore-filtered watch events do not count.
/// Test/diagnostic hook.
#[doc(hidden)]
pub fn debug_status_recomputes(gitdir: &Path) -> u64 {
    status_recomputes()
        .lock()
        .unwrap()
        .get(gitdir)
        .copied()
        .unwrap_or(0)
}

fn status_recomputes() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    COUNTS.get_or_init(Default::default)
}

/// Handle to one open's subscription on the shared state engine; dropping
/// it detaches, and the last detach stops the engine.
pub struct StateHandle {
    engine: Arc<EngineRef>,
    sub_id: u64,
}

impl StateHandle {
    pub fn ack(&self, state_id: u32) {
        let _ = self.engine.tx.send(EngineMsg::Ack {
            sub_id: self.sub_id,
            state_id,
        });
    }

    pub fn watch_log(&self, log_id: u16, flags: u8, limit: u16, spec: String) {
        let _ = self.engine.tx.send(EngineMsg::WatchLog {
            sub_id: self.sub_id,
            log_id,
            flags,
            limit,
            spec,
        });
    }

    pub fn unwatch_log(&self, log_id: u16) {
        let _ = self.engine.tx.send(EngineMsg::UnwatchLog {
            sub_id: self.sub_id,
            log_id,
        });
    }

    pub fn log_ack(&self, log_id: u16, update_id: u32) {
        let _ = self.engine.tx.send(EngineMsg::LogAck {
            sub_id: self.sub_id,
            log_id,
            update_id,
        });
    }

    /// Detach this open from the shared engine; the engine (and its
    /// watchers) stop when the last open detaches.
    pub fn stop(&self) {
        let _ = self.engine.tx.send(EngineMsg::Detach {
            sub_id: self.sub_id,
        });
    }
}

impl Drop for StateHandle {
    fn drop(&mut self) {
        let _ = self.engine.tx.send(EngineMsg::Detach {
            sub_id: self.sub_id,
        });
        // `engine` drops after this: the last handle's EngineRef drop is
        // what stops the thread.
    }
}

impl RepoHandle {
    /// Attach to the repo's shared state engine, spawning it on first
    /// attach: an immediate first snapshot, then snapshots after settled
    /// changes, at most one unacked per open.
    pub fn start_state(&self, repo_id: u16, opts: StateOptions, outbox: Outbox) -> StateHandle {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_SUB: AtomicU64 = AtomicU64::new(1);
        static NEXT_ENGINE: AtomicU64 = AtomicU64::new(1);
        let sub_id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
        let mut attach = EngineMsg::Attach {
            sub_id,
            repo_id,
            opts,
            outbox,
        };
        let mut reg = engines().lock().unwrap();
        if let Some(engine) = reg.get(self.gitdir.as_ref()).and_then(Weak::upgrade) {
            match engine.tx.send(attach) {
                Ok(()) => return StateHandle { engine, sub_id },
                // The engine thread is gone (panic): replace it below.
                Err(std::sync::mpsc::SendError(msg)) => attach = msg,
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = Arc::new(EngineRef {
            tx,
            key: self.gitdir.clone(),
        });
        reg.insert((*self.gitdir).clone(), Arc::downgrade(&engine));
        drop(reg);
        // Queued before the thread starts, so the first message the engine
        // sees is this subscriber.
        let _ = engine.tx.send(attach);
        let watch_tx = engine.tx.clone();
        let handle = self.clone();
        let seq = NEXT_ENGINE.fetch_add(1, Ordering::Relaxed);
        std::thread::Builder::new()
            .name(format!("blit-git-state-{seq}"))
            .spawn(move || Engine::new(handle).run(rx, watch_tx))
            .expect("spawn git state engine");
        StateHandle { engine, sub_id }
    }
}

// ---------------------------------------------------------------------------
// The engine proper
// ---------------------------------------------------------------------------

/// One live log subscription (`GIT_LOG_WATCH`).
struct LogSub {
    flags: u8,
    limit: u16,
    spec: String,
    /// Last resolved endpoints; a page is re-sent only when these move.
    endpoints: Option<(Vec<gix::ObjectId>, Vec<gix::ObjectId>)>,
    /// A ref moved (or first registration): re-resolve on the next tick.
    dirty: bool,
    /// The one in-flight page id, if any (coalescing pacing).
    unacked: Option<u32>,
    next_update_id: u32,
}

/// One open's view of the shared engine: its requested flags, its own ack
/// window and id sequence, its own identical-snapshot suppression, and
/// its own log subscriptions (log ids are client-assigned per open).
struct Subscriber {
    repo_id: u16,
    opts: StateOptions,
    outbox: Outbox,
    next_state_id: u32,
    /// The one in-flight `GIT_STATE` snapshot id, if any.
    unacked: Option<u32>,
    /// Needs a (re-)send once the ack window frees.
    pending: bool,
    /// The last sent `(flags, records)`: a byte-identical snapshot is not
    /// re-sent and burns no state_id.
    last_sent: Option<(u8, Vec<u8>)>,
    /// Live log subscriptions, keyed by client-assigned `log_id`.
    log_subs: HashMap<u16, LogSub>,
    /// Outbox dead or closed server-side; reaped after the current pass.
    gone: bool,
}

/// The union of subscriber demands: what the shared computation must
/// cover so every subscriber's filtered view is complete.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Demand {
    status: bool,
    untracked: bool,
    ignored: bool,
    tracking: bool,
}

/// One computed snapshot, cut at the superset demand and assembled per
/// subscriber from these segments.
struct Parts {
    /// HEAD/refs/op/pseudo-ref/stash records — every subscriber gets these.
    base: Vec<u8>,
    refs_truncated: bool,
    /// UPSTREAM records; present when any subscriber wants TRACKING.
    tracking: Option<Vec<u8>>,
    /// STATUS records at the superset untracked/ignored demand, plus the
    /// truncation flag.
    status: Option<(Vec<u8>, bool)>,
    /// The demand the segments were computed under.
    demand: Demand,
}

/// The armed native watches. The recursive worktree watch already covers
/// a gitdir living inside the worktree, so while it is up the targeted
/// gitdir watches are dropped rather than double-watching the `.git`
/// subtree.
struct Arms {
    watcher: notify::RecommendedWatcher,
    /// Targeted gitdir/common paths currently armed; empty while the
    /// worktree watch covers them.
    gitdir_paths: Vec<PathBuf>,
    worktree: bool,
}

struct Engine {
    repo: RepoHandle,
    /// Engine-thread repository, re-opened when `config` changes so the
    /// upstream mapping and exclude sources stay fresh for the shared
    /// engine's whole life (the open-time snapshot in `repo` cannot).
    local: gix::Repository,
    local_stale: bool,
    /// Per-open subscribers, keyed by attach id.
    subs: HashMap<u64, Subscriber>,
    /// Effective settle windows: the minimum across subscribers
    /// (docs/design/git.md: "runs at the minimum requested window and
    /// coalesces for slower clients").
    refs_latency: Duration,
    status_latency: Duration,
    /// Earliest settle deadline for a pending ref/HEAD/op/stash change.
    refs_due: Option<Instant>,
    /// Earliest settle deadline for a pending worktree-status change. Kept
    /// separate so a slow status window never delays a ref/HEAD update —
    /// the snapshot fires at whichever deadline comes first.
    status_due: Option<Instant>,
    /// The worktree side changed: the status pipeline must recompute. A
    /// pure ref settle leaves this clear and reuses the previous status
    /// records unless the fingerprinted status inputs (HEAD, index,
    /// info/exclude) moved.
    status_dirty: bool,
    /// The last computed status segment and the inputs it derives from.
    status_memo: Option<StatusMemo>,
    /// HEAD-flatten memo and worktree stat cache for the status pipeline.
    status_caches: crate::diffs::StatusCaches,
    /// Ahead/behind memoized by the immutable `(tip, upstream)` oid pair
    /// (docs/design/git.md UPSTREAM); rebuilt each snapshot so pairs no
    /// longer referenced are evicted.
    ahead_behind: HashMap<(gix::ObjectId, gix::ObjectId), (u8, u32, u32)>,
    /// The last computed snapshot segments, shared by every subscriber
    /// until the next settled change (or a demand change).
    parts: Option<Parts>,
    /// Exclude stack for ignore-filtering worktree events; invalidated on
    /// any ignore-source change and rebuilt lazily.
    excludes: Option<gix::worktree::Stack>,
    /// `core.excludesFile` as configured, for event-path matching.
    excludes_file: Option<PathBuf>,
    watch: Option<Arms>,
    /// Set when watching can never work (watcher creation failed): every
    /// current and future subscriber is closed with this reason.
    fatal: Option<u8>,
    gitdir: PathBuf,
    common: PathBuf,
    workdir: Option<PathBuf>,
    /// `config` files under the gitdir roots: events here refresh the
    /// engine repository (upstream mapping, core.excludesFile).
    config_paths: [PathBuf; 2],
    /// `info/exclude` under the gitdir roots: ignore sources.
    exclude_paths: [PathBuf; 2],
}

/// The non-worktree inputs the status pipeline reads, fingerprinted so a
/// ref-side settle only recomputes status when one of them actually
/// moved: HEAD's commit (staged side), the index file (both sides), and
/// `info/exclude` (untracked classification). Worktree `.gitignore`
/// edits arrive as worktree events and set `status_dirty` instead.
struct StatusMemo {
    head: Option<gix::ObjectId>,
    index_sig: Option<FileSig>,
    exclude_sig: Option<FileSig>,
    /// The `(untracked, ignored)` superset the records were computed at;
    /// a demand change past it recomputes.
    demand: (bool, bool),
    records: Vec<u8>,
    truncated: bool,
}

/// A file's size + full-precision mtime (+ inode on unix), the same
/// precision bar as the worktree stat cache.
#[derive(Clone, Copy, PartialEq)]
struct FileSig {
    size: u64,
    mtime_s: i64,
    mtime_ns: u32,
    #[cfg(unix)]
    ino: u64,
}

fn file_sig(path: &std::path::Path) -> Option<FileSig> {
    use std::time::UNIX_EPOCH;
    let md = std::fs::symlink_metadata(path).ok()?;
    let disk = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(FileSig {
        size: md.len(),
        mtime_s: disk.as_secs() as i64,
        mtime_ns: disk.subsec_nanos(),
        #[cfg(unix)]
        ino: md.ino(),
    })
}

impl Engine {
    fn new(repo: RepoHandle) -> Engine {
        let local = repo.local();
        let gitdir = local.git_dir().to_path_buf();
        let common = local.common_dir().to_path_buf();
        let workdir = local.workdir().map(|p| p.to_path_buf());
        let defaults = StateOptions::default();
        let excludes_file = config_excludes_file(&local);
        let config_paths = [gitdir.join("config"), common.join("config")];
        let exclude_paths = [
            gitdir.join("info").join("exclude"),
            common.join("info").join("exclude"),
        ];
        Engine {
            repo,
            local,
            local_stale: false,
            subs: HashMap::new(),
            refs_latency: defaults.refs_latency,
            status_latency: defaults.status_latency,
            refs_due: None,
            status_due: None,
            status_dirty: true,
            status_memo: None,
            status_caches: Default::default(),
            ahead_behind: Default::default(),
            parts: None,
            excludes: None,
            excludes_file,
            watch: None,
            fatal: None,
            gitdir,
            common,
            workdir,
            config_paths,
            exclude_paths,
        }
    }

    fn run(mut self, rx: Receiver<EngineMsg>, watch_tx: Sender<EngineMsg>) {
        // Serve the attaches queued before this thread started, so the
        // watch set is armed against real subscriber demand in one pass
        // (see `sync_watches`) rather than armed broadly and narrowed.
        // Arming stays ahead of the first snapshot: a change landing
        // between the two would raise no event and leave state stale.
        while let Ok(msg) = rx.try_recv() {
            if self.handle_msg(msg) {
                return;
            }
        }
        if let Err(reason) = self.arm_watcher(watch_tx) {
            // Watching can never work — state would silently go stale, so
            // every subscriber (present and future) is closed with the
            // reason. The thread stays to answer attaches until the last
            // handle detaches.
            self.fatal = Some(reason);
            self.close_all(reason);
        }
        loop {
            let now = Instant::now();
            // Fire elapsed settle timers. A ref change invalidates the
            // shared snapshot and every log subscription (its endpoints
            // may have moved); a status change additionally dirties the
            // status pipeline.
            if self.refs_due.is_some_and(|d| now >= d) {
                self.refs_due = None;
                self.parts = None;
                for sub in self.subs.values_mut() {
                    sub.pending = true;
                    for log in sub.log_subs.values_mut() {
                        log.dirty = true;
                    }
                }
            }
            if self.status_due.is_some_and(|d| now >= d) {
                self.status_due = None;
                self.parts = None;
                self.status_dirty = true;
                for sub in self.subs.values_mut() {
                    sub.pending = true;
                }
            }
            self.reap();
            // Watches reconcile before the snapshot is cut, so a change
            // landing right after the cut still raises an event.
            self.sync_watches();
            self.emit_states();
            self.service_log_subs();
            let timeout = match [self.refs_due, self.status_due].into_iter().flatten().min() {
                Some(due) => due.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            match rx.recv_timeout(timeout) {
                Ok(msg) => {
                    if self.handle_msg(msg) {
                        return;
                    }
                    // Drain whatever else queued (event bursts) before the
                    // next compute pass.
                    while let Ok(msg) = rx.try_recv() {
                        if self.handle_msg(msg) {
                            return;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Returns true when the engine must stop.
    fn handle_msg(&mut self, msg: EngineMsg) -> bool {
        match msg {
            EngineMsg::Attach {
                sub_id,
                repo_id,
                opts,
                outbox,
            } => {
                if let Some(reason) = self.fatal {
                    let mut outbox = outbox;
                    let _ = outbox(msg_git_closed(repo_id, reason));
                    return false;
                }
                self.subs.insert(
                    sub_id,
                    Subscriber {
                        repo_id,
                        opts,
                        outbox,
                        next_state_id: 1,
                        unacked: None,
                        pending: true,
                        last_sent: None,
                        log_subs: HashMap::new(),
                        gone: false,
                    },
                );
                self.recompute_windows();
            }
            EngineMsg::Detach { sub_id } => {
                if self.subs.remove(&sub_id).is_some() {
                    self.recompute_windows();
                }
            }
            EngineMsg::Ack { sub_id, state_id } => {
                if let Some(sub) = self.subs.get_mut(&sub_id)
                    && sub.unacked == Some(state_id)
                {
                    sub.unacked = None;
                }
            }
            EngineMsg::Event { paths } => self.handle_event(&paths),
            EngineMsg::WatchLog {
                sub_id,
                log_id,
                flags,
                limit,
                spec,
            } => {
                let max_log_subs = self.repo.budgets.max_log_subs;
                let Some(sub) = self.subs.get_mut(&sub_id) else {
                    return false;
                };
                // Re-watching an existing id replaces it; a new id past the
                // cap is refused with a BUDGET page so the client unblocks
                // rather than waiting forever for a subscription that never
                // registered.
                if !sub.log_subs.contains_key(&log_id) && sub.log_subs.len() >= max_log_subs {
                    let msg = blit_remote::git::msg_git_log_page(
                        log_id,
                        1,
                        blit_remote::git::GIT_STATUS_BUDGET,
                        0,
                        &[],
                        &[],
                    );
                    if !(sub.outbox)(msg) {
                        sub.gone = true;
                    }
                    return false;
                }
                sub.log_subs.insert(
                    log_id,
                    LogSub {
                        flags,
                        limit,
                        spec,
                        endpoints: None,
                        dirty: true,
                        unacked: None,
                        next_update_id: 1,
                    },
                );
            }
            EngineMsg::UnwatchLog { sub_id, log_id } => {
                if let Some(sub) = self.subs.get_mut(&sub_id) {
                    sub.log_subs.remove(&log_id);
                }
            }
            EngineMsg::LogAck {
                sub_id,
                log_id,
                update_id,
            } => {
                if let Some(sub) = self.subs.get_mut(&sub_id)
                    && let Some(log) = sub.log_subs.get_mut(&log_id)
                    && log.unacked == Some(update_id)
                {
                    log.unacked = None;
                }
            }
            EngineMsg::Stop => return true,
        }
        false
    }

    /// Close every attached subscriber with `reason` — the watcher failed
    /// after they attached, so their state can no longer be trusted.
    fn close_all(&mut self, reason: u8) {
        for sub in self.subs.values_mut() {
            let _ = (sub.outbox)(msg_git_closed(sub.repo_id, reason));
            sub.gone = true;
        }
    }

    /// Drop subscribers whose outbox died or that were closed. The engine
    /// itself stops only when the last handle detaches (registry
    /// refcount), so a dead client never strands the other opens.
    fn reap(&mut self) {
        if self.subs.values().any(|s| s.gone) {
            self.subs.retain(|_, s| !s.gone);
            self.recompute_windows();
        }
    }

    /// Effective settle windows: the minimum across subscribers, so the
    /// engine reacts as fast as its fastest client asked; slower clients
    /// coalesce through their own ack windows. Only status-requesting
    /// subscribers vote on the status window — a log-only open's default
    /// must not drag recomputation faster than any status client wants.
    fn recompute_windows(&mut self) {
        let defaults = StateOptions::default();
        self.refs_latency = self
            .subs
            .values()
            .map(|s| s.opts.refs_latency)
            .min()
            .unwrap_or(defaults.refs_latency);
        self.status_latency = self
            .subs
            .values()
            .filter(|s| s.opts.status)
            .map(|s| s.opts.status_latency)
            .min()
            .unwrap_or(defaults.status_latency);
    }

    // -- watches ------------------------------------------------------------

    /// Create the watcher and arm the initial set. `Err(reason)` when the
    /// watcher itself cannot exist.
    fn arm_watcher(&mut self, tx: Sender<EngineMsg>) -> Result<(), u8> {
        // The dominant gitdir churn (fetch/gc/commit/hash-object) writes
        // under objects/; those events carry no HEAD/ref/status meaning,
        // so drop them before they reach the engine thread.
        let objects = [self.gitdir.join("objects"), self.common.join("objects")];
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = &res else {
                return;
            };
            // Recomputing status opens `.gitignore`, `HEAD` and the refs it
            // watches; on Linux those opens come back as events, so without
            // this the settle window spins instead of debouncing.
            if blit_fssync::backend::is_read_only_event(&event.kind) {
                return;
            }
            if !event.paths.is_empty()
                && event
                    .paths
                    .iter()
                    .all(|p| objects.iter().any(|o| p.starts_with(o)))
            {
                return;
            }
            let _ = tx.send(EngineMsg::Event {
                paths: event.paths.clone(),
            });
        })
        .map_err(|e| watch_close_reason(&e))?;
        self.watch = Some(Arms {
            watcher,
            gitdir_paths: Vec::new(),
            worktree: false,
        });
        // `sync_watches` picks the set: when a status subscriber's
        // recursive worktree watch already covers the gitdir, arming the
        // targeted gitdir watches here would only be undone a moment
        // later, at the cost of a native stream rebuild per path.
        self.sync_watches();
        Ok(())
    }

    /// Non-recursive on the gitdir roots (HEAD, index, MERGE_HEAD…),
    /// recursive on refs/ and the sequencer dirs. Individual arm failures
    /// are tolerated (a missing subdir simply is not watched); the paths
    /// that did arm are recorded so the set can be dropped when the
    /// worktree watch covers it.
    fn arm_gitdir(&mut self) {
        use notify::Watcher as _;
        let dirs = [self.gitdir.clone(), self.common.clone()];
        let Some(arms) = &mut self.watch else {
            return;
        };
        if !arms.gitdir_paths.is_empty() {
            return;
        }
        for dir in dirs.iter().collect::<std::collections::HashSet<_>>() {
            if arms
                .watcher
                .watch(dir, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                arms.gitdir_paths.push(dir.clone());
            }
            for sub in [
                "refs",
                "rebase-merge",
                "rebase-apply",
                "sequencer",
                "logs/refs",
            ] {
                let path = dir.join(sub);
                if path.exists()
                    && arms
                        .watcher
                        .watch(&path, notify::RecursiveMode::Recursive)
                        .is_ok()
                {
                    arms.gitdir_paths.push(path);
                }
            }
        }
    }

    fn disarm_gitdir(&mut self) {
        use notify::Watcher as _;
        let Some(arms) = &mut self.watch else {
            return;
        };
        for path in arms.gitdir_paths.drain(..) {
            let _ = arms.watcher.unwatch(&path);
        }
    }

    /// True when the recursive worktree watch already delivers gitdir
    /// events (the `.git` directory lives inside the worktree).
    fn gitdir_covered(&self) -> bool {
        self.workdir
            .as_deref()
            .is_some_and(|w| self.gitdir.starts_with(w) && self.common.starts_with(w))
    }

    /// Reconcile the armed watches with subscriber demand: the worktree
    /// watch exists while any subscriber wants status, and while it covers
    /// the gitdir the targeted gitdir watches are dropped rather than
    /// double-watching the `.git` subtree.
    fn sync_watches(&mut self) {
        use notify::Watcher as _;
        let Some(arms) = &self.watch else {
            return;
        };
        let armed = arms.worktree;
        let want = self.workdir.is_some() && self.subs.values().any(|s| s.opts.status && !s.gone);
        if want && !armed {
            let workdir = self.workdir.clone().expect("want implies workdir");
            let result = self
                .watch
                .as_mut()
                .expect("checked above")
                .watcher
                .watch(&workdir, notify::RecursiveMode::Recursive);
            match result {
                Ok(()) => self.watch.as_mut().expect("checked above").worktree = true,
                Err(e) => {
                    // The worktree watch is load-bearing for status: those
                    // subscribers would silently never update, so close
                    // them; watch-less opens are unaffected.
                    let reason = watch_close_reason(&e);
                    for sub in self.subs.values_mut().filter(|s| s.opts.status && !s.gone) {
                        let _ = (sub.outbox)(msg_git_closed(sub.repo_id, reason));
                        sub.gone = true;
                    }
                }
            }
        }
        // Reconcile the targeted gitdir watches against what the worktree
        // watch already covers. Both calls are idempotent, so the common
        // case (steady state, or a first arm that goes straight to the
        // right set) issues no watcher calls at all — each one rebuilds
        // the whole native stream, so arming a set only to drop it again
        // costs far more than the bookkeeping.
        let covered = self.watch.as_ref().is_some_and(|a| a.worktree) && self.gitdir_covered();
        if covered {
            self.disarm_gitdir();
        } else {
            // Arm before any unwatch below, so no window opens where a ref
            // move is unseen.
            self.arm_gitdir();
        }
        if !want && armed {
            let workdir = self.workdir.clone().expect("armed implies workdir");
            if let Some(arms) = &mut self.watch {
                let _ = arms.watcher.unwatch(&workdir);
                arms.worktree = false;
            }
        }
    }

    // -- event classification (ignore-filtered) -----------------------------

    /// Classify raw watch paths into the two settle sides. Worktree events
    /// filter through the repo's ignore rules (docs/design/git.md status
    /// side) unless a subscriber surfaces ignored files; correctness beats
    /// savings — anything unclassifiable dirties status.
    fn handle_event(&mut self, paths: &[PathBuf]) {
        let mut refs_side = false;
        let mut status_side = false;
        // An empty path set (backend rescan) is unattributable: both sides.
        if paths.is_empty() {
            refs_side = true;
            status_side = true;
        }
        let workdir = self.workdir.clone();
        for path in paths {
            if self.is_exclude_source(path) {
                // An ignore-source edit changes classifications the
                // previous snapshot baked in: rebuild the stack AND
                // recompute status.
                self.excludes = None;
                status_side = true;
                if self.under_gitdir(path) {
                    refs_side = true;
                }
                continue;
            }
            if self.config_paths.iter().any(|c| path == c) {
                // Config drives the upstream mapping and core.excludesFile:
                // refresh the engine repository and the exclude stack.
                self.excludes = None;
                self.local_stale = true;
                refs_side = true;
                continue;
            }
            if self.under_gitdir(path) {
                refs_side = true;
                continue;
            }
            match &workdir {
                Some(workdir) if path.starts_with(workdir) => {
                    if self.ignored_surfaced() || !self.path_ignored(path, workdir) {
                        status_side = true;
                    }
                }
                // Outside every watched root: cannot classify — an extra
                // recompute, never a lost update.
                _ => status_side = true,
            }
        }
        if refs_side {
            self.arm(false);
        }
        if status_side {
            self.arm(true);
        }
    }

    /// Arm the matching side's settle window; same-side events debounce
    /// (extend), but a ref event never inherits the coarser status window
    /// and vice versa.
    fn arm(&mut self, status_side: bool) {
        let (slot, latency) = if status_side {
            (&mut self.status_due, self.status_latency)
        } else {
            (&mut self.refs_due, self.refs_latency)
        };
        let due = Instant::now() + latency;
        match *slot {
            Some(existing) if existing >= due => {}
            _ => *slot = Some(due),
        }
    }

    fn under_gitdir(&self, path: &Path) -> bool {
        path.starts_with(&self.gitdir) || path.starts_with(&self.common)
    }

    /// A file whose content feeds the exclude stack: any `.gitignore`,
    /// the gitdir `info/exclude`s, or the configured `core.excludesFile`.
    /// Its own events must both invalidate the stack and dirty status.
    fn is_exclude_source(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|n| n == ".gitignore")
            || self.exclude_paths.iter().any(|p| path == p)
            || self.excludes_file.as_deref() == Some(path)
    }

    /// True when any subscriber opened with IGNORED: ignored files appear
    /// in its status, so ignored-path events are real updates for it and
    /// the filter must not run.
    fn ignored_surfaced(&self) -> bool {
        self.subs
            .values()
            .any(|s| s.opts.status && s.opts.ignored && !s.gone)
    }

    /// Definitively ignored? A deleted path's dir-vs-file reading is
    /// unknowable, so it counts as ignored only when BOTH interpretations
    /// are (`target/` ignores the directory but not a file named
    /// `target`). Any failure — stack build, non-decodable path — reads
    /// as not-ignored: the safe direction is a recompute.
    fn path_ignored(&mut self, abs: &Path, workdir: &Path) -> bool {
        let Ok(rel) = abs.strip_prefix(workdir) else {
            return false;
        };
        if rel.as_os_str().is_empty() {
            return false;
        }
        let Ok(rel) = gix::path::os_str_into_bstr(rel.as_os_str()) else {
            return false;
        };
        if self.excludes.is_none() {
            self.build_excludes();
        }
        let Some(stack) = self.excludes.as_mut() else {
            return false;
        };
        use gix::index::entry::Mode;
        let mode = match std::fs::symlink_metadata(abs) {
            Ok(md) => Some(if md.is_dir() { Mode::DIR } else { Mode::FILE }),
            Err(_) => None,
        };
        let objects = &self.local.objects;
        let mut excluded = |mode: Mode| -> bool {
            stack
                .at_entry(rel, Some(mode), objects)
                .map(|platform| platform.is_excluded())
                .unwrap_or(false)
        };
        match mode {
            Some(mode) => excluded(mode),
            None => excluded(Mode::FILE) && excluded(Mode::DIR),
        }
    }

    /// (Re)build the exclude stack from the engine repository, refreshing
    /// the remembered `core.excludesFile` path. Left `None` (every path
    /// then reads not-ignored) when the repo has no worktree or a source
    /// fails to load.
    fn build_excludes(&mut self) {
        if self.local_stale {
            self.refresh_local();
        }
        self.excludes_file = config_excludes_file(&self.local);
        self.excludes = self
            .local
            .worktree()
            .and_then(|worktree| worktree.excludes(None).ok().map(|stack| stack.detach()));
    }

    /// The shared `ThreadSafeRepository` keeps its open-time config
    /// snapshot, so a `config` change re-opens the engine's own
    /// repository — the upstream mapping and exclude sources read fresh
    /// values. On failure the old instance stays: stale but serving.
    fn refresh_local(&mut self) {
        self.local_stale = false;
        let start = self.workdir.as_deref().unwrap_or(&self.gitdir);
        if let Ok(fresh) = gix::ThreadSafeRepository::discover(start) {
            self.local = self.repo.sized(fresh.to_thread_local());
        }
    }

    // -- snapshots ----------------------------------------------------------

    /// The union of live subscriber demands.
    fn demand(&self) -> Demand {
        let mut demand = Demand::default();
        for sub in self.subs.values().filter(|s| s.opts.wants_state && !s.gone) {
            demand.status |= sub.opts.status;
            demand.untracked |= sub.opts.untracked;
            demand.ignored |= sub.opts.ignored;
            demand.tracking |= sub.opts.tracking;
        }
        demand
    }

    /// Send each pending subscriber its filtered view of the shared
    /// snapshot, computing the snapshot at most once per settled change.
    fn emit_states(&mut self) {
        if !self
            .subs
            .values()
            .any(|s| s.opts.wants_state && s.pending && s.unacked.is_none() && !s.gone)
        {
            return;
        }
        let demand = self.demand();
        if self.parts.as_ref().map(|p| p.demand) != Some(demand) {
            self.parts = None;
        }
        let parts = match self.parts.take() {
            Some(parts) => parts,
            None => self.compute_parts(demand),
        };
        for sub in self.subs.values_mut() {
            if sub.gone || !sub.opts.wants_state || !sub.pending || sub.unacked.is_some() {
                continue;
            }
            sub.pending = false;
            let (flags, records) = assemble(&parts, &sub.opts);
            // A byte-identical snapshot carries no new state — the
            // stream's contract is "latest state" — so skip the send and
            // keep the state_id for the next real change.
            if sub
                .last_sent
                .as_ref()
                .is_some_and(|(last_flags, last)| *last_flags == flags && *last == records)
            {
                continue;
            }
            let state_id = sub.next_state_id;
            sub.next_state_id = sub.next_state_id.wrapping_add(1);
            if !(sub.outbox)(msg_git_state(sub.repo_id, state_id, flags, &records)) {
                sub.gone = true;
                continue;
            }
            sub.unacked = Some(state_id);
            sub.last_sent = Some((flags, records));
        }
        self.parts = Some(parts);
    }

    /// Cut the snapshot segments once, at the superset of subscriber
    /// demands; per-subscriber assembly filters from here.
    fn compute_parts(&mut self, demand: Demand) -> Parts {
        if self.local_stale {
            self.refresh_local();
        }
        let repo = &self.local;
        let mut base = Vec::new();
        head_record(repo, &mut base);
        let mut branches: Vec<String> = Vec::new();
        let entries_max = self.repo.budgets.entries_max;
        let refs_truncated = !refs_records(repo, entries_max, &mut base, &mut branches);
        op_record(repo, &mut base);
        special_ref_records(repo, &mut base);
        stash_records(repo, entries_max, &mut base);
        let tracking = demand.tracking.then(|| {
            let mut records = Vec::new();
            upstream_records(
                repo,
                self.repo.budgets.walk_max,
                &mut self.ahead_behind,
                &branches,
                &mut records,
            );
            records
        });
        let status = demand.status.then(|| {
            status_segment(
                repo,
                demand,
                &self.repo.budgets,
                &mut self.status_dirty,
                &mut self.status_memo,
                &mut self.status_caches,
                self.repo.gitdir.as_ref(),
            )
        });
        Parts {
            base,
            refs_truncated,
            tracking,
            status,
            demand,
        }
    }

    /// Re-resolve and re-send any dirty log subscription whose ack window
    /// is free and whose endpoints actually moved.
    fn service_log_subs(&mut self) {
        let any = self.subs.values().any(|s| {
            !s.gone
                && s.log_subs
                    .values()
                    .any(|log| log.dirty && log.unacked.is_none())
        });
        if !any {
            return;
        }
        let repo = self.repo.local();
        let budgets = self.repo.budgets.clone();
        let memo = self.repo.merge_memo.clone();
        for sub in self.subs.values_mut() {
            if sub.gone {
                continue;
            }
            let ids: Vec<u16> = sub
                .log_subs
                .iter()
                .filter(|(_, log)| log.dirty && log.unacked.is_none())
                .map(|(id, _)| *id)
                .collect();
            for log_id in ids {
                let cancel = crate::Cancel::default();
                let log = sub.log_subs.get_mut(&log_id).expect("present");
                log.dirty = false;
                // Resolve the spec to endpoints. Plain refs are cheap
                // lookups and `A...B` merge bases come from the oid-pair
                // memo, so a ref settle that moved nothing re-walks
                // nothing.
                let resolved = crate::requests::resolve_spec(
                    &repo,
                    &memo,
                    &log.spec,
                    budgets.walk_max,
                    &cancel,
                );
                let (page, endpoints) = match resolved {
                    Ok((tips, hides)) => {
                        // Skip when endpoints are unchanged and a page was
                        // sent.
                        if log.endpoints.as_ref() == Some(&(tips.clone(), hides.clone())) {
                            continue;
                        }
                        let limit = if log.limit == 0 {
                            budgets.log_default
                        } else {
                            (log.limit as usize).min(budgets.log_max)
                        };
                        match crate::requests::walk_log(
                            &repo,
                            tips.clone(),
                            hides.clone(),
                            log.flags,
                            limit,
                            None,
                            &budgets,
                            &cancel,
                        ) {
                            Ok((records, frontier, more)) => {
                                let flags = if more {
                                    blit_remote::git::GIT_COMMITS_MORE
                                } else {
                                    0
                                };
                                let update_id = log.next_update_id;
                                log.next_update_id = log.next_update_id.wrapping_add(1);
                                let msg = blit_remote::git::msg_git_log_page(
                                    log_id,
                                    update_id,
                                    GIT_STATUS_OK,
                                    flags,
                                    &frontier,
                                    &records,
                                );
                                log.unacked = Some(update_id);
                                (msg, Some((tips, hides)))
                            }
                            Err(status) => {
                                let update_id = log.next_update_id;
                                log.next_update_id = log.next_update_id.wrapping_add(1);
                                let msg = blit_remote::git::msg_git_log_page(
                                    log_id,
                                    update_id,
                                    status,
                                    0,
                                    &[],
                                    &[],
                                );
                                log.unacked = Some(update_id);
                                (msg, None)
                            }
                        }
                    }
                    Err(status) => {
                        // Unresolvable (e.g. a ref that does not exist yet):
                        // report the status, keep the sub alive so it
                        // recovers when the ref appears.
                        let update_id = log.next_update_id;
                        log.next_update_id = log.next_update_id.wrapping_add(1);
                        let msg = blit_remote::git::msg_git_log_page(
                            log_id,
                            update_id,
                            status,
                            0,
                            &[],
                            &[],
                        );
                        log.unacked = Some(update_id);
                        (msg, None)
                    }
                };
                log.endpoints = endpoints;
                if !(sub.outbox)(page) {
                    sub.gone = true;
                    break;
                }
            }
        }
    }
}

/// One open's snapshot from the shared parts: segments the open did not
/// request are dropped, and status records are filtered to the letters
/// its flags admit.
fn assemble(parts: &Parts, opts: &StateOptions) -> (u8, Vec<u8>) {
    let mut flags = 0u8;
    if parts.refs_truncated {
        flags |= GIT_STATE_REFS_TRUNCATED;
    }
    let mut records = parts.base.clone();
    if opts.tracking
        && let Some(tracking) = &parts.tracking
    {
        records.extend_from_slice(tracking);
    }
    if opts.status
        && let Some((status, truncated)) = &parts.status
    {
        filter_status(status, opts.untracked, opts.ignored, &mut records);
        // Conservative: the superset walk's truncation may or may not
        // have cost this subscriber entries; over-reporting TRUNCATED is
        // harmless, under-reporting would lie.
        if *truncated {
            flags |= GIT_STATE_STATUS_TRUNCATED;
        }
    }
    (flags, records)
}

/// Copy STATUS records, dropping or blanking porcelain letters the open's
/// flags do not admit: '?' needs UNTRACKED, '!' needs IGNORED. A staged
/// letter beside a filtered worktree letter survives with the worktree
/// side blanked (the delete-then-recreate case); a record left with two
/// blanks disappears entirely.
fn filter_status(records: &[u8], untracked: bool, ignored: bool, out: &mut Vec<u8>) {
    if untracked && ignored {
        out.extend_from_slice(records);
        return;
    }
    let admit = |letter: u8| match letter {
        b'?' => untracked,
        b'!' => ignored,
        _ => true,
    };
    let mut data = records;
    // Frames are `[record_len:4][kind:1][…]` (docs/design/git.md records);
    // STATUS carries `[staged:1][unstaged:1]` right after the kind.
    while data.len() >= 4 {
        let len = u32::from_le_bytes(data[0..4].try_into().expect("4 bytes")) as usize;
        if len == 0 || data.len() < 4 + len {
            break; // malformed framing ends the payload, as on the wire
        }
        let frame = &data[..4 + len];
        data = &data[4 + len..];
        if frame[4] != GIT_STATE_RECORD_STATUS || len < 3 {
            out.extend_from_slice(frame);
            continue;
        }
        let staged = if admit(frame[5]) { frame[5] } else { b' ' };
        let unstaged = if admit(frame[6]) { frame[6] } else { b' ' };
        if staged == b' ' && unstaged == b' ' {
            continue; // entirely outside this open's view
        }
        let start = out.len();
        out.extend_from_slice(frame);
        out[start + 5] = staged;
        out[start + 6] = unstaged;
    }
}

/// The STATUS segment through the engine's memo: worktree events set
/// `dirty`; HEAD, the index file, and `info/exclude` are fingerprinted so
/// a pure ref settle (branch created, tag pushed) reuses the previous
/// records verbatim. A demand change past what the memo holds recomputes.
fn status_segment(
    repo: &gix::Repository,
    demand: Demand,
    budgets: &Budgets,
    dirty: &mut bool,
    memo: &mut Option<StatusMemo>,
    caches: &mut crate::diffs::StatusCaches,
    key: &Path,
) -> (Vec<u8>, bool) {
    let head = repo.head_id().ok().map(|id| id.detach());
    let index_sig = file_sig(&repo.index_path());
    let exclude_sig = file_sig(&repo.common_dir().join("info").join("exclude"));
    let demand_pair = (demand.untracked, demand.ignored);
    if !*dirty
        && let Some(memo) = memo.as_ref()
        && memo.head == head
        && memo.index_sig == index_sig
        && memo.exclude_sig == exclude_sig
        && memo.demand == demand_pair
    {
        return (memo.records.clone(), memo.truncated);
    }
    *dirty = false;
    *status_recomputes()
        .lock()
        .unwrap()
        .entry(key.to_path_buf())
        .or_insert(0) += 1;
    let mut records = Vec::new();
    let mut flags = 0u8;
    crate::diffs::append_status_records(
        repo,
        demand.untracked,
        demand.ignored,
        budgets,
        caches,
        &mut records,
        &mut flags,
    );
    let truncated = flags & GIT_STATE_STATUS_TRUNCATED != 0;
    *memo = Some(StatusMemo {
        head,
        index_sig,
        exclude_sig,
        demand: demand_pair,
        records: records.clone(),
        truncated,
    });
    (records, truncated)
}

fn config_excludes_file(repo: &gix::Repository) -> Option<PathBuf> {
    repo.config_snapshot()
        .trusted_path("core.excludesFile")?
        .ok()
        .map(|path| path.into_owned())
}

// ---------------------------------------------------------------------------
// Record builders (shared by every subscriber's snapshot)
// ---------------------------------------------------------------------------

fn head_record(repo: &gix::Repository, records: &mut Vec<u8>) {
    let Ok(head) = repo.head() else {
        return;
    };
    let (head_flags, oid, name) = match head.kind {
        gix::head::Kind::Symbolic(reference) => {
            let name = crate::escape_bstr(reference.name.as_bstr());
            let oid = repo
                .head_id()
                .map(|id| oid_bytes(id.as_ref()))
                .unwrap_or(GIT_OID_NONE);
            (0, oid, name)
        }
        gix::head::Kind::Detached { target, .. } => {
            (GIT_HEAD_DETACHED, oid_bytes(target.as_ref()), String::new())
        }
        gix::head::Kind::Unborn(name) => (
            GIT_HEAD_UNBORN,
            GIT_OID_NONE,
            crate::escape_bstr(name.as_bstr()),
        ),
    };
    append_git_state_record(
        records,
        &GitStateRecord::Head {
            flags: head_flags,
            oid,
            name: &name,
        },
    );
}

/// All refs; returns false when the entry budget truncated the set.
fn refs_records(
    repo: &gix::Repository,
    entries_max: usize,
    records: &mut Vec<u8>,
    branches: &mut Vec<String>,
) -> bool {
    let Ok(platform) = repo.references() else {
        return true;
    };
    let Ok(iter) = platform.all() else {
        return true;
    };
    for (count, reference) in iter.flatten().enumerate() {
        if count >= entries_max {
            return false;
        }
        let name = crate::escape_bstr(reference.name().as_bstr());
        let mut ref_flags = 0u8;
        let mut reference = reference;
        let oid = match reference.target() {
            gix::refs::TargetRef::Object(id) => oid_bytes(id),
            gix::refs::TargetRef::Symbolic(_) => {
                ref_flags |= GIT_REF_SYMBOLIC;
                reference
                    .peel_to_id_in_place()
                    .map(|id| oid_bytes(id.as_ref()))
                    .unwrap_or(GIT_OID_NONE)
            }
        };
        // Annotated tags peel to their target commit.
        let mut peeled = GIT_OID_NONE;
        if name.starts_with("refs/tags/")
            && let Ok(id) = reference.peel_to_id_in_place()
        {
            let peeled_bytes = oid_bytes(id.as_ref());
            if peeled_bytes != oid {
                peeled = peeled_bytes;
                ref_flags |= GIT_REF_PEELED_VALID;
            }
        }
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            branches.push(branch.to_string());
        }
        append_git_state_record(
            records,
            &GitStateRecord::Ref {
                flags: ref_flags,
                oid,
                peeled,
                name: &name,
            },
        );
    }
    true
}

fn op_record(repo: &gix::Repository, records: &mut Vec<u8>) {
    use gix::state::InProgress;
    let Some(state) = repo.state() else {
        return;
    };
    let (op, head_file) = match state {
        InProgress::Merge => (GIT_OP_MERGE, Some("MERGE_HEAD")),
        InProgress::Rebase | InProgress::RebaseInteractive => (GIT_OP_REBASE, None),
        InProgress::CherryPick | InProgress::CherryPickSequence => {
            (GIT_OP_CHERRY_PICK, Some("CHERRY_PICK_HEAD"))
        }
        InProgress::Revert | InProgress::RevertSequence => (GIT_OP_REVERT, Some("REVERT_HEAD")),
        InProgress::Bisect => (GIT_OP_BISECT, Some("BISECT_EXPECTED_REV")),
        _ => return,
    };
    let oid = match (head_file, op) {
        // MERGE_HEAD can hold several oids (octopus); the op head is
        // the first, and special_ref_records streams them all.
        (Some(file), _) => read_git_file_oids(repo, file).into_iter().next(),
        // Rebase keeps its head under the rebase directory.
        (None, _) => ["rebase-merge/orig-head", "rebase-apply/orig-head"]
            .iter()
            .find_map(|f| read_git_file_oids(repo, f).into_iter().next()),
    };
    // Rebase progress as "step/total" (docs/design/git.md OP record):
    // rebase-merge counts in msgnum/end, rebase-apply in next/last.
    let detail = if op == GIT_OP_REBASE {
        let read_num = |name: &str| -> Option<u32> {
            let text = std::fs::read_to_string(repo.git_dir().join(name)).ok()?;
            text.trim().parse().ok()
        };
        [
            ("rebase-merge/msgnum", "rebase-merge/end"),
            ("rebase-apply/next", "rebase-apply/last"),
        ]
        .iter()
        .find_map(|(cur, total)| Some(format!("{}/{}", read_num(cur)?, read_num(total)?)))
        .unwrap_or_default()
    } else {
        String::new()
    };
    append_git_state_record(
        records,
        &GitStateRecord::Op {
            op,
            oid: oid.map(|id| oid_bytes(id.as_ref())).unwrap_or(GIT_OID_NONE),
            detail: &detail,
        },
    );
}

/// The in-progress operation's pseudo-refs — `MERGE_HEAD` (every line;
/// an octopus holds several), `CHERRY_PICK_HEAD`, `REVERT_HEAD`,
/// `REBASE_HEAD`, plus `ORIG_HEAD` only while an operation is live
/// (stale otherwise) — streamed as ordinary `STATE_REF` records
/// (docs/design/git.md). Their names carry no `refs/` prefix, which is
/// how clients tell them apart.
fn special_ref_records(repo: &gix::Repository, records: &mut Vec<u8>) {
    let mut emit = |name: &str, oid: [u8; 32]| {
        append_git_state_record(
            records,
            &GitStateRecord::Ref {
                flags: 0,
                oid,
                peeled: GIT_OID_NONE,
                name,
            },
        );
    };
    for file in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "REBASE_HEAD"] {
        if let Some(id) = read_git_file_oids(repo, file).into_iter().next() {
            emit(file, oid_bytes(id.as_ref()));
        }
    }
    for (n, id) in read_git_file_oids(repo, "MERGE_HEAD")
        .into_iter()
        .enumerate()
    {
        // Octopus extras get an informal suffix — the mirror's ref map
        // is keyed by name, and `MERGE_HEAD#2` reads honestly as a pill.
        let name = if n == 0 {
            "MERGE_HEAD".to_string()
        } else {
            format!("MERGE_HEAD#{}", n + 1)
        };
        emit(&name, oid_bytes(id.as_ref()));
    }
    if repo.state().is_some()
        && let Some(id) = read_git_file_oids(repo, "ORIG_HEAD").into_iter().next()
    {
        emit("ORIG_HEAD", oid_bytes(id.as_ref()));
    }
}

fn upstream_records(
    repo: &gix::Repository,
    walk_max: usize,
    memo: &mut HashMap<(gix::ObjectId, gix::ObjectId), (u8, u32, u32)>,
    branches: &[String],
    records: &mut Vec<u8>,
) {
    // Counts memoized by the immutable `(tip, upstream)` oid pair
    // (docs/design/git.md UPSTREAM): steady state costs nothing, and
    // rebuilding the map evicts pairs no branch references anymore.
    let mut next: HashMap<(gix::ObjectId, gix::ObjectId), (u8, u32, u32)> = Default::default();
    for branch in branches {
        let Some((upstream_name, upstream_id)) = upstream_of(repo, branch) else {
            continue;
        };
        let name = format!("refs/heads/{branch}");
        let Some(upstream_id) = upstream_id else {
            append_git_state_record(
                records,
                &GitStateRecord::Upstream {
                    flags: GIT_UPSTREAM_GONE,
                    ahead: 0,
                    behind: 0,
                    name: &name,
                    upstream: &upstream_name,
                },
            );
            continue;
        };
        let tip = repo
            .find_reference(&name)
            .ok()
            .and_then(|mut r| r.peel_to_id_in_place().ok().map(|id| id.detach()));
        let Some(tip) = tip else {
            continue;
        };
        let key = (tip, upstream_id);
        let (flags, ahead, behind) = match memo.get(&key).or_else(|| next.get(&key)) {
            Some(&counts) => counts,
            None => ahead_behind(repo, walk_max, tip, upstream_id),
        };
        next.insert(key, (flags, ahead, behind));
        append_git_state_record(
            records,
            &GitStateRecord::Upstream {
                flags,
                ahead,
                behind,
                name: &name,
                upstream: &upstream_name,
            },
        );
    }
    *memo = next;
}

fn stash_records(repo: &gix::Repository, entries_max: usize, records: &mut Vec<u8>) {
    let name: &gix::refs::FullNameRef = "refs/stash".try_into().expect("valid ref name");
    // The reverse reflog reader works through this window.
    let mut buf = vec![0u8; 64 * 1024];
    let Ok(Some(iter)) = repo.refs.reflog_iter_rev(name, &mut buf) else {
        return;
    };
    for (index, entry) in iter.flatten().enumerate() {
        if index >= entries_max {
            break;
        }
        let (msg, _) = crate::utf8_lossy_flag(entry.message.as_ref());
        let time = entry.signature.time;
        append_git_state_record(
            records,
            &GitStateRecord::Stash {
                index: index as u16,
                oid: oid_bytes(entry.new_oid.as_ref()),
                time: time.seconds,
                tz: (time.offset / 60) as i16,
                msg: &msg,
            },
        );
    }
}

/// Every oid in a gitdir-root file, one per line — `MERGE_HEAD` holds
/// several during an octopus merge. Empty when the file is absent or has
/// no parseable hash.
fn read_git_file_oids(repo: &gix::Repository, name: &str) -> Vec<gix::ObjectId> {
    let Ok(text) = std::fs::read_to_string(repo.git_dir().join(name)) else {
        return Vec::new();
    };
    text.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

/// Count `upstream..tip` and `tip..upstream`; `COUNTS_VALID` is withheld
/// past the walk budget. Callers memoize by the immutable oid pair.
fn ahead_behind(
    repo: &gix::Repository,
    walk_max: usize,
    tip: gix::ObjectId,
    upstream: gix::ObjectId,
) -> (u8, u32, u32) {
    let count = |from: gix::ObjectId, hide: gix::ObjectId| -> Option<u32> {
        let walk = repo.rev_walk([from]).with_hidden([hide]);
        let iter = walk.all().ok()?;
        let mut n = 0u32;
        for item in iter {
            item.ok()?;
            n += 1;
            if n as usize > walk_max {
                return None;
            }
        }
        Some(n)
    };
    match (count(tip, upstream), count(upstream, tip)) {
        (Some(ahead), Some(behind)) => (GIT_UPSTREAM_COUNTS_VALID, ahead, behind),
        _ => (0, 0, 0),
    }
}

/// The configured upstream of `branch`: `(escaped tracking ref name,
/// Some(tip) | None when the ref is gone)`. None when no upstream at all.
fn upstream_of(repo: &gix::Repository, branch: &str) -> Option<(String, Option<gix::ObjectId>)> {
    let full = format!("refs/heads/{branch}");
    let name: &gix::refs::FullNameRef = full.as_str().try_into().ok()?;
    let tracking = repo
        .branch_remote_tracking_ref_name(name, gix::remote::Direction::Fetch)?
        .ok()?;
    let escaped = crate::escape_bstr(tracking.as_bstr());
    match repo.find_reference(tracking.as_bstr()) {
        Ok(mut reference) => {
            let id = reference.peel_to_id_in_place().ok().map(|id| id.detach());
            Some((escaped, id))
        }
        Err(_) => Some((escaped, None)),
    }
}
