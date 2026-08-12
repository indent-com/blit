//! Native watch backend via the `notify` crate (inotify on Linux, FSEvents
//! on macOS, `ReadDirectoryChangesW` on Windows), demoted to a dirty-set
//! hint source: every event becomes `Hint::Dirty(path)` and every
//! loss signal — overflow, rescan flag, backend error — degrades to
//! `Hint::Rescan`. No backend behavior is client-visible; the engine
//! verifies everything against the filesystem before emitting.

use crate::{BackendHandle, Hint, HintSender};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Keeps the native watch alive; dropping it unwatches.
pub struct WatchBackend {
    /// Owned by the shared-root handle. The reconciler only receives a
    /// weak registration handle: the watcher callback owns a channel
    /// sender, so giving the receiver thread a strong clone would make the
    /// root keep itself alive after its last client disappeared.
    pub watches: Arc<Watches>,
}

impl WatchBackend {
    /// A non-owning registration handle for the reconciler.
    ///
    /// `RecommendedWatcher` owns the event callback, and that callback owns
    /// a sender for the reconciler's inbox. The reconciler owns the matching
    /// receiver. A strong `Arc<Watches>` here therefore forms
    /// receiver -> watcher -> sender -> receiver and prevents both the
    /// watcher and its worker threads from ever being released.
    pub fn registrar(&self) -> Box<dyn BackendHandle> {
        Box::new(WeakWatches(Arc::downgrade(&self.watches)))
    }
}

struct WeakWatches(std::sync::Weak<Watches>);

/// Whether per-directory arming is worth it for a filtered root.
///
/// Only on inotify, where a recursive watch is really one descriptor per
/// directory and skipping `node_modules` is the whole point. FSEvents
/// covers a tree with a single stream and `ReadDirectoryChangesW` with a
/// single handle, so there per-directory arming would cost *more* objects,
/// not fewer — and an unfiltered root keeps the recursive watch on every
/// platform, so nothing changes for a sync that excludes nothing.
pub fn per_dir_watching_pays(recursive: bool, single: bool, filtered: bool) -> bool {
    cfg!(target_os = "linux") && recursive && !single && filtered
}

/// The native watches a root holds, and the reconciler's handle on them.
///
/// Two kinds, tracked apart because they are retired by different rules:
///
/// - **tree** directories, armed one at a time when `per_dir` is set. A
///   recursive inotify watch is a descriptor per directory whether or not
///   the sync mirrors it, so a filtered sync of a checkout would still pay
///   for `node_modules` and `target` — the cost the exclusion exists to
///   avoid, and on a big tree the one that hits
///   `fs.inotify.max_user_watches`. Arming per directory puts the
///   reconciler in charge: it arms exactly what it indexes, in the order it
///   indexes it, and never reaches the excluded subtrees. The
///   arm-before-list contract holds one level down — a directory is armed
///   before it is read — and these are disarmed as the index drops them.
/// - **outside** directories, holding ignore sources above the root. No
///   hint from inside the tree could ever report those, so without them a
///   parent `.gitignore` edit is invisible for the life of the sync. They
///   are armed once and never retired, since nothing in the index tracks
///   them.
pub struct Watches {
    watcher: Mutex<RecommendedWatcher>,
    /// Whether the tree is watched a directory at a time. When false, one
    /// recursive watch on the root already covers it and the tree-side
    /// calls are no-ops.
    per_dir: bool,
    /// Tree directories currently armed, so re-arming is a set lookup
    /// rather than a syscall and teardown knows what to unwatch.
    ///
    /// Ordered, because disarming is always a *subtree*: `Path`'s
    /// component-wise ordering puts a directory's descendants immediately
    /// after it and before any sibling, so a range query costs the size of
    /// the subtree rather than the size of the tree. With a hash set,
    /// deleting one directory scanned every armed path — and `rm -rf` on a
    /// deep tree paid that per directory it removed.
    armed: Mutex<std::collections::BTreeSet<PathBuf>>,
    /// Directories outside the tree, exempt from every retirement rule.
    outside: Mutex<std::collections::BTreeSet<PathBuf>>,
}

impl Watches {
    /// Arm a tree directory, non-recursively. `false` only when the
    /// process is out of watch descriptors — the caller closes the root,
    /// because a mirror with an unwatched directory in it is silently
    /// stale. Every other failure (the directory vanished mid-scan,
    /// permission) returns `true`: there is nothing to watch, and nothing
    /// to mirror either.
    pub fn add_dir(&self, dir: &Path) -> bool {
        if !self.per_dir {
            return true; // the recursive watch on the root covers it
        }
        if self.armed.lock().unwrap().contains(dir) {
            return true;
        }
        match self.arm(dir) {
            Ok(()) => {
                self.armed.lock().unwrap().insert(dir.to_path_buf());
                true
            }
            Err(e) => !is_watch_exhaustion(&e),
        }
    }

    /// Arm a directory outside the tree because it holds an ignore source.
    /// Failure is not fatal the way a tree directory's is: the rules were
    /// already read, and the only loss is noticing a later edit — the
    /// behavior every sync had before these were watched at all.
    pub fn watch_outside(&self, dir: &Path) {
        if self.outside.lock().unwrap().contains(dir) {
            return;
        }
        if self.arm(dir).is_ok() {
            self.outside.lock().unwrap().insert(dir.to_path_buf());
        }
    }

    /// Whether the tree is watched a directory at a time rather than by
    /// one recursive watch on the root.
    pub fn is_per_dir(&self) -> bool {
        self.per_dir
    }

    fn arm(&self, dir: &Path) -> notify::Result<()> {
        self.watcher
            .lock()
            .unwrap()
            .watch(dir, RecursiveMode::NonRecursive)
    }

    /// Disarm `dir` and everything under it — a deleted or newly excluded
    /// subtree. inotify drops the kernel watch on deletion by itself; this
    /// is what keeps the bookkeeping (and notify's own map) from growing
    /// across a create/delete cycle.
    pub fn remove_dir(&self, dir: &Path) {
        let gone: Vec<PathBuf> = {
            let armed = self.armed.lock().unwrap();
            armed
                .range(dir.to_path_buf()..)
                .take_while(|p| p.starts_with(dir))
                .cloned()
                .collect()
        };
        self.drop_watches(gone);
    }

    /// Drop every armed tree directory `keep` rejects. Used after a full
    /// rescan, which replaces the index wholesale and so cannot report
    /// individual removals — the one place a whole-set pass is the right
    /// shape, since every entry has to be reconsidered anyway.
    pub fn retain_dirs(&self, keep: &dyn Fn(&Path) -> bool) {
        let gone: Vec<PathBuf> = {
            let armed = self.armed.lock().unwrap();
            armed.iter().filter(|p| !keep(p)).cloned().collect()
        };
        self.drop_watches(gone);
    }

    fn drop_watches(&self, gone: Vec<PathBuf>) {
        if gone.is_empty() {
            return;
        }
        let mut watcher = self.watcher.lock().unwrap();
        let mut armed = self.armed.lock().unwrap();
        for dir in gone {
            // Already gone from the kernel's side once the directory was
            // deleted; unwatch is how notify forgets it too.
            let _ = watcher.unwatch(&dir);
            armed.remove(&dir);
        }
    }
}

impl BackendHandle for WeakWatches {
    fn add_dir(&self, dir: &Path) -> bool {
        self.0.upgrade().is_some_and(|watches| watches.add_dir(dir))
    }
    fn watch_outside(&self, dir: &Path) {
        if let Some(watches) = self.0.upgrade() {
            watches.watch_outside(dir);
        }
    }
    fn remove_dir(&self, dir: &Path) {
        if let Some(watches) = self.0.upgrade() {
            watches.remove_dir(dir);
        }
    }
    fn retain_dirs(&self, keep: &dyn Fn(&Path) -> bool) {
        if let Some(watches) = self.0.upgrade() {
            watches.retain_dirs(keep);
        }
    }
}

/// Whether arming failed because the process is out of watch descriptors,
/// as opposed to the path being gone or unreadable. `ENOSPC` is what
/// `inotify_add_watch` returns at `max_user_watches`.
fn is_watch_exhaustion(err: &notify::Error) -> bool {
    match &err.kind {
        notify::ErrorKind::MaxFilesWatch => true,
        notify::ErrorKind::Io(e) => matches!(e.raw_os_error(), Some(23) | Some(24) | Some(28)),
        _ => false,
    }
}

/// Whether an event reports only that something was *read*.
///
/// notify's inotify mask includes `IN_OPEN` (notify 8.2 `src/inotify.rs`),
/// so on Linux every open of a watched file arrives as an event. Any
/// watcher that reads inside its own watched tree — this engine hashing a
/// file, the git engine opening `.gitignore` and `HEAD` to recompute
/// status, an LSP backend reading a document — then retriggers itself, and
/// the settle window becomes a spin loop rather than a debounce. macOS and
/// Windows have no notion of a read event, so dropping these costs nothing
/// and is not a platform-specific behavior difference: it removes one.
///
/// Only the unambiguous reads go. `Close(Write)` is inotify's
/// `IN_CLOSE_WRITE` — the end of a *writing* session — and an unspecified
/// `Access(Any)` could be either, so both stay: an extra pass is always
/// cheaper than a lost change.
pub fn is_read_only_event(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};
    matches!(
        kind,
        notify::EventKind::Access(
            AccessKind::Read | AccessKind::Open(_) | AccessKind::Close(AccessMode::Read)
        )
    )
}

/// Build the platform watcher every blit watch goes through.
///
/// Identical to `notify::recommended_watcher` but for one setting:
/// `Config::default()` turns symlink following *on*, and notify's inotify
/// backend re-`WalkDir`s a subtree on every `IN_CREATE`/`IN_MOVED_TO` that
/// carries `ISDIR` (notify 8.2 `src/inotify.rs`). A recursive watch on a
/// worktree that contains a pnpm `node_modules` — where every package is a
/// symlink into `.pnpm/`, so the same real directories are reachable under
/// many paths — or a `.direnv` linking into the nix store therefore walks a
/// tree several times its real size, and re-walks it per directory created
/// anywhere inside. Measured on this repo: 9.7k real directories, 92k when
/// following, and four such event loops pinned four cores indefinitely.
///
/// Cost is not the whole argument, because not following also changes *what*
/// is covered. A recursive sync enumerates a symlinked directory under the
/// link's own path (`docs/design/fs-watch.md` § Links), and notify's walk
/// yields a symlink as a symlink when it is not following, so `filter_dir`
/// drops it and no descriptor covers those aliased paths: an edit under one
/// is hinted at the target's real path — which the sync sees only when that
/// target is itself inside the root — and never at the alias.
///
/// Following did not reliably cover them either. `inotify_add_watch` returns
/// the *same* descriptor for an inode already watched, and notify keys its
/// descriptor→path map on that descriptor, so arming both a pnpm alias and
/// its real path left whichever the walk reached last reporting for both, and
/// unwatching either dropped both. The choice is therefore between one stable
/// rule and an arming-order lottery that could strand the real path, not
/// between coverage and none. The status engine never reported on the aliases
/// at all: it asks git, which follows the index.
pub fn watcher<F: notify::EventHandler>(handler: F) -> notify::Result<RecommendedWatcher> {
    RecommendedWatcher::new(handler, Config::default().with_follow_symlinks(false))
}

/// Arm a native watch on `root` feeding `hints`. Must be called *before*
/// the engine's initial enumeration so nothing slips between scan and arm.
/// With `per_dir` (see [`per_dir_watching_pays`]) only `root` is armed here
/// and the reconciler arms the rest as it enumerates, so excluded subtrees
/// never cost a descriptor.
pub fn watch(
    root: &Path,
    recursive: bool,
    per_dir: bool,
    hints: HintSender,
) -> notify::Result<WatchBackend> {
    let mut backend = watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(event) => {
            if event.need_rescan() {
                hints.send(Hint::Rescan);
                return;
            }
            if is_read_only_event(&event.kind) {
                return;
            }
            for path in event.paths {
                hints.send(Hint::Dirty(path));
            }
        }
        Err(_) => {
            hints.send(Hint::Rescan);
        }
    })?;
    let mode = if recursive && !per_dir {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    backend.watch(root, mode)?;
    Ok(WatchBackend {
        watches: Arc::new(Watches {
            watcher: Mutex::new(backend),
            per_dir,
            armed: Mutex::new(std::collections::BTreeSet::from([root.to_path_buf()])),
            outside: Mutex::new(Default::default()),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};

    /// The read/write split the whole watch layer depends on. Getting a
    /// write wrong loses updates; getting a read wrong turns every settle
    /// window into a spin loop, since watchers read inside their own tree.
    #[test]
    fn reads_are_filtered_and_writes_are_not() {
        for kind in [
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Open(AccessMode::Read)),
            EventKind::Access(AccessKind::Open(AccessMode::Write)),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
        ] {
            assert!(is_read_only_event(&kind), "{kind:?} reports a read");
        }
        for kind in [
            // IN_CLOSE_WRITE: a writing session just ended.
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            // Unspecified: ambiguous, so it costs a pass rather than
            // risking a lost change.
            EventKind::Access(AccessKind::Any),
            EventKind::Access(AccessKind::Other),
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Remove(RemoveKind::File),
            EventKind::Any,
            EventKind::Other,
        ] {
            assert!(!is_read_only_event(&kind), "{kind:?} may report a change");
        }
    }

    /// Disarming a subtree is a range query, which is only correct
    /// because `Path` orders component-wise: `a/b`'s descendants sort
    /// immediately after it and before `a/b-x`, a sibling that shares its
    /// string prefix. Byte-wise ordering would put `a/b-x` *between* them
    /// and the range would stop early, stranding watches.
    #[test]
    fn disarming_a_subtree_takes_the_subtree_and_stops_at_a_sibling() {
        let dir = std::env::temp_dir().join(format!("blit-disarm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for sub in ["a/b/c", "a/b-x", "a/bb"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch = watch(&dir, true, true, HintSender { tx }).unwrap().watches;
        for sub in ["a", "a/b", "a/b/c", "a/b-x", "a/bb"] {
            assert!(watch.add_dir(&dir.join(sub)));
        }
        watch.remove_dir(&dir.join("a/b"));
        let armed: Vec<PathBuf> = watch.armed.lock().unwrap().iter().cloned().collect();
        let rel: Vec<&str> = armed
            .iter()
            .filter_map(|p| p.strip_prefix(&dir).ok())
            .filter_map(|p| p.to_str())
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(rel, ["", "a", "a/b-x", "a/bb"], "the root itself stays too");
    }

    /// A watched tree reachable under two names — `real/` and a symlink to it —
    /// must report changes under the *real* one.
    ///
    /// This is the property [`watcher`] buys, and it is a positive assertion
    /// rather than a wait on a negative: `inotify_add_watch` hands back the
    /// same descriptor for an inode already watched, and notify keys its
    /// descriptor→path map on that descriptor, so with following on, arming
    /// the link overwrote the mapping for the real directory and a write to
    /// `real/inner/x` was delivered as `link/inner/x`. The real path — the one
    /// git reports and every non-aliased sync entry lives under — then got no
    /// hint at all. Reverting to `Config::default()` here fails this test with
    /// exactly that swap, whenever the walk reaches the link second.
    #[cfg(target_os = "linux")]
    #[test]
    fn changes_are_reported_under_the_real_path_not_a_symlinked_alias() {
        use crate::{Hint, RootMsg};
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join(format!("blit-watch-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real/inner")).unwrap();
        std::os::unix::fs::symlink(dir.join("real"), dir.join("link")).unwrap();
        let dir = dir.canonicalize().unwrap();

        let (tx, rx) = mpsc::channel();
        // `watch` arms synchronously, so nothing can slip in before the write.
        let _backend = watch(&dir, true, false, HintSender { tx }).unwrap();
        std::fs::write(dir.join("real/inner/w.txt"), b"x").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        let hit = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break None;
            }
            match rx.recv_timeout(left) {
                Ok(RootMsg::Hint(Hint::Dirty(p))) if p.ends_with("real/inner/w.txt") => {
                    break Some(p);
                }
                Ok(RootMsg::Hint(hint)) => seen.push(format!("{hint:?}")),
                Ok(_) => {}
                Err(_) => break None,
            }
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            hit.is_some(),
            "no hint under real/inner/; got {seen:?} — an alias reported instead means \
             the watch is following symlinks again"
        );
    }
}
