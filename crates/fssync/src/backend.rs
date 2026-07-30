//! Native watch backend via the `notify` crate (inotify on Linux, FSEvents
//! on macOS, `ReadDirectoryChangesW` on Windows), demoted to a dirty-set
//! hint source: every event becomes `Hint::Dirty(path)` and every
//! loss signal — overflow, rescan flag, backend error — degrades to
//! `Hint::Rescan`. No backend behavior is client-visible; the engine
//! verifies everything against the filesystem before emitting.

use crate::{Hint, HintSender};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;

/// Keeps the native watch alive; dropping it unwatches.
pub struct WatchBackend {
    _watcher: RecommendedWatcher,
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
/// Following is also wrong on the merits here: neither the status engine nor
/// the sync engine reports on paths outside the root it was handed, so the
/// walk only ever costs work whose results are discarded.
pub fn watcher<F: notify::EventHandler>(handler: F) -> notify::Result<RecommendedWatcher> {
    RecommendedWatcher::new(handler, Config::default().with_follow_symlinks(false))
}

/// Arm a native watch on `root` feeding `hints`. Must be called *before*
/// the engine's initial enumeration so nothing slips between scan and arm.
pub fn watch(root: &Path, recursive: bool, hints: HintSender) -> notify::Result<WatchBackend> {
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
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    backend.watch(root, mode)?;
    Ok(WatchBackend { _watcher: backend })
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
}
