//! One language-server backend: child process, LSP session, open set,
//! diagnostics cache, and query translation — the engine thread + inbox
//! shape shared with fssync and git (docs/design/lsp.md "Server
//! implementation").
//!
//! The engine is the sole LSP client of its child: it owns `initialize`,
//! document synchronization from disk, and every server→client request.
//! Attachments observe through [`SharedInfo`] and a ping channel;
//! queries arrive with a reply sink and leave as ready-to-send wire
//! messages.

use std::collections::{HashMap, VecDeque};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blit_remote::lsp::{
    LSP_CAP_DEFINITION, LSP_CAP_DOC_SYMBOLS, LSP_CAP_HOVER, LSP_CAP_REFERENCES, LSP_CAP_RENAME,
    LSP_CAP_WS_SYMBOLS, LSP_PHASE_FAILED, LSP_PHASE_INDEXING, LSP_PHASE_INITIALIZING,
    LSP_PHASE_READY, LSP_PHASE_SPAWNING, LSP_PROGRESS_UNKNOWN, LSP_QUERY_DEFINITION,
    LSP_QUERY_DOC_SYMBOLS, LSP_QUERY_HOVER, LSP_QUERY_REFERENCES, LSP_QUERY_RENAME,
    LSP_QUERY_WS_SYMBOLS, LSP_REFS_INCLUDE_DECLARATION, LSP_RESP_TRUNCATED, LSP_STATUS_CANCELLED,
    LSP_STATUS_NOT_FOUND, LSP_STATUS_OK, LSP_STATUS_OTHER, LSP_STATUS_WARMING, LspHash,
    msg_lsp_query_resp,
};
use serde_json::{Value, json};

use crate::discovery::{ServerSpec, language_id};
use crate::rpc::{self, RpcMsg};
use crate::text::{self, PositionEncoding};
use crate::translate::{self, RecordSink, TextSource};
use crate::{Budgets, Sink};

/// A live child session's I/O halves.
pub struct SessionIo {
    pub writer: Box<dyn Write + Send>,
    pub reader: Box<dyn Read + Send>,
    pub child: Option<std::process::Child>,
}

/// Produces a fresh session on spawn and respawn. Production spawns the
/// discovery-table command; tests wire in-process pipes.
pub type Spawner = Box<dyn FnMut() -> std::io::Result<SessionIo> + Send>;

/// The default spawner: the spec's command, cwd at the workspace root,
/// stdio piped, stderr discarded.
pub fn command_spawner(spec: &ServerSpec, root: &Path) -> Spawner {
    let command = spec.command.clone();
    let root = root.to_path_buf();
    Box::new(move || {
        let mut child = std::process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(&root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let writer = Box::new(child.stdin.take().expect("piped stdin"));
        let reader = Box::new(child.stdout.take().expect("piped stdout"));
        Ok(SessionIo {
            writer,
            reader,
            child: Some(child),
        })
    })
}

/// One diagnostic already in wire form (byte columns).
#[derive(Clone, Debug)]
pub struct WireDiag {
    pub severity: u8,
    pub flags: u8,
    pub line: u32,
    pub col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub code: String,
    pub source: String,
    pub msg: String,
}

/// One file's cached diagnostic set. An empty `diags` is a tombstone —
/// the wire `n = 0` clear record.
#[derive(Clone, Debug)]
pub struct FileDiags {
    /// Monotonic per-backend change sequence, for attachment cursors.
    pub seq: u64,
    pub hash: LspHash,
    pub diags: Vec<WireDiag>,
}

/// Projected backend state, as the `SERVER` record reports it.
#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub phase: u8,
    pub progress_pct: u8,
    pub caps: u32,
    pub epoch: u32,
    pub refused_edits: u32,
    pub msg: String,
    pub pid: Option<u32>,
}

/// State attachments read directly; the engine only writes.
pub struct SharedInfo {
    pub info: Mutex<ServerInfo>,
    /// Bumped on any `info` change.
    pub state_seq: AtomicU64,
    /// Terminal: the backend was stopped (LSP_STOP or idle sweep) and
    /// its engine thread is gone for good. Attachments drop its record
    /// and respawn on the next query; a transient crash does NOT set
    /// this (the engine restarts itself).
    pub gone: std::sync::atomic::AtomicBool,
    /// Keyed by absolute path.
    pub diags: Mutex<HashMap<PathBuf, FileDiags>>,
    /// The latest `FileDiags::seq` issued.
    pub diag_seq: AtomicU64,
    pub subs: AtomicUsize,
    pub last_detach: Mutex<Instant>,
}

pub(crate) enum Cmd {
    Attach {
        sub: u64,
        ping: Sender<crate::attach::AttCmd>,
    },
    Detach {
        sub: u64,
    },
    Query {
        sub: u64,
        nonce: u16,
        kind: u8,
        flags: u8,
        line: u32,
        col: u32,
        /// Absolute; `None` for `WS_SYMBOLS`.
        path: Option<PathBuf>,
        arg: String,
        /// The attachment root wire paths relativize against.
        wire_root: PathBuf,
        sink: Sink,
    },
    Cancel {
        sub: u64,
        nonce: u16,
    },
    Dirty(Vec<PathBuf>),
    Rpc(u64, RpcMsg),
    ChildGone(u64),
    Stop,
}

/// A shared, daemon-owned language-server backend.
pub struct Backend {
    pub server_ref: u16,
    pub id: String,
    pub root: PathBuf,
    /// File extensions this backend answers for (query routing).
    pub extensions: Vec<String>,
    pub shared: Arc<SharedInfo>,
    inbox: Sender<Cmd>,
}

impl Backend {
    pub(crate) fn start(
        server_ref: u16,
        spec: ServerSpec,
        root: PathBuf,
        spawner: Spawner,
        budgets: Budgets,
    ) -> Arc<Backend> {
        let shared = Arc::new(SharedInfo {
            info: Mutex::new(ServerInfo {
                phase: LSP_PHASE_SPAWNING,
                progress_pct: LSP_PROGRESS_UNKNOWN,
                caps: 0,
                epoch: 0,
                refused_edits: 0,
                msg: String::new(),
                pid: None,
            }),
            state_seq: AtomicU64::new(1),
            gone: std::sync::atomic::AtomicBool::new(false),
            diags: Mutex::new(HashMap::new()),
            diag_seq: AtomicU64::new(0),
            subs: AtomicUsize::new(0),
            last_detach: Mutex::new(Instant::now()),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let backend = Arc::new(Backend {
            server_ref,
            id: spec.id.clone(),
            root: root.clone(),
            extensions: spec.extensions.clone(),
            shared: shared.clone(),
            inbox: tx.clone(),
        });
        let engine = Engine {
            spec,
            root,
            shared,
            inbox: rx,
            inbox_tx: tx,
            spawner,
            budgets,
            io_tx: None,
            pending_reopen: Vec::new(),
            child: None,
            session_gen: 0,
            next_id: 0,
            init_id: None,
            pending: HashMap::new(),
            subs: HashMap::new(),
            open_docs: HashMap::new(),
            open_order: VecDeque::new(),
            enc: PositionEncoding::Utf16,
            initialized: false,
            progress: HashMap::new(),
            status_seen: false,
            quiesce_at: None,
            restarts: VecDeque::new(),
            respawn_at: None,
            dirty: HashMap::new(),
            dirty_deadline: None,
            watcher: None,
        };
        std::thread::Builder::new()
            .name(format!("blit-lsp-{}", backend.id))
            .spawn(move || engine.run())
            .expect("spawn lsp engine thread");
        backend
    }

    pub(crate) fn send(&self, cmd: Cmd) -> bool {
        if self.shared.gone.load(Ordering::Relaxed) {
            return false;
        }
        self.inbox.send(cmd).is_ok()
    }

    /// True once the engine has terminally stopped (LSP_STOP / sweep).
    pub fn is_gone(&self) -> bool {
        self.shared.gone.load(Ordering::Relaxed)
    }

    /// Coarse capability bits (`LSP_CAP_*`) the backend advertised; `0`
    /// until it finishes `initialize`.
    pub fn caps(&self) -> u32 {
        self.shared.info.lock().unwrap().caps
    }

    /// Current lifecycle phase (`LSP_PHASE_*`).
    pub fn phase(&self) -> u8 {
        self.shared.info.lock().unwrap().phase
    }

    /// Best-effort resident set size of the child, in bytes.
    pub fn rss_bytes(&self) -> u64 {
        let pid = self.shared.info.lock().unwrap().pid;
        pid.map(rss_of_pid).unwrap_or(0)
    }
}

fn caps_bits(capabilities: &Value) -> u32 {
    let mut caps = 0;
    let has = |v: &Value| !(v.is_null() || v.as_bool() == Some(false));
    if has(&capabilities["definitionProvider"]) {
        caps |= LSP_CAP_DEFINITION;
    }
    if has(&capabilities["referencesProvider"]) {
        caps |= LSP_CAP_REFERENCES;
    }
    if has(&capabilities["hoverProvider"]) {
        caps |= LSP_CAP_HOVER;
    }
    if has(&capabilities["documentSymbolProvider"]) {
        caps |= LSP_CAP_DOC_SYMBOLS;
    }
    if has(&capabilities["workspaceSymbolProvider"]) {
        caps |= LSP_CAP_WS_SYMBOLS;
    }
    if has(&capabilities["renameProvider"]) {
        caps |= LSP_CAP_RENAME;
    }
    caps
}

struct PendingQuery {
    sub: u64,
    nonce: u16,
    kind: u8,
    path: Option<PathBuf>,
    wire_root: PathBuf,
    sink: Sink,
}

enum PendingCtx {
    Init,
    Query(PendingQuery),
}

struct Pending {
    deadline: Instant,
    ctx: PendingCtx,
}

struct Engine {
    spec: ServerSpec,
    root: PathBuf,
    shared: Arc<SharedInfo>,
    inbox: Receiver<Cmd>,
    inbox_tx: Sender<Cmd>,
    spawner: Spawner,
    budgets: Budgets,
    /// Sends framed bytes to the dedicated writer thread, so the
    /// engine never blocks on a wedged child's stdin.
    io_tx: Option<Sender<Vec<u8>>>,
    /// Open-set paths to replay once a respawned session is READY.
    pending_reopen: Vec<PathBuf>,
    child: Option<std::process::Child>,
    /// Bumped per (re)spawn so a stale reader thread's `ChildGone`
    /// cannot kill a fresh session.
    session_gen: u64,
    next_id: i64,
    init_id: Option<i64>,
    pending: HashMap<i64, Pending>,
    subs: HashMap<u64, Sender<crate::attach::AttCmd>>,
    /// Absolute path → (version, text, hash); the exact text the
    /// backend holds.
    open_docs: HashMap<PathBuf, (i64, String, LspHash)>,
    open_order: VecDeque<PathBuf>,
    enc: PositionEncoding,
    /// The session finished the `initialize` handshake — notifications
    /// are legal from here on, independent of the reported phase.
    initialized: bool,
    /// Active `$/progress` tokens → last percentage.
    progress: HashMap<String, Option<u8>>,
    /// The session sent `experimental/serverStatus` at least once;
    /// from then on quiescence is its call, not the progress-idle
    /// heuristic (docs/design/lsp.md "Sessions and discovery").
    status_seen: bool,
    /// When the progress-idle grace window ends and INDEXING may
    /// become READY. Armed after `initialized` and whenever the last
    /// progress token ends; disarmed by new progress or serverStatus.
    quiesce_at: Option<Instant>,
    restarts: VecDeque<Instant>,
    respawn_at: Option<Instant>,
    dirty: HashMap<PathBuf, ()>,
    dirty_deadline: Option<Instant>,
    watcher: Option<notify::RecommendedWatcher>,
}

impl Engine {
    fn run(mut self) {
        self.start_session();
        self.start_watcher();
        loop {
            match self.inbox.recv_timeout(Duration::from_millis(100)) {
                Ok(Cmd::Stop) => break,
                Ok(cmd) => {
                    if !self.handle(cmd) {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // Drain whatever is queued before doing timed work.
            loop {
                match self.inbox.try_recv() {
                    Ok(Cmd::Stop) => return self.shutdown_child(),
                    Ok(cmd) => {
                        if !self.handle(cmd) {
                            return self.shutdown_child();
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return self.shutdown_child(),
                }
            }
            self.expire_pending();
            self.flush_dirty();
            self.maybe_ready();
            self.maybe_respawn();
        }
        self.shutdown_child();
    }

    // -- session lifecycle ------------------------------------------------

    fn start_session(&mut self) {
        self.session_gen += 1;
        self.initialized = false;
        self.status_seen = false;
        self.quiesce_at = None;
        let session_gen = self.session_gen;
        match (self.spawner)() {
            Ok(io) => {
                let pid = io.child.as_ref().map(|c| c.id());
                self.child = io.child;
                // Writer thread: owns the child's stdin, pulls framed
                // bytes off a channel. A child that stops reading blocks
                // this thread, never the engine loop.
                let (io_tx, io_rx) = std::sync::mpsc::channel::<Vec<u8>>();
                let mut writer = io.writer;
                let wtx = self.inbox_tx.clone();
                std::thread::Builder::new()
                    .name("blit-lsp-write".into())
                    .spawn(move || {
                        while let Ok(bytes) = io_rx.recv() {
                            use std::io::Write as _;
                            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                                let _ = wtx.send(Cmd::ChildGone(session_gen));
                                return;
                            }
                        }
                    })
                    .expect("spawn lsp writer thread");
                self.io_tx = Some(io_tx);
                let tx = self.inbox_tx.clone();
                let reader = io.reader;
                std::thread::Builder::new()
                    .name("blit-lsp-read".into())
                    .spawn(move || {
                        let mut reader = BufReader::new(reader);
                        while let Some(msg) = rpc::read_msg(&mut reader) {
                            if tx.send(Cmd::Rpc(session_gen, msg)).is_err() {
                                return;
                            }
                        }
                        let _ = tx.send(Cmd::ChildGone(session_gen));
                    })
                    .expect("spawn lsp reader thread");
                self.set_info(|info| {
                    info.phase = LSP_PHASE_INITIALIZING;
                    info.pid = pid;
                    info.msg.clear();
                });
                self.send_initialize();
            }
            Err(e) => {
                self.set_info(|info| {
                    info.phase = LSP_PHASE_FAILED;
                    info.msg = format!("spawn failed: {e}");
                });
                // A spawn failure mid-restart-chain is transient; keep
                // the chain alive under the same backoff/budget as a
                // crash rather than dead-ending in FAILED.
                self.schedule_respawn();
            }
        }
    }

    /// Prune the restart window and schedule a backoff respawn if under
    /// budget. Shared by crash, spawn-failure, and init-timeout paths.
    fn schedule_respawn(&mut self) {
        let now = Instant::now();
        while let Some(front) = self.restarts.front()
            && now.duration_since(*front) > Duration::from_secs(3600)
        {
            self.restarts.pop_front();
        }
        if self.restarts.len() < self.budgets.max_restarts {
            let backoff = Duration::from_secs(1 << self.restarts.len().min(6));
            self.restarts.push_back(now);
            self.respawn_at = Some(now + backoff);
        }
    }

    fn send_initialize(&mut self) {
        let root_uri = text::path_to_uri(&self.root);
        let name = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".into());
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "blit", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": root_uri,
            "workspaceFolders": [ { "uri": root_uri, "name": name } ],
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                    "didChangeWatchedFiles": { "dynamicRegistration": true },
                    "symbol": {},
                    "applyEdit": true,
                },
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "publishDiagnostics": { "tagSupport": { "valueSet": [1, 2] } },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "rename": {},
                },
                "window": { "workDoneProgress": true },
                // rust-analyzer's quiescence signal
                // (experimental/serverStatus); explicit readiness
                // beats the progress-idle grace heuristic.
                "experimental": { "serverStatusNotification": true },
            },
            "initializationOptions": self.spec.init.clone().unwrap_or(Value::Null),
        });
        let id = self.next_request_id();
        self.init_id = Some(id);
        self.pending.insert(
            id,
            Pending {
                deadline: Instant::now() + self.budgets.init_timeout,
                ctx: PendingCtx::Init,
            },
        );
        self.write(rpc::request(id, "initialize", params));
    }

    fn on_initialized(&mut self, result: &Value) {
        let capabilities = &result["capabilities"];
        if let Some(label) = capabilities["positionEncoding"].as_str()
            && let Some(enc) = PositionEncoding::from_label(label)
        {
            self.enc = enc;
        }
        let caps = caps_bits(capabilities);
        self.write(rpc::notification("initialized", json!({})));
        if let Some(settings) = self.spec.settings.clone() {
            self.write(rpc::notification(
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            ));
        }
        // Not READY yet: most servers (rust-analyzer, gopls) answer
        // `initialize` in milliseconds and start indexing *after*, with
        // the first `$/progress` trailing the handshake. Report
        // INDEXING and let quiescence — a progress-idle grace window,
        // or serverStatus — promote to READY, so `blit lsp wait` never
        // returns inside that gap.
        self.initialized = true;
        self.quiesce_at = Some(Instant::now() + self.budgets.ready_grace);
        self.set_info(|info| {
            info.caps = caps;
            info.phase = LSP_PHASE_INDEXING;
        });
        // Now that the handshake is done, replay the open set a respawn
        // deferred (notifications before `initialized` are illegal).
        for path in std::mem::take(&mut self.pending_reopen) {
            self.ensure_open(&path);
        }
    }

    /// Promote INDEXING to READY once the progress-idle grace window
    /// has run out — the quiescence heuristic for servers without an
    /// explicit signal.
    fn maybe_ready(&mut self) {
        let Some(at) = self.quiesce_at else { return };
        if self.status_seen || !self.progress.is_empty() {
            self.quiesce_at = None;
            return;
        }
        if Instant::now() < at {
            return;
        }
        self.quiesce_at = None;
        self.set_info(|info| {
            if info.phase == LSP_PHASE_INDEXING {
                info.phase = LSP_PHASE_READY;
                info.msg.clear();
            }
        });
    }

    fn start_watcher(&mut self) {
        use notify::Watcher;
        let tx = self.inbox_tx.clone();
        let root = self.root.clone();
        let watcher = notify::recommended_watcher(move |event: Result<notify::Event, _>| {
            if let Ok(event) = event {
                let paths: Vec<PathBuf> = event
                    .paths
                    .into_iter()
                    // The `.git` subtree churns constantly and no
                    // language server wants it.
                    .filter(|p| !p.strip_prefix(&root).is_ok_and(|r| r.starts_with(".git")))
                    .collect();
                if !paths.is_empty() {
                    let _ = tx.send(Cmd::Dirty(paths));
                }
            }
        });
        if let Ok(mut watcher) = watcher
            && watcher
                .watch(&self.root, notify::RecursiveMode::Recursive)
                .is_ok()
        {
            self.watcher = Some(watcher);
        }
    }

    fn shutdown_child(&mut self) {
        // Terminal: mark gone so attachments drop this backend's record
        // and route later queries to a respawn (docs/design/lsp.md
        // LSP_STOP), then wake every subscriber once so the drop is
        // seen even though the engine loop is ending.
        self.shared.gone.store(true, Ordering::Relaxed);
        self.shared.state_seq.fetch_add(1, Ordering::Relaxed);
        for ping in self.subs.values() {
            let _ = ping.send(crate::attach::AttCmd::Ping);
        }
        // Every in-flight or still-queued query must get its one
        // response — an unanswered nonce pins the connection's
        // in-flight budget forever.
        self.answer_all_queries();
        // Graceful: shutdown request, exit notification, then kill.
        // Send directly through a taken sender so a failed write does
        // not re-enter on_child_gone; dropping it ends the writer.
        if let Some(tx) = self.io_tx.take() {
            let id = self.next_request_id();
            let _ = tx.send(rpc::frame(&rpc::request(id, "shutdown", Value::Null)));
            let _ = tx.send(rpc::frame(&rpc::notification("exit", Value::Null)));
        }
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                // The daemon's global reaper may win the race and steal
                // the status; any error here means the child is gone.
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
            reap_backstop_status(pid);
        }
    }

    /// Answer every in-flight and still-queued query with a terminal
    /// status so no nonce is left pending when the engine stops.
    fn answer_all_queries(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (_, p) in pending {
            if let PendingCtx::Query(q) = p.ctx {
                respond(&q, LSP_STATUS_OTHER, 0, "", &[]);
            }
        }
        while let Ok(cmd) = self.inbox.try_recv() {
            if let Cmd::Query { sink, nonce, .. } = cmd {
                let _ = sink(msg_lsp_query_resp(nonce, LSP_STATUS_OTHER, 0, "", &[]));
            }
        }
    }

    fn on_child_gone(&mut self) {
        if self.io_tx.is_none() && self.child.is_none() {
            return; // already handling
        }
        self.io_tx = None;
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            // Never leak a still-running child: escalate to kill.
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
            reap_backstop_status(pid);
        }
        // Every in-flight request dies with the session.
        let pending = std::mem::take(&mut self.pending);
        for (_, p) in pending {
            if let PendingCtx::Query(q) = p.ctx {
                respond(&q, LSP_STATUS_OTHER, 0, "", &[]);
            }
        }
        self.init_id = None;
        self.initialized = false;
        self.progress.clear();
        self.status_seen = false;
        self.quiesce_at = None;
        self.set_info(|info| {
            info.phase = LSP_PHASE_FAILED;
            info.pid = None;
            if info.msg.is_empty() {
                info.msg = "server exited".into();
            }
        });
        self.schedule_respawn();
    }

    fn maybe_respawn(&mut self) {
        if let Some(at) = self.respawn_at
            && Instant::now() >= at
        {
            self.respawn_at = None;
            // The fresh server has no documents open, and LSP forbids
            // notifications before `initialized`. Remember what to
            // reopen and replay it in on_initialized once READY.
            self.pending_reopen = self.open_order.iter().cloned().collect();
            self.open_docs.clear();
            self.open_order.clear();
            self.start_session();
        }
    }

    // -- command handling -------------------------------------------------

    fn handle(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Attach { sub, ping } => {
                self.subs.insert(sub, ping);
                self.shared.subs.store(self.subs.len(), Ordering::Relaxed);
            }
            Cmd::Detach { sub } => {
                self.subs.remove(&sub);
                self.shared.subs.store(self.subs.len(), Ordering::Relaxed);
                *self.shared.last_detach.lock().unwrap() = Instant::now();
            }
            Cmd::Query {
                sub,
                nonce,
                kind,
                flags,
                line,
                col,
                path,
                arg,
                wire_root,
                sink,
            } => self.handle_query(
                sub, nonce, kind, flags, line, col, path, arg, wire_root, sink,
            ),
            Cmd::Cancel { sub, nonce } => {
                let id = self.pending.iter().find_map(|(id, p)| match &p.ctx {
                    PendingCtx::Query(q) if q.sub == sub && q.nonce == nonce => Some(*id),
                    _ => None,
                });
                if let Some(id) = id {
                    self.write(rpc::notification("$/cancelRequest", json!({ "id": id })));
                }
            }
            Cmd::Dirty(paths) => {
                for path in paths {
                    self.dirty.insert(path, ());
                }
                self.dirty_deadline
                    .get_or_insert(Instant::now() + Duration::from_millis(200));
            }
            // A dead session's reader can race a respawn; its traffic
            // must never touch the fresh session.
            Cmd::Rpc(gen_, msg) if gen_ == self.session_gen => self.handle_rpc(msg),
            Cmd::Rpc(..) => {}
            Cmd::ChildGone(gen_) if gen_ == self.session_gen => self.on_child_gone(),
            Cmd::ChildGone(_) => {}
            Cmd::Stop => return false,
        }
        true
    }

    fn handle_rpc(&mut self, msg: RpcMsg) {
        match msg {
            RpcMsg::Response { id, result, error } => {
                let Some(id) = id.as_i64() else { return };
                let Some(pending) = self.pending.remove(&id) else {
                    return;
                };
                match pending.ctx {
                    PendingCtx::Init => match (result, &error) {
                        (Some(result), None) => self.on_initialized(&result),
                        _ => {
                            self.set_info(|info| {
                                info.phase = LSP_PHASE_FAILED;
                                info.msg = format!("initialize failed: {error:?}");
                            });
                        }
                    },
                    PendingCtx::Query(q) => self.finish_query(q, result, error),
                }
            }
            RpcMsg::Request { id, method, params } => {
                self.handle_server_request(id, &method, params)
            }
            RpcMsg::Notification { method, params } => {
                self.handle_server_notification(&method, params)
            }
        }
    }

    fn handle_server_request(&mut self, id: Value, method: &str, params: Value) {
        let reply = match method {
            "workspace/configuration" => {
                let n = params["items"].as_array().map(|a| a.len()).unwrap_or(0);
                let settings = self.spec.settings.clone().unwrap_or(Value::Null);
                // Section-blind: every requested item gets the whole
                // verbatim settings value (blit never interprets it).
                rpc::response(&id, Value::Array(vec![settings; n]))
            }
            "client/registerCapability" | "client/unregisterCapability" => {
                self.set_info(|info| info.epoch += 1);
                rpc::response(&id, Value::Null)
            }
            "window/workDoneProgress/create" => {
                if let Some(token) = token_key(&params["token"]) {
                    self.progress.insert(token, None);
                }
                rpc::response(&id, Value::Null)
            }
            "workspace/applyEdit" => {
                self.set_info(|info| info.refused_edits += 1);
                rpc::response(&id, json!({ "applied": false }))
            }
            "workspace/workspaceFolders" => {
                let uri = text::path_to_uri(&self.root);
                let name = self
                    .root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                rpc::response(&id, json!([ { "uri": uri, "name": name } ]))
            }
            "window/showMessageRequest" => {
                let message = params["message"].as_str().unwrap_or_default().to_string();
                self.set_info(|info| info.msg = message);
                rpc::response(&id, Value::Null)
            }
            _ => rpc::error_response(&id, -32601, "method not found"),
        };
        self.write(reply);
    }

    fn handle_server_notification(&mut self, method: &str, params: Value) {
        match method {
            "textDocument/publishDiagnostics" => self.on_publish_diagnostics(params),
            "$/progress" => self.on_progress(params),
            "experimental/serverStatus" => self.on_server_status(params),
            "window/showMessage" => {
                let message = params["message"].as_str().unwrap_or_default().to_string();
                self.set_info(|info| info.msg = message);
            }
            _ => {}
        }
    }

    fn on_progress(&mut self, params: Value) {
        let Some(token) = token_key(&params["token"]) else {
            return;
        };
        let value = &params["value"];
        // Once serverStatus speaks, progress only feeds pct/msg; phase
        // is the status notification's call.
        let heuristic = !self.status_seen;
        match value["kind"].as_str() {
            Some("begin") | Some("report") => {
                let pct = value["percentage"].as_u64().map(|p| p.min(100) as u8);
                self.progress.insert(token, pct);
                if heuristic {
                    self.quiesce_at = None;
                }
                let msg = value["title"]
                    .as_str()
                    .or_else(|| value["message"].as_str())
                    .map(str::to_string);
                let overall = self.overall_progress();
                self.set_info(|info| {
                    if heuristic && info.phase == LSP_PHASE_READY {
                        info.phase = LSP_PHASE_INDEXING;
                    }
                    info.progress_pct = overall;
                    if let Some(msg) = msg {
                        info.msg = msg;
                    }
                });
            }
            Some("end") => {
                self.progress.remove(&token);
                // Progress-idle is necessary but not sufficient:
                // servers pause between warmup stages (rust-analyzer's
                // metadata → crate graph → indexing), so READY waits
                // out the grace window in maybe_ready.
                if heuristic && self.progress.is_empty() && self.initialized {
                    self.quiesce_at = Some(Instant::now() + self.budgets.ready_grace);
                }
                let overall = self.overall_progress();
                self.set_info(|info| {
                    info.progress_pct = overall;
                });
            }
            _ => {}
        }
    }

    /// rust-analyzer's explicit quiescence signal. Authoritative for
    /// phase from the first notification on: `quiescent` decides
    /// READY/INDEXING with no grace window.
    fn on_server_status(&mut self, params: Value) {
        self.status_seen = true;
        self.quiesce_at = None;
        let quiescent = params["quiescent"].as_bool().unwrap_or(false);
        let healthy = matches!(params["health"].as_str(), Some("ok") | None);
        let message = params["message"].as_str().map(str::to_string);
        self.set_info(|info| {
            if info.phase == LSP_PHASE_INDEXING || info.phase == LSP_PHASE_READY {
                info.phase = if quiescent {
                    LSP_PHASE_READY
                } else {
                    LSP_PHASE_INDEXING
                };
            }
            match message {
                Some(m) => info.msg = m,
                None if quiescent && healthy => info.msg.clear(),
                None => {}
            }
        });
    }

    fn overall_progress(&self) -> u8 {
        let mut sum = 0u32;
        let mut n = 0u32;
        for pct in self.progress.values().flatten() {
            sum += u32::from(*pct);
            n += 1;
        }
        match sum.checked_div(n) {
            Some(avg) => avg as u8,
            None => LSP_PROGRESS_UNKNOWN,
        }
    }

    fn on_publish_diagnostics(&mut self, params: Value) {
        let Some(uri) = params["uri"].as_str() else {
            return;
        };
        let Some(path) = text::uri_to_path(uri) else {
            return;
        };
        // Transcode against the text the server diagnosed. Prefer the
        // open-doc text, but only when its version matches the publish:
        // a publish for an older version was computed against text we no
        // longer hold, so transcoding against current text would place
        // diagnostics wrong and stamp a false content hash. In that case
        // fall back to disk with an unknown hash (the server re-publishes
        // for the current version after our didChange).
        let publish_version = params["version"].as_i64();
        let looked: Option<(String, LspHash)> = match self.open_docs.get(&path) {
            Some((doc_version, text, hash))
                if publish_version.is_none_or(|v| v == *doc_version) =>
            {
                Some((text.clone(), *hash))
            }
            _ => std::fs::read(&path).ok().and_then(|bytes| {
                let hash = text::hash_bytes(&bytes);
                String::from_utf8(bytes).ok().map(|t| (t, hash))
            }),
        };
        // When versions disagree, the true content is unknown to us.
        let stale = self
            .open_docs
            .get(&path)
            .zip(publish_version)
            .is_some_and(|((v, ..), pv)| pv != *v);
        let empty = Vec::new();
        let items = params["diagnostics"].as_array().unwrap_or(&empty);
        let mut diags = Vec::with_capacity(items.len());
        for item in items {
            let range = &item["range"];
            let wr = match &looked {
                Some((txt, _)) => translate::range_to_wire(range, txt, self.enc),
                None => translate::raw_range(range),
            };
            let mut flags = 0u8;
            if let Some(tags) = item["tags"].as_array() {
                for tag in tags {
                    match tag.as_u64() {
                        Some(1) => flags |= blit_remote::lsp::LSP_DIAG_UNNECESSARY,
                        Some(2) => flags |= blit_remote::lsp::LSP_DIAG_DEPRECATED,
                        _ => {}
                    }
                }
            }
            let code = match &item["code"] {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => String::new(),
            };
            diags.push(WireDiag {
                severity: item["severity"].as_u64().unwrap_or(1) as u8,
                flags,
                line: wr.line,
                col: wr.col,
                end_line: wr.end_line,
                end_col: wr.end_col,
                code,
                source: item["source"].as_str().unwrap_or(&self.spec.id).to_string(),
                msg: item["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        let seq = self.shared.diag_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let hash = if stale {
            blit_remote::lsp::LSP_HASH_NONE
        } else {
            looked
                .map(|(_, h)| h)
                .unwrap_or(blit_remote::lsp::LSP_HASH_NONE)
        };
        self.shared
            .diags
            .lock()
            .unwrap()
            .insert(path, FileDiags { seq, hash, diags });
        self.ping_subs();
    }

    // -- open set ---------------------------------------------------------

    /// `didOpen` a file (disk bytes) if not already open, LRU-evicting
    /// past the cap. Returns `false` when the file is unreadable.
    fn ensure_open(&mut self, path: &Path) -> bool {
        if self.open_docs.contains_key(path) {
            // Refresh LRU order.
            if let Some(pos) = self.open_order.iter().position(|p| p == path) {
                self.open_order.remove(pos);
                self.open_order.push_back(path.to_path_buf());
            }
            return true;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let hash = text::hash_bytes(&bytes);
        let Ok(body) = String::from_utf8(bytes) else {
            return false;
        };
        self.write(rpc::notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": text::path_to_uri(path),
                    "languageId": language_id(path),
                    "version": 1,
                    "text": body,
                }
            }),
        ));
        self.open_docs.insert(path.to_path_buf(), (1, body, hash));
        self.open_order.push_back(path.to_path_buf());
        while self.open_docs.len() > self.budgets.max_docs {
            let Some(evict) = self.open_order.pop_front() else {
                break;
            };
            self.open_docs.remove(&evict);
            self.write(rpc::notification(
                "textDocument/didClose",
                json!({ "textDocument": { "uri": text::path_to_uri(&evict) } }),
            ));
        }
        true
    }

    fn flush_dirty(&mut self) {
        let Some(deadline) = self.dirty_deadline else {
            return;
        };
        if Instant::now() < deadline {
            return;
        }
        // No notifications before the handshake — LSP forbids any
        // (except exit) before `initialized`. Hints keep accumulating
        // in self.dirty and flush once the session is up; INDEXING is
        // fine (a didChange mid-index is legal and keeps the server
        // current).
        if !self.initialized {
            return;
        }
        self.dirty_deadline = None;
        let dirty: Vec<PathBuf> = self.dirty.drain().map(|(p, ())| p).collect();
        let mut events = Vec::with_capacity(dirty.len());
        for path in dirty {
            let exists = path.exists();
            events.push(json!({
                "uri": text::path_to_uri(&path),
                "type": if exists { 2 } else { 3 },
            }));
            if let Some((version, _, _)) = self.open_docs.get(&path) {
                let version = version + 1;
                if exists {
                    if let Ok(bytes) = std::fs::read(&path) {
                        let hash = text::hash_bytes(&bytes);
                        if let Ok(body) = String::from_utf8(bytes) {
                            self.write(rpc::notification(
                                "textDocument/didChange",
                                json!({
                                    "textDocument": {
                                        "uri": text::path_to_uri(&path),
                                        "version": version,
                                    },
                                    "contentChanges": [ { "text": body } ],
                                }),
                            ));
                            self.open_docs.insert(path.clone(), (version, body, hash));
                        }
                    }
                } else {
                    self.write(rpc::notification(
                        "textDocument/didClose",
                        json!({ "textDocument": { "uri": text::path_to_uri(&path) } }),
                    ));
                    self.open_docs.remove(&path);
                    self.open_order.retain(|p| p != &path);
                }
            }
        }
        self.write(rpc::notification(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": events }),
        ));
    }

    // -- queries ----------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn handle_query(
        &mut self,
        sub: u64,
        nonce: u16,
        kind: u8,
        flags: u8,
        line: u32,
        col: u32,
        path: Option<PathBuf>,
        arg: String,
        wire_root: PathBuf,
        sink: Sink,
    ) {
        let phase = self.shared.info.lock().unwrap().phase;
        let q = PendingQuery {
            sub,
            nonce,
            kind,
            path: path.clone(),
            wire_root,
            sink,
        };
        match phase {
            LSP_PHASE_SPAWNING | LSP_PHASE_INITIALIZING => {
                return respond(&q, LSP_STATUS_WARMING, 0, "", &[]);
            }
            LSP_PHASE_FAILED => return respond(&q, LSP_STATUS_OTHER, 0, "", &[]),
            _ => {}
        }
        let (method, params) = match kind {
            LSP_QUERY_WS_SYMBOLS => ("workspace/symbol", json!({ "query": arg })),
            _ => {
                let Some(path) = &path else {
                    return respond(&q, blit_remote::lsp::LSP_STATUS_INVALID, 0, "", &[]);
                };
                if !self.ensure_open(path) {
                    return respond(&q, LSP_STATUS_NOT_FOUND, 0, "", &[]);
                }
                let (_, txt, _) = &self.open_docs[path];
                let character = text::col_to_encoding(txt, line, col, self.enc);
                let doc = json!({ "uri": text::path_to_uri(path) });
                let position = json!({ "line": line, "character": character });
                match kind {
                    LSP_QUERY_DEFINITION => (
                        "textDocument/definition",
                        json!({ "textDocument": doc, "position": position }),
                    ),
                    LSP_QUERY_REFERENCES => (
                        "textDocument/references",
                        json!({
                            "textDocument": doc,
                            "position": position,
                            "context": {
                                "includeDeclaration": flags & LSP_REFS_INCLUDE_DECLARATION != 0
                            },
                        }),
                    ),
                    LSP_QUERY_HOVER => (
                        "textDocument/hover",
                        json!({ "textDocument": doc, "position": position }),
                    ),
                    LSP_QUERY_DOC_SYMBOLS => (
                        "textDocument/documentSymbol",
                        json!({ "textDocument": doc }),
                    ),
                    LSP_QUERY_RENAME => (
                        "textDocument/rename",
                        json!({ "textDocument": doc, "position": position, "newName": arg }),
                    ),
                    _ => return respond(&q, blit_remote::lsp::LSP_STATUS_INVALID, 0, "", &[]),
                }
            }
        };
        let id = self.next_request_id();
        self.pending.insert(
            id,
            Pending {
                deadline: Instant::now() + self.budgets.query_timeout,
                ctx: PendingCtx::Query(q),
            },
        );
        self.write(rpc::request(id, method, params));
    }

    fn finish_query(&mut self, q: PendingQuery, result: Option<Value>, error: Option<Value>) {
        if let Some(error) = error {
            // A query dispatched while the backend is still warming up
            // (many servers accept requests during indexing, then reject
            // them until the project finishes loading) reports the
            // retryable WARMING, not a bare OTHER "error" — so a client
            // retries or runs `blit lsp wait` instead of seeing a
            // meaningless failure.
            let phase = self.shared.info.lock().unwrap().phase;
            let warming = matches!(
                phase,
                LSP_PHASE_SPAWNING | LSP_PHASE_INITIALIZING | LSP_PHASE_INDEXING
            );
            let status = match error["code"].as_i64() {
                Some(-32800) => LSP_STATUS_CANCELLED, // RequestCancelled
                Some(-32002) => LSP_STATUS_WARMING,   // ServerNotInitialized
                Some(-32801) => LSP_STATUS_WARMING,   // ContentModified — retryable
                _ if warming => LSP_STATUS_WARMING,
                _ => LSP_STATUS_OTHER,
            };
            // Carry the server's own message to the client so a failed
            // query reads as "server X: <reason>", not a bare "error".
            let detail = if status == LSP_STATUS_OTHER {
                let msg = error["message"].as_str().unwrap_or("no message");
                format!("{}: {msg}", self.spec.id)
            } else {
                String::new()
            };
            return respond(&q, status, 0, &detail, &[]);
        }
        let result = result.unwrap_or(Value::Null);
        if result.is_null() {
            return respond(&q, LSP_STATUS_NOT_FOUND, 0, "", &[]);
        }
        // Borrow the open set in (text, hash) view for translation.
        let open_view: HashMap<PathBuf, (String, LspHash)> = self
            .open_docs
            .iter()
            .map(|(p, (_, t, h))| (p.clone(), (t.clone(), *h)))
            .collect();
        let mut src = TextSource::new(&open_view);
        let mut buf = Vec::new();
        let mut sink = RecordSink {
            buf: &mut buf,
            entries_left: self.budgets.entries_max,
            bytes_max: self.budgets.bytes_max,
            truncated: false,
            incomplete: false,
        };
        match q.kind {
            LSP_QUERY_DEFINITION | LSP_QUERY_REFERENCES => {
                translate::locations(&mut sink, &mut src, &q.wire_root, &result, self.enc)
            }
            LSP_QUERY_HOVER => {
                let path = q.path.clone().unwrap_or_default();
                translate::hover(&mut sink, &mut src, &q.wire_root, &path, &result, self.enc)
            }
            LSP_QUERY_DOC_SYMBOLS => {
                let path = q.path.clone().unwrap_or_default();
                translate::doc_symbols(&mut sink, &mut src, &q.wire_root, &path, &result, self.enc)
            }
            LSP_QUERY_WS_SYMBOLS => {
                translate::ws_symbols(&mut sink, &mut src, &q.wire_root, &result, self.enc)
            }
            LSP_QUERY_RENAME => {
                translate::rename_edits(&mut sink, &mut src, &q.wire_root, &result, self.enc)
            }
            _ => {}
        }
        let mut flags = 0;
        if sink.truncated {
            flags |= LSP_RESP_TRUNCATED;
        }
        if sink.incomplete {
            flags |= blit_remote::lsp::LSP_RESP_INCOMPLETE;
        }
        respond(&q, LSP_STATUS_OK, flags, "", &buf);
    }

    // -- plumbing ---------------------------------------------------------

    fn next_request_id(&mut self) -> i64 {
        self.next_id += 1;
        self.next_id
    }

    fn write(&mut self, payload: Value) {
        // Hand the framed bytes to the writer thread; a closed channel
        // means that thread is gone, i.e. the child died.
        let failed = self
            .io_tx
            .as_ref()
            .is_some_and(|tx| tx.send(rpc::frame(&payload)).is_err());
        if failed {
            self.on_child_gone();
        }
    }

    fn set_info(&mut self, f: impl FnOnce(&mut ServerInfo)) {
        f(&mut self.shared.info.lock().unwrap());
        self.shared.state_seq.fetch_add(1, Ordering::Relaxed);
        self.ping_subs();
    }

    fn ping_subs(&mut self) {
        self.subs
            .retain(|_, ping| ping.send(crate::attach::AttCmd::Ping).is_ok());
        self.shared.subs.store(self.subs.len(), Ordering::Relaxed);
    }

    fn expire_pending(&mut self) {
        let now = Instant::now();
        let expired: Vec<i64> = self
            .pending
            .iter()
            .filter(|(_, p)| now >= p.deadline)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let Some(pending) = self.pending.remove(&id) else {
                continue;
            };
            match pending.ctx {
                PendingCtx::Init => {
                    self.set_info(|info| {
                        info.phase = LSP_PHASE_FAILED;
                        info.msg = "initialize timed out".into();
                    });
                    // A server wedged in initialize must be killed and
                    // restarted, not left running while pinning a slot.
                    self.on_child_gone();
                }
                PendingCtx::Query(q) => {
                    self.write(rpc::notification("$/cancelRequest", json!({ "id": id })));
                    respond(&q, LSP_STATUS_OTHER, 0, "", &[]);
                }
            }
        }
    }
}

/// Reap our own child so the daemon's global `waitpid(-1)` backstop
/// does not have to. We `wait()` before this on every path; the
/// backstop only parks statuses for PTY-owned pids (see
/// `blit-server` `reap_zombies`), so an LSP child leaves nothing behind
/// to collide with a later recycled pid (docs/design/lsp.md § Server
/// implementation). This hook stays for symmetry and future backends.
fn reap_backstop_status(_pid: u32) {}

fn respond(q: &PendingQuery, status: u8, flags: u8, detail: &str, records: &[u8]) {
    debug_assert!(status != LSP_STATUS_OK || records.is_empty() || !records.is_empty());
    let _ = (q.sink)(msg_lsp_query_resp(q.nonce, status, flags, detail, records));
}

fn token_key(token: &Value) -> Option<String> {
    match token {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Best-effort resident set size of `pid`, in bytes; 0 when unknown.
#[cfg(target_os = "linux")]
fn rss_of_pid(pid: u32) -> u64 {
    let Ok(statm) = std::fs::read_to_string(format!("/proc/{pid}/statm")) else {
        return 0;
    };
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|f| f.parse().ok())
        .unwrap_or(0);
    pages * unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

#[cfg(target_os = "macos")]
fn rss_of_pid(pid: u32) -> u64 {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    let got = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if got == size {
        info.pti_resident_size
    } else {
        0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rss_of_pid(_pid: u32) -> u64 {
    0
}
