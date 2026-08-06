//! Git introspection engine (docs/git.md).
//!
//! The server side of `FEATURE_GIT`, split along the protocol's own grain:
//! a per-repo engine thread owns the mutable-state stream (`GIT_STATE`
//! snapshots: HEAD, refs, in-progress operation, upstream tracking, stash,
//! worktree status) with coalescing ack pacing, while object reads (log,
//! tree, blob, diff, patch, index, merge-base) are stateless request
//! handlers callable from any thread. Everything is built on gitoxide;
//! handlers return ready-to-send wire messages so the server only routes.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use blit_remote::git::{
    GIT_OID_FORMAT_SHA1, GIT_OID_FORMAT_SHA256, GIT_REPO_BARE, GIT_REPO_FETCHABLE, GIT_REPO_LINKED,
    GIT_REPO_SHALLOW, GIT_REPO_SPARSE, GIT_STATUS_INVALID, GIT_STATUS_NOT_FOUND,
    GIT_STATUS_PERMISSION, GIT_STATUS_WRONG_TYPE, GitOid,
};

mod diffs;
mod reads;
mod requests;
mod state;

pub use reads::{DiscoverLimits, discover, discover_within, open_submodule};
pub use state::{StateHandle, StateOptions};
#[doc(hidden)]
pub use state::{debug_engine_refs, debug_status_recomputes};

/// Messages ready for the client outbox. Returns `false` when the client
/// is gone; the engine then exits.
pub type Outbox = Box<dyn FnMut(Vec<u8>) -> bool + Send>;

/// Cooperative cancellation for one in-flight request (`GIT_CANCEL`).
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Environment-tunable budgets (docs/git.md limits table).
pub struct Budgets {
    pub blob_max: u64,
    pub log_default: usize,
    pub log_max: usize,
    pub entries_max: usize,
    pub walk_max: usize,
    pub bytes_max: usize,
    /// Directory entries scanned by the untracked walk before it bails with a
    /// partial view. A big worktree holds far more entries than final status
    /// records, so this is generous and independent of `entries_max`.
    pub untracked_scan_max: usize,
    /// Unmatched add/delete candidate pairs the similarity rename pass
    /// will consider before falling back to the exact-oid join and
    /// reporting `RENAME_LIMIT`. git's own guard, under its own name
    /// (`diff.renameLimit`), because the pass is quadratic.
    pub rename_limit: usize,
    /// Lines one `GIT_BLAME` response attributes before it truncates with a
    /// `CURSOR`. Blaming a viewport is cheap and blaming a 200 000-line file
    /// is not, so this bounds a response rather than the answer.
    pub blame_lines_max: u32,
    /// Concurrent `GIT_LOG_WATCH` subscriptions per repo. `log_id` is
    /// client-assigned (a u16), so the engine's subscription map is keyed by
    /// untrusted input; this bounds it. A handful of watched logs covers any
    /// real UI — the cap only stops a client exhausting memory with distinct
    /// ids.
    pub max_log_subs: usize,
}

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            blob_max: env_u64("BLIT_GIT_BLOB_MAX", 16 * 1024 * 1024),
            log_default: 256,
            log_max: env_u64("BLIT_GIT_LOG_MAX", 4096) as usize,
            entries_max: env_u64("BLIT_GIT_ENTRIES_MAX", 10_000) as usize,
            walk_max: env_u64("BLIT_GIT_WALK_MAX", 100_000) as usize,
            bytes_max: env_u64("BLIT_GIT_BYTES_MAX", 8 * 1024 * 1024) as usize,
            untracked_scan_max: env_u64("BLIT_GIT_UNTRACKED_SCAN_MAX", 1_000_000) as usize,
            rename_limit: env_u64("BLIT_GIT_RENAME_LIMIT", 1_000) as usize,
            blame_lines_max: env_u64("BLIT_GIT_BLAME_LINES_MAX", 50_000).max(1) as u32,
            max_log_subs: env_u64("BLIT_GIT_MAX_LOG_SUBS", 64) as usize,
        }
    }
}

pub(crate) fn env_u64_pub(name: &str, default: u64) -> u64 {
    env_u64(name, default)
}

pub(crate) fn env_usize(name: &str, default: usize) -> usize {
    env_u64(name, default as u64) as usize
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub(crate) fn env_latency(name: &str, default_ms: u64, max_ms: u64) -> Duration {
    Duration::from_millis(env_u64(name, default_ms).clamp(1, max_ms))
}

/// Everything `GIT_REPO` reports on success.
pub struct RepoInfo {
    pub oid_format: u8,
    pub flags: u8,
    /// Escaped canonical worktree root; empty for bare.
    pub workdir: String,
    /// Escaped canonical git directory.
    pub gitdir: String,
}

/// A discovered repository, cheaply sharable across threads. Each request
/// handler materializes its own thread-local `gix::Repository`.
pub struct RepoHandle {
    shared: Arc<gix::ThreadSafeRepository>,
    pub budgets: Arc<Budgets>,
    /// Merge bases memoized by oid pair, shared across every handler and
    /// the state engine (docs/design/git.md: memoize by oid pair).
    pub(crate) merge_memo: Arc<crate::requests::MergeMemo>,
    /// Object-cache size hint, computed once from the index.
    cache_hint: Arc<std::sync::OnceLock<usize>>,
    /// Canonical git directory — the cross-open sharing key: opens of one
    /// repo dedupe to one handle and attach to one state engine.
    pub(crate) gitdir: Arc<std::path::PathBuf>,
}

impl Clone for RepoHandle {
    fn clone(&self) -> Self {
        RepoHandle {
            shared: self.shared.clone(),
            budgets: self.budgets.clone(),
            merge_memo: self.merge_memo.clone(),
            cache_hint: self.cache_hint.clone(),
            gitdir: self.gitdir.clone(),
        }
    }
}

impl RepoHandle {
    pub(crate) fn local(&self) -> gix::Repository {
        self.sized(self.shared.to_thread_local())
    }

    /// Size `repo`'s object cache: gix leaves it unset, so tree-heavy
    /// handlers (flatten, log path filters, status) re-inflate the same
    /// delta chains on every access. Sized once per repo from the index —
    /// gix's own tree-diff sizing hint — with a floor for repos whose
    /// hint would be too small to hold real trees.
    pub(crate) fn sized(&self, mut repo: gix::Repository) -> gix::Repository {
        let bytes = *self.cache_hint.get_or_init(|| {
            repo.index_or_empty()
                .map(|index| repo.compute_object_cache_size_for_tree_diffs(&index))
                .unwrap_or(0)
                .max(4 * 1024 * 1024)
        });
        repo.object_cache_size_if_unset(bytes);
        repo
    }
}

/// Cross-open repository sharing (docs/design/git.md: "When several opens
/// share one engine"): handles dedupe by canonical gitdir, so N opens of
/// one repo share one `ThreadSafeRepository`, one merge-base memo, and
/// one object-cache hint — and `start_state` attaches them all to one
/// engine. Entries hold weak refs: the last close frees the repository.
struct SharedRepo {
    gitdir: Arc<std::path::PathBuf>,
    repo: std::sync::Weak<gix::ThreadSafeRepository>,
    budgets: std::sync::Weak<Budgets>,
    merge_memo: std::sync::Weak<crate::requests::MergeMemo>,
    cache_hint: std::sync::Weak<std::sync::OnceLock<usize>>,
}

impl SharedRepo {
    fn upgrade(&self) -> Option<RepoHandle> {
        Some(RepoHandle {
            shared: self.repo.upgrade()?,
            budgets: self.budgets.upgrade()?,
            merge_memo: self.merge_memo.upgrade()?,
            cache_hint: self.cache_hint.upgrade()?,
            gitdir: self.gitdir.clone(),
        })
    }
}

type RepoRegistry = std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, SharedRepo>>;

fn repo_registry() -> &'static RepoRegistry {
    static REPOS: std::sync::OnceLock<RepoRegistry> = std::sync::OnceLock::new();
    REPOS.get_or_init(Default::default)
}

/// Discover the repository containing `path` (standard upward discovery,
/// stopping at filesystem boundaries). Returns the handle plus the
/// `GIT_REPO` payload, or a common-status code and diagnostic.
pub fn open(path: &str) -> Result<(RepoHandle, RepoInfo), (u8, String)> {
    if path.is_empty() || path.contains('\0') {
        return Err((GIT_STATUS_INVALID, "invalid path".into()));
    }
    let start = Path::new(path);
    if !start.exists() {
        return Err((GIT_STATUS_NOT_FOUND, "path not found".into()));
    }
    // gix upward-discovery starts at a *directory*; given a file path it fails
    // with "not a directory". Callers legitimately pass a file (a diff/commit
    // tile opened for one file), so discover from the containing directory.
    let start = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(start)
    };
    let shared = gix::ThreadSafeRepository::discover(start).map_err(|e| {
        let msg = e.to_string();
        let status = if msg.contains("denied") {
            GIT_STATUS_PERMISSION
        } else {
            GIT_STATUS_WRONG_TYPE
        };
        (status, msg)
    })?;
    let repo = shared.to_thread_local();
    // This gix build only knows SHA-1; the wire format is ready for
    // SHA-256 repositories once gitoxide grows support.
    #[allow(unreachable_patterns)]
    let oid_format = match repo.object_hash() {
        gix::hash::Kind::Sha1 => GIT_OID_FORMAT_SHA1,
        _ => GIT_OID_FORMAT_SHA256,
    };
    let mut flags = 0u8;
    let workdir = match repo.workdir() {
        Some(dir) => blit_fssync::escape_path(&canonical(dir)),
        None => {
            flags |= GIT_REPO_BARE;
            String::new()
        }
    };
    let gitdir = canonical(repo.git_dir());
    if repo.is_shallow() {
        flags |= GIT_REPO_SHALLOW;
    }
    if canonical(repo.common_dir()) != gitdir {
        flags |= GIT_REPO_LINKED;
    }
    if repo
        .config_snapshot()
        .boolean("core.sparseCheckout")
        .unwrap_or(false)
    {
        flags |= GIT_REPO_SPARSE;
    }
    // Capability, answered per repository rather than per connection: a
    // box without `git` or with BLIT_GIT_FETCH=0 cannot fetch this repo
    // or any other, and a client should learn that at open.
    if crate::reads::fetch_available() {
        flags |= GIT_REPO_FETCHABLE;
    }
    let info = RepoInfo {
        oid_format,
        flags,
        workdir,
        gitdir: blit_fssync::escape_path(&gitdir),
    };
    // Dedupe through the registry: a repo already open elsewhere shares
    // its handle (and thus its engine); otherwise this discovery becomes
    // the shared one. Dead entries are swept on the way through.
    let mut reg = repo_registry().lock().unwrap();
    reg.retain(|_, entry| entry.repo.strong_count() > 0);
    let handle = match reg.get(&gitdir).and_then(SharedRepo::upgrade) {
        Some(handle) => handle,
        None => {
            let handle = RepoHandle {
                shared: Arc::new(shared),
                budgets: Arc::new(Budgets::default()),
                merge_memo: Arc::new(crate::requests::MergeMemo::default()),
                cache_hint: Arc::new(std::sync::OnceLock::new()),
                gitdir: Arc::new(gitdir.clone()),
            };
            reg.insert(
                gitdir,
                SharedRepo {
                    gitdir: handle.gitdir.clone(),
                    repo: Arc::downgrade(&handle.shared),
                    budgets: Arc::downgrade(&handle.budgets),
                    merge_memo: Arc::downgrade(&handle.merge_memo),
                    cache_hint: Arc::downgrade(&handle.cache_hint),
                },
            );
            handle
        }
    };
    Ok((handle, info))
}

fn canonical(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// Oid and text helpers shared by state and request code
// ---------------------------------------------------------------------------

pub(crate) fn oid_bytes(id: &gix::oid) -> GitOid {
    let mut out = [0u8; 32];
    let bytes = id.as_bytes();
    out[..bytes.len()].copy_from_slice(bytes);
    out
}

/// Reverse of [`oid_bytes`] for the repository's hash kind.
pub(crate) fn oid_from_wire(repo: &gix::Repository, oid: &GitOid) -> gix::ObjectId {
    #[allow(unreachable_patterns)]
    match repo.object_hash() {
        gix::hash::Kind::Sha1 => gix::ObjectId::from_bytes_or_panic(&oid[..20]),
        _ => gix::ObjectId::from_bytes_or_panic(&oid[..32]),
    }
}

pub(crate) fn is_zero_oid(oid: &GitOid) -> bool {
    oid.iter().all(|&b| b == 0)
}

/// Wire text for possibly non-UTF-8 repo bytes (paths, ref names): the
/// escaping scheme of docs/fs-watch.md, via the fssync helpers.
pub(crate) fn escape_bstr(bytes: &[u8]) -> String {
    blit_fssync::escape_bytes(bytes)
}

pub(crate) fn unescape_wire(s: &str) -> Option<Vec<u8>> {
    blit_fssync::unescape_to_bytes(s)
}

/// Lossy-flagged UTF-8 for names/emails/messages (docs/git.md: re-encoded
/// server-side, `LOSSY` when bytes were replaced).
pub(crate) fn utf8_lossy_flag(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

/// Re-encode commit text (names, emails, message) to UTF-8, honoring the
/// commit's `encoding` header (docs/git.md). A recognized non-UTF-8 label
/// is decoded through it; otherwise (absent, UTF-8, or unknown label) we
/// fall back to lossy UTF-8. The bool is the `LOSSY` flag.
pub(crate) fn commit_text(bytes: &[u8], encoding: Option<&[u8]>) -> (String, bool) {
    if let Some(label) = encoding
        && let Some(enc) = encoding_rs::Encoding::for_label(label)
        && enc != encoding_rs::UTF_8
    {
        let (text, _, had_errors) = enc.decode(bytes);
        return (text.into_owned(), had_errors);
    }
    utf8_lossy_flag(bytes)
}

#[cfg(test)]
mod tests {
    use super::Budgets;

    /// `BLIT_GIT_MAX_LOG_SUBS` overrides the default subscription cap; unset
    /// falls back to 64. This is the only unit test in this binary, so the
    /// process-global env mutation cannot race a parallel test.
    #[test]
    fn max_log_subs_is_env_configurable() {
        assert_eq!(Budgets::default().max_log_subs, 64);
        // SAFETY: single-threaded — no other test runs in this binary.
        unsafe { std::env::set_var("BLIT_GIT_MAX_LOG_SUBS", "3") };
        assert_eq!(Budgets::default().max_log_subs, 3);
        unsafe { std::env::remove_var("BLIT_GIT_MAX_LOG_SUBS") };
        assert_eq!(Budgets::default().max_log_subs, 64);
    }
}
