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
    GIT_OID_FORMAT_SHA1, GIT_OID_FORMAT_SHA256, GIT_REPO_BARE, GIT_REPO_LINKED, GIT_REPO_SHALLOW,
    GIT_REPO_SPARSE, GIT_STATUS_NOT_FOUND, GIT_STATUS_OTHER, GIT_STATUS_PERMISSION,
    GIT_STATUS_WRONG_TYPE, GitOid,
};

mod diffs;
mod requests;
mod state;

pub use state::{StateHandle, StateOptions};

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
            max_log_subs: env_u64("BLIT_GIT_MAX_LOG_SUBS", 64) as usize,
        }
    }
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
}

impl Clone for RepoHandle {
    fn clone(&self) -> Self {
        RepoHandle {
            shared: self.shared.clone(),
            budgets: self.budgets.clone(),
        }
    }
}

impl RepoHandle {
    pub(crate) fn local(&self) -> gix::Repository {
        self.shared.to_thread_local()
    }
}

/// Discover the repository containing `path` (standard upward discovery,
/// stopping at filesystem boundaries). Returns the handle plus the
/// `GIT_REPO` payload, or a unified-status code and diagnostic.
pub fn open(path: &str) -> Result<(RepoHandle, RepoInfo), (u8, String)> {
    if path.is_empty() || path.contains('\0') {
        return Err((GIT_STATUS_OTHER, "invalid path".into()));
    }
    let start = Path::new(path);
    if !start.exists() {
        return Err((GIT_STATUS_NOT_FOUND, "path not found".into()));
    }
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
    let info = RepoInfo {
        oid_format,
        flags,
        workdir,
        gitdir: blit_fssync::escape_path(&gitdir),
    };
    Ok((
        RepoHandle {
            shared: Arc::new(shared),
            budgets: Arc::new(Budgets::default()),
        },
        info,
    ))
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
