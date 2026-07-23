//! Native watch backend via the `notify` crate (inotify on Linux, FSEvents
//! on macOS, `ReadDirectoryChangesW` on Windows), demoted to a dirty-set
//! hint source: every event becomes `Hint::Dirty(path)` and every
//! loss signal — overflow, rescan flag, backend error — degrades to
//! `Hint::Rescan`. No backend behavior is client-visible; the engine
//! verifies everything against the filesystem before emitting.

use crate::{Hint, HintSender};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;

/// Keeps the native watch alive; dropping it unwatches.
pub struct WatchBackend {
    _watcher: RecommendedWatcher,
}

/// Arm a native watch on `root` feeding `hints`. Must be called *before*
/// the engine's initial enumeration so nothing slips between scan and arm.
pub fn watch(root: &Path, recursive: bool, hints: HintSender) -> notify::Result<WatchBackend> {
    let mut watcher =
        notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(event) => {
                if event.need_rescan() {
                    hints.send(Hint::Rescan);
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
    watcher.watch(root, mode)?;
    Ok(WatchBackend { _watcher: watcher })
}
