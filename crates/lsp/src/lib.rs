//! Language intelligence engine (docs/design/lsp.md).
//!
//! The server side of `FEATURE_LSP`: warm language-server backends,
//! daemon-owned and keyed by `(canonical_root, server_id)`, shared by
//! every attachment and surviving client disconnects — the PTY model,
//! not the fs/git model. Each backend is the *sole LSP client* of its
//! child process; blit terminates the protocol and projects records.
//! Handlers return ready-to-send wire messages so the server only
//! routes.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use blit_remote::lsp::{
    LSP_STATUS_BUDGET, LSP_STATUS_NOT_FOUND, LSP_STATUS_OK, LSP_STATUS_OTHER, LspServersRecord,
    append_lsp_servers_record, msg_lsp_servers_resp, msg_lsp_stopped,
};

mod attach;
mod backend;
pub mod discovery;
mod rpc;
mod text;
mod translate;

#[cfg(test)]
mod tests;

pub use attach::Attachment;
pub use backend::{Backend, SessionIo, Spawner};
pub use text::hash_bytes;

/// Serialized wire messages ready for a client outbox. Returns `false`
/// when the client is gone; senders then stop.
pub type Sink = Arc<dyn Fn(Vec<u8>) -> bool + Send + Sync>;

/// Environment-tunable budgets (docs/design/lsp.md limits table).
#[derive(Clone)]
pub struct Budgets {
    pub max_servers: usize,
    pub max_docs: usize,
    pub query_timeout: Duration,
    pub init_timeout: Duration,
    pub idle: Duration,
    pub entries_max: usize,
    pub bytes_max: usize,
    pub max_restarts: usize,
    pub spawn_rate_per_min: usize,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            max_servers: env_u64("BLIT_LSP_MAX_SERVERS", 4).max(1) as usize,
            // At least 1: the open set must hold the document a query is
            // about, or ensure_open would open-then-evict it and the
            // query would index a missing key.
            max_docs: env_u64("BLIT_LSP_MAX_DOCS", 128).max(1) as usize,
            query_timeout: Duration::from_millis(env_u64("BLIT_LSP_TIMEOUT_MS", 30_000)),
            init_timeout: Duration::from_secs(env_u64("BLIT_LSP_INIT_TIMEOUT", 60)),
            idle: Duration::from_secs(env_u64("BLIT_LSP_IDLE_SECS", 900)),
            entries_max: env_u64("BLIT_LSP_ENTRIES_MAX", 10_000) as usize,
            bytes_max: env_u64("BLIT_LSP_BYTES_MAX", 8 * 1024 * 1024) as usize,
            max_restarts: env_u64("BLIT_LSP_MAX_RESTARTS", 3) as usize,
            spawn_rate_per_min: env_u64("BLIT_LSP_SPAWN_RATE", 30) as usize,
        }
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

struct Registry {
    backends: HashMap<(PathBuf, String), Arc<Backend>>,
    next_ref: u16,
    spawns: VecDeque<Instant>,
    sweeper: bool,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            backends: HashMap::new(),
            next_ref: 1,
            spawns: VecDeque::new(),
            sweeper: false,
        })
    })
}

/// A discovered-and-spawned workspace, ready to attach. Two phases so
/// the server can send `LSP_OPENED` into its FIFO outbox *before* the
/// pacing thread's first `LSP_STATE` (the GIT_REPO-before-GIT_STATE
/// discipline).
pub struct Prepared {
    root: PathBuf,
    backends: Vec<Arc<Backend>>,
    /// Per-backend `(spec, root)`, so an attachment can respawn a
    /// backend a later `LSP_STOP` or sweep killed (docs/design/lsp.md:
    /// "a later query respawns it").
    specs: Vec<(discovery::ServerSpec, PathBuf)>,
    budgets: Budgets,
}

impl Prepared {
    pub fn attach(self, lsp_id: u16, flags: u8, diag_latency_ms: u16, sink: Sink) -> Attachment {
        Attachment::start(
            lsp_id,
            self.root,
            self.backends,
            self.specs,
            flags,
            diag_latency_ms,
            sink,
            &self.budgets,
        )
    }
}

/// Re-resolve a backend by `(spec, root)` for an attachment whose cached
/// handle went `gone` — the respawn a later query triggers. Cheap when
/// the backend is already live in the registry.
pub(crate) fn reacquire(
    spec: &discovery::ServerSpec,
    root: &Path,
    budgets: &Budgets,
) -> Option<Arc<Backend>> {
    let spawner = backend::command_spawner(spec, root);
    get_or_spawn(spec.clone(), root.to_path_buf(), spawner, budgets).ok()
}

/// Discovery and lazy backend spawn for `path` (docs/design/lsp.md
/// `LSP_OPEN`). On success returns the `Prepared` workspace, the escaped
/// attachment root, and a detail string naming any matched-but-absent
/// servers (empty when all matched markers had their binary on PATH) —
/// so a client "learns what to install" at open time.
pub fn prepare(path: &str) -> Result<(Prepared, String, String), (u8, String)> {
    if path.is_empty() || path.contains('\0') {
        return Err((LSP_STATUS_OTHER, "invalid path".into()));
    }
    let start = Path::new(path);
    if !start.exists() {
        return Err((LSP_STATUS_NOT_FOUND, "path not found".into()));
    }
    let start = start
        .canonicalize()
        .map_err(|e| (LSP_STATUS_OTHER, e.to_string()))?;
    let (found, root) = discovery::discover(&start);
    if found.is_empty() {
        return Err((
            LSP_STATUS_NOT_FOUND,
            format!("no known project markers under {}", root.display()),
        ));
    }
    let missing: Vec<String> = found
        .iter()
        .filter(|d| !d.on_path)
        .map(|d| format!("{}: not found on PATH", d.spec.command[0]))
        .collect();
    let budgets = Budgets::default();
    let mut backends = Vec::new();
    let mut specs = Vec::new();
    for discovered in found.into_iter().filter(|d| d.on_path) {
        let spawner = backend::command_spawner(&discovered.spec, &discovered.root);
        backends.push(get_or_spawn(
            discovered.spec.clone(),
            discovered.root.clone(),
            spawner,
            &budgets,
        )?);
        specs.push((discovered.spec, discovered.root));
    }
    if backends.is_empty() {
        return Err((LSP_STATUS_NOT_FOUND, missing.join(", ")));
    }
    let escaped = blit_fssync::escape_path(&root);
    Ok((
        Prepared {
            root,
            backends,
            specs,
            budgets,
        },
        escaped,
        missing.join(", "),
    ))
}

/// Join a live backend or spawn one, under the server and spawn-rate
/// budgets. Detail strings name the limit for `LSP_OPENED`.
fn get_or_spawn(
    spec: discovery::ServerSpec,
    root: PathBuf,
    spawner: Spawner,
    budgets: &Budgets,
) -> Result<Arc<Backend>, (u8, String)> {
    let mut reg = registry().lock().unwrap();
    let key = (root.clone(), spec.id.clone());
    if let Some(backend) = reg.backends.get(&key) {
        // Refresh the idle clock under the registry lock so a sweeper
        // tick cannot stop this backend between here and the caller's
        // Cmd::Attach (TOCTOU): any later sweep sees a recent
        // last_detach and skips it.
        *backend.shared.last_detach.lock().unwrap() = Instant::now();
        return Ok(backend.clone());
    }
    if reg.backends.len() >= budgets.max_servers {
        return Err((
            LSP_STATUS_BUDGET,
            format!("server limit reached ({})", budgets.max_servers),
        ));
    }
    let now = Instant::now();
    while let Some(front) = reg.spawns.front()
        && now.duration_since(*front) > Duration::from_secs(60)
    {
        reg.spawns.pop_front();
    }
    if reg.spawns.len() >= budgets.spawn_rate_per_min {
        return Err((LSP_STATUS_BUDGET, "spawn rate limit reached".into()));
    }
    reg.spawns.push_back(now);
    let server_ref = reg.next_ref;
    reg.next_ref = reg.next_ref.wrapping_add(1).max(1);
    let backend = Backend::start(server_ref, spec, root, spawner, budgets.clone());
    reg.backends.insert(key, backend.clone());
    if !reg.sweeper {
        reg.sweeper = true;
        let idle = budgets.idle;
        std::thread::Builder::new()
            .name("blit-lsp-sweep".into())
            .spawn(move || sweep(idle))
            .expect("spawn lsp sweeper thread");
    }
    Ok(backend)
}

/// Idle shutdown (docs/design/lsp.md "Sessions and discovery"): a
/// backend with zero attachments past the idle window is shut down —
/// the deliberate third lifecycle between fssync's drop-on-last-ref and
/// the PTY's explicit close.
fn sweep(idle: Duration) {
    loop {
        std::thread::sleep(Duration::from_secs(15));
        let mut reg = registry().lock().unwrap();
        let expired: Vec<(PathBuf, String)> = reg
            .backends
            .iter()
            .filter(|(_, b)| {
                b.shared.subs.load(std::sync::atomic::Ordering::Relaxed) == 0
                    && b.shared.last_detach.lock().unwrap().elapsed() > idle
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some(backend) = reg.backends.remove(&key) {
                backend.send(backend::Cmd::Stop);
            }
        }
    }
}

/// Build the `LSP_SERVERS` response: every live backend, daemon-wide.
pub fn servers_response(nonce: u16) -> Vec<u8> {
    let reg = registry().lock().unwrap();
    let mut records = Vec::new();
    let mut backends: Vec<&Arc<Backend>> = reg.backends.values().collect();
    backends.sort_by_key(|b| b.server_ref);
    for backend in backends {
        let info = backend.shared.info.lock().unwrap().clone();
        append_lsp_servers_record(
            &mut records,
            &LspServersRecord::Server {
                server_ref: backend.server_ref,
                phase: info.phase,
                progress_pct: info.progress_pct,
                caps: info.caps,
                epoch: info.epoch,
                refused_edits: info.refused_edits,
                rss: backend.rss_bytes(),
                id: &backend.id,
                msg: &info.msg,
                root: &blit_fssync::escape_path(&backend.root),
            },
        );
    }
    msg_lsp_servers_resp(nonce, LSP_STATUS_OK, 0, &records)
}

/// Shut one backend down by `server_ref` (docs/design/lsp.md
/// `LSP_STOP`); a later open or query respawns it.
pub fn stop_response(nonce: u16, server_ref: u16) -> Vec<u8> {
    let mut reg = registry().lock().unwrap();
    let key = reg
        .backends
        .iter()
        .find(|(_, b)| b.server_ref == server_ref)
        .map(|(k, _)| k.clone());
    match key {
        Some(key) => {
            if let Some(backend) = reg.backends.remove(&key) {
                backend.send(backend::Cmd::Stop);
            }
            msg_lsp_stopped(nonce, LSP_STATUS_OK)
        }
        None => msg_lsp_stopped(nonce, LSP_STATUS_NOT_FOUND),
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// Spawn a backend over in-process pipes; the fake server runs
    /// `serve` on its ends in a thread.
    pub fn pipe_backend(
        spec: discovery::ServerSpec,
        root: PathBuf,
        budgets: Budgets,
        serve: impl FnMut(
            std::io::BufReader<Box<dyn std::io::Read + Send>>,
            Box<dyn std::io::Write + Send>,
        ) + Send
        + Clone
        + 'static,
    ) -> Arc<Backend> {
        let spawner: Spawner = Box::new(move || {
            let (their_stdin_r, our_stdin_w) = std::io::pipe()?;
            let (our_stdout_r, their_stdout_w) = std::io::pipe()?;
            let mut serve = serve.clone();
            std::thread::spawn(move || {
                let reader: Box<dyn std::io::Read + Send> = Box::new(their_stdin_r);
                serve(
                    std::io::BufReader::new(reader),
                    Box::new(their_stdout_w) as Box<dyn std::io::Write + Send>,
                );
            });
            Ok(SessionIo {
                writer: Box::new(our_stdin_w),
                reader: Box::new(our_stdout_r),
                child: None,
            })
        });
        Backend::start(1, spec, root, spawner, budgets)
    }
}
