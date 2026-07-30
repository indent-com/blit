//! Diff, patch, index, and worktree-status handlers (docs/git.md).
//!
//! Every diff view is the same primitive: flatten two endpoints (commit /
//! tree / index / worktree) into path→(mode, oid) maps, then walk them in
//! step. Worktree entries carry a zero oid until content is read; rename
//! detection is an exact-oid join (reported at similarity 100); the
//! ignore-whitespace modes compare normalized bytes before calling
//! something changed. Patches are aligned row records cut with imara-diff,
//! spans refined per word or character.

use std::collections::BTreeMap;
use std::ops::Range;

use blit_remote::git::{
    GIT_DIFF_ENTRY_BINARY, GIT_DIFF_ENTRY_SUBMODULE, GIT_DIFF_IGNORE_ALL_SPACE,
    GIT_DIFF_IGNORE_SPACE_CHANGE, GIT_DIFF_IGNORED, GIT_DIFF_RENAMES, GIT_DIFF_TRUNCATED,
    GIT_DIFF_UNTRACKED, GIT_ENDPOINT_COMMIT, GIT_ENDPOINT_EMPTY, GIT_ENDPOINT_INDEX,
    GIT_ENDPOINT_MERGE_BASE, GIT_ENDPOINT_TREE, GIT_ENDPOINT_WORKTREE, GIT_INDEX_INTENT_TO_ADD,
    GIT_INDEX_SKIP_WORKTREE, GIT_INDEX_TRUNCATED, GIT_OID_NONE, GIT_PATCH_CHAR_SPANS,
    GIT_PATCH_FILE_BINARY, GIT_PATCH_NO_SPANS, GIT_PATCH_STRUCTURED, GIT_PATCH_TEXT,
    GIT_PATCH_TRUNCATED, GIT_STATE_STATUS_TRUNCATED, GIT_STATUS_CANCELLED,
    GIT_STATUS_ENTRY_CONFLICTED, GIT_STATUS_INVALID, GIT_STATUS_NOT_FOUND, GIT_STATUS_OK,
    GIT_STATUS_OTHER, GIT_STATUS_TOO_LARGE, GitDiffRecord, GitDiffRequest, GitEndpoint,
    GitIndexRecord, GitOid, GitPatchRecord, GitPatchRequest, GitStateRecord,
    append_git_diff_record, append_git_index_record, append_git_patch_record,
    append_git_state_record, msg_git_diff_resp, msg_git_index_resp, msg_git_patch_resp,
};

use crate::{Budgets, Cancel, RepoHandle, is_zero_oid, oid_bytes, oid_from_wire};

/// One side of a flattened endpoint.
#[derive(Clone, PartialEq)]
struct Side {
    mode: u32,
    /// Zero for worktree entries whose content has not been hashed.
    oid: gix::ObjectId,
    /// Worktree entries lazily hash/compare on demand.
    worktree: bool,
    /// An untracked worktree entry that git ignores; drives the `!`
    /// porcelain letter (docs/git.md STATUS record).
    ignored: bool,
}

type Flat = BTreeMap<Vec<u8>, Side>;

/// A file's on-disk identity at the moment its content was proven equal
/// to an index blob. Size and full-precision mtime as in [`stat_matches`],
/// minus its racy-index guard — this signature is anchored to a read this
/// process performed rather than to the index file's mtime; inode and
/// device (unix) also catch rename-over rewrites that preserve both.
#[derive(Clone, Copy, PartialEq)]
struct DiskSig {
    size: u64,
    mtime_s: i64,
    mtime_ns: u32,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    dev: u64,
}

impl DiskSig {
    fn of(md: &std::fs::Metadata) -> Option<DiskSig> {
        use std::time::UNIX_EPOCH;
        let disk = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(DiskSig {
            size: md.len(),
            mtime_s: disk.as_secs() as i64,
            mtime_ns: disk.subsec_nanos(),
            #[cfg(unix)]
            ino: md.ino(),
            #[cfg(unix)]
            dev: md.dev(),
        })
    }
}

/// One proven content-equality: the file looked like `disk` when its
/// bytes hashed equal to index blob `oid`.
struct StatSig {
    disk: DiskSig,
    oid: gix::ObjectId,
}

type StatCache = std::collections::HashMap<Vec<u8>, StatSig>;

/// Cross-snapshot caches for the status pipeline, owned by one state
/// engine (single-threaded use). The index is never written, so git's
/// stat-cache discipline lives here instead: a file whose stat matches an
/// earlier content-equal read is clean without re-reading, and a
/// touched-but-equal file is read once, not once per snapshot.
#[derive(Default)]
pub(crate) struct StatusCaches {
    /// HEAD-tree flatten memo, keyed by the tree's oid.
    head_flat: Option<(gix::ObjectId, Flat)>,
    /// Per-path stat cache (see [`StatSig`]).
    stats: StatCache,
}

fn zero_id(repo: &gix::Repository) -> gix::ObjectId {
    gix::ObjectId::null(repo.object_hash())
}

/// The sorted index's entry range whose paths can satisfy `filter` — the
/// exact entry plus the `filter/` subtree — instead of a full scan. Every
/// returned entry still passes [`under_filter`] (the prefix range also
/// covers e.g. `filterfoo`).
fn index_filter_range<'a>(index: &'a gix::index::File, filter: &[u8]) -> &'a [gix::index::Entry] {
    let prefix: &gix::bstr::BStr = filter.into();
    index.prefixed_entries(prefix).unwrap_or(&[])
}

/// Flatten an endpoint into path → side. `filter` restricts to a subtree.
/// `truncated` is set when the untracked walk hit its budget and returned a
/// partial set, so callers can raise the appropriate TRUNCATED flag.
/// `stats` is the status pipeline's stat cache ([`StatusCaches`]); request
/// handlers pass `None`.
#[allow(clippy::too_many_arguments)]
fn flatten(
    repo: &gix::Repository,
    endpoint: &GitEndpoint,
    filter: &[u8],
    untracked: bool,
    ignored: bool,
    budgets: &Budgets,
    cancel: &Cancel,
    truncated: &std::cell::Cell<bool>,
    stats: Option<&StatCache>,
) -> Result<Flat, u8> {
    let mut flat = Flat::new();
    match endpoint.kind {
        GIT_ENDPOINT_EMPTY => {}
        GIT_ENDPOINT_COMMIT | GIT_ENDPOINT_TREE => {
            if is_zero_oid(&endpoint.oid) {
                return Err(GIT_STATUS_NOT_FOUND);
            }
            let id = oid_from_wire(repo, &endpoint.oid);
            let object = repo.find_object(id).map_err(|_| GIT_STATUS_NOT_FOUND)?;
            let tree = object.peel_to_tree().map_err(|_| GIT_STATUS_NOT_FOUND)?;
            // A non-empty filter descends to its entry first and traverses
            // only that subtree — never the whole tree (docs/git.md:
            // `path` filters to a subtree).
            let (tree, prefix) = if filter.is_empty() {
                (tree, Vec::new())
            } else {
                match tree
                    .lookup_entry_by_path(gix::path::from_byte_slice(filter))
                    .map_err(|_| GIT_STATUS_OTHER)?
                {
                    None => return Ok(flat),
                    Some(entry) if entry.mode().is_tree() => {
                        let sub = entry
                            .object()
                            .map_err(|_| GIT_STATUS_NOT_FOUND)?
                            .peel_to_tree()
                            .map_err(|_| GIT_STATUS_OTHER)?;
                        let mut prefix = filter.to_vec();
                        prefix.push(b'/');
                        (sub, prefix)
                    }
                    Some(entry) => {
                        flat.insert(
                            filter.to_vec(),
                            Side {
                                mode: entry.mode().value() as u32,
                                oid: entry.oid().to_owned(),
                                worktree: false,
                                ignored: false,
                            },
                        );
                        return Ok(flat);
                    }
                }
            };
            let mut recorder = gix::traverse::tree::Recorder::default();
            tree.traverse()
                .breadthfirst(&mut recorder)
                .map_err(|_| GIT_STATUS_OTHER)?;
            for entry in recorder.records {
                if cancel.is_cancelled() {
                    return Err(GIT_STATUS_CANCELLED);
                }
                if !matches!(entry.mode.kind(), gix::object::tree::EntryKind::Tree) {
                    let mut path = prefix.clone();
                    path.extend_from_slice(&entry.filepath);
                    flat.insert(
                        path,
                        Side {
                            mode: entry.mode.value() as u32,
                            oid: entry.oid,
                            worktree: false,
                            ignored: false,
                        },
                    );
                }
            }
        }
        GIT_ENDPOINT_INDEX => {
            let index = repo.index_or_empty().map_err(|_| GIT_STATUS_OTHER)?;
            for entry in index_filter_range(&index, filter) {
                let path = entry.path(&index);
                if !under_filter(path, filter) {
                    continue;
                }
                // Stage 0 is the resolved entry; conflicts diff via their
                // "ours" stage so the path still appears.
                if entry.stage() == gix::index::entry::Stage::Base {
                    continue;
                }
                flat.entry(path.to_vec()).or_insert(Side {
                    mode: entry.mode.bits(),
                    oid: entry.id,
                    worktree: false,
                    ignored: false,
                });
            }
        }
        GIT_ENDPOINT_WORKTREE => {
            let workdir = repo.workdir().ok_or(GIT_STATUS_INVALID)?.to_path_buf();
            // Tracked files: the index projected onto the disk.
            let index = repo.index_or_empty().map_err(|_| GIT_STATUS_OTHER)?;
            let index_mtime = file_mtime(index.path());
            for entry in index_filter_range(&index, filter) {
                if cancel.is_cancelled() {
                    return Err(GIT_STATUS_CANCELLED);
                }
                let path = entry.path(&index);
                if !under_filter(path, filter)
                    || entry.stage() == gix::index::entry::Stage::Base
                    || entry
                        .flags
                        .contains(gix::index::entry::Flags::SKIP_WORKTREE)
                {
                    continue;
                }
                let abs = workdir.join(gix::path::from_byte_slice(path));
                let Ok(md) = std::fs::symlink_metadata(&abs) else {
                    continue; // deleted from the worktree
                };
                if md.is_dir() {
                    continue; // replaced by a directory: not a file anymore
                }
                // Index stat first; then the engine's own stat cache — a
                // stat unchanged since its content last hashed equal to
                // this same blob is clean without a read.
                let unchanged = stat_matches(entry, &md, index_mtime)
                    || stats.is_some_and(|cache| {
                        cache.get(path.as_ref() as &[u8]).is_some_and(|sig| {
                            sig.oid == entry.id && Some(sig.disk) == DiskSig::of(&md)
                        })
                    });
                flat.insert(
                    path.to_vec(),
                    Side {
                        mode: worktree_mode(&md, entry.mode.bits()),
                        oid: if unchanged { entry.id } else { zero_id(repo) },
                        worktree: !unchanged,
                        ignored: false,
                    },
                );
            }
            if untracked {
                collect_untracked(
                    repo, &workdir, &index, filter, ignored, budgets, cancel, &mut flat, truncated,
                )?;
            }
        }
        _ => return Err(GIT_STATUS_INVALID),
    }
    Ok(flat)
}

fn under_filter(path: &[u8], filter: &[u8]) -> bool {
    filter.is_empty()
        || path == filter
        || (path.len() > filter.len() && path.starts_with(filter) && path[filter.len()] == b'/')
}

/// A file's mtime as (seconds, nanoseconds) since the epoch, or None if it
/// cannot be read.
fn file_mtime(path: &std::path::Path) -> Option<(i64, u32)> {
    use std::time::UNIX_EPOCH;
    let d = std::fs::metadata(path)
        .and_then(|md| md.modified())
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some((d.as_secs() as i64, d.subsec_nanos()))
}

/// Conservative index-stat match: size plus full-precision mtime, and only
/// for an entry the index is entitled to call clean at all.
///
/// Nanoseconds alone are not enough. Linux stamps mtimes from the coarse
/// clock, which only advances once per timer tick, so a file written twice
/// within one tick carries a byte-identical mtime both times — and a
/// same-size rewrite between the `git add` that recorded the stat and the
/// index write that followed it is then indistinguishable from no write at
/// all. That is the racy-git problem, and git's answer (`is_racy_stat`,
/// read-cache.c) is to distrust any entry whose recorded mtime is not
/// strictly older than the index file's own mtime: within that window the
/// stat proves nothing and the content must be read. `index_mtime` is that
/// timestamp; None (unreadable index) distrusts everything.
///
/// A false mismatch only costs a content hash, never a wrong answer.
fn stat_matches(
    entry: &gix::index::Entry,
    md: &std::fs::Metadata,
    index_mtime: Option<(i64, u32)>,
) -> bool {
    use std::time::UNIX_EPOCH;
    if u64::from(entry.stat.size) != md.len() {
        return false;
    }
    let Some(disk) = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    else {
        return false;
    };
    let disk = (disk.as_secs() as i64, disk.subsec_nanos());
    if (i64::from(entry.stat.mtime.secs), entry.stat.mtime.nsecs) != disk {
        return false;
    }
    index_mtime.is_some_and(|index_mtime| index_mtime > disk)
}

fn worktree_mode(md: &std::fs::Metadata, _index_mode: u32) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if md.file_type().is_symlink() {
            return 0o120000;
        }
        if md.mode() & 0o111 != 0 {
            return 0o100755;
        }
        0o100644
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        _index_mode
    }
}

/// Untracked (and optionally ignored) files via a bounded walk honoring
/// the exclude stack.
#[allow(clippy::too_many_arguments)]
fn collect_untracked(
    repo: &gix::Repository,
    workdir: &std::path::Path,
    index: &gix::index::File,
    filter: &[u8],
    ignored: bool,
    budgets: &Budgets,
    cancel: &Cancel,
    flat: &mut Flat,
    truncated: &std::cell::Cell<bool>,
) -> Result<(), u8> {
    let worktree = repo.worktree().ok_or(GIT_STATUS_INVALID)?;
    let mut excludes = worktree.excludes(None).map_err(|_| GIT_STATUS_OTHER)?;
    let mut stack: Vec<std::path::PathBuf> = vec![workdir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        if cancel.is_cancelled() {
            return Err(GIT_STATUS_CANCELLED);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > budgets.untracked_scan_max {
                truncated.set(true); // bounded: partial untracked view
                return Ok(());
            }
            let abs = entry.path();
            let Ok(rel) = abs.strip_prefix(workdir) else {
                continue;
            };
            let rel_bytes = gix::path::os_str_into_bstr(rel.as_os_str())
                .map(|b| b.to_owned())
                .unwrap_or_default();
            let rel_vec = rel_bytes.to_vec();
            if rel_vec == b".git" || rel_vec.is_empty() {
                continue;
            }
            let Ok(md) = entry.metadata() else { continue };
            let is_dir = md.is_dir();
            // With a filter, only directories that can contain it (its
            // ancestors) or sit under it are worth descending; files
            // outside the filter never match.
            let filter_relevant = filter.is_empty()
                || under_filter(&rel_vec, filter)
                || (is_dir
                    && filter.len() > rel_vec.len()
                    && filter.starts_with(&rel_vec)
                    && filter[rel_vec.len()] == b'/');
            if !filter_relevant {
                continue;
            }
            let mode = if is_dir {
                gix::index::entry::Mode::DIR
            } else {
                gix::index::entry::Mode::FILE
            };
            let rel_bstr: &gix::bstr::BStr = rel_bytes.as_ref();
            let excluded = excludes
                .at_entry(rel_bstr, Some(mode))
                .map(|platform| platform.is_excluded())
                .unwrap_or(false);
            if excluded && !ignored {
                continue;
            }
            if is_dir {
                stack.push(abs);
                continue;
            }
            if index.entry_by_path(rel_bytes.as_ref()).is_some() {
                continue;
            }
            flat.insert(
                rel_vec,
                Side {
                    mode: worktree_mode(&md, 0o100644),
                    oid: zero_id(repo),
                    worktree: true,
                    ignored: excluded,
                },
            );
        }
    }
    Ok(())
}

/// One computed difference, pre-rename-join.
struct Change {
    path: Vec<u8>,
    st: u8,
    old: Option<Side>,
    new: Option<Side>,
}

/// Read one side's bytes: blob by oid, or the worktree file. A worktree
/// symlink yields its target path (git's symlink blob content), never the
/// pointed-at file's bytes.
fn side_bytes(
    repo: &gix::Repository,
    workdir: Option<&std::path::Path>,
    path: &[u8],
    side: &Side,
) -> Option<Vec<u8>> {
    if !side.oid.is_null() {
        // detach() moves the decoded buffer out — no copy.
        return repo.find_object(side.oid).ok().map(|o| o.detach().data);
    }
    let workdir = workdir?;
    let abs = workdir.join(gix::path::from_byte_slice(path));
    if side.mode & 0o170000 == 0o120000 {
        let target = std::fs::read_link(&abs).ok()?;
        return Some(gix::path::into_bstr(target).into_owned().into());
    }
    std::fs::read(abs).ok()
}

/// Binary sniff (NUL in the first 8 KiB) without materializing whole
/// sides: worktree files read at most 8 KiB from disk; object sides
/// borrow the decoded blob and memoize the verdict by oid, so a rename
/// pair or duplicated blob sniffs once. Over-cap sides count as binary
/// unread, as everywhere else.
fn side_is_binary(
    repo: &gix::Repository,
    workdir: Option<&std::path::Path>,
    path: &[u8],
    side: &Side,
    input_cap: u64,
    memo: &mut std::collections::HashMap<gix::ObjectId, bool>,
) -> bool {
    if !side.oid.is_null() {
        if let Some(&binary) = memo.get(&side.oid) {
            return binary;
        }
        let over_cap = repo
            .find_header(side.oid)
            .ok()
            .is_some_and(|h| h.size() > input_cap);
        let binary = over_cap
            || repo
                .find_object(side.oid)
                .ok()
                .is_some_and(|o| looks_binary(&o.data));
        memo.insert(side.oid, binary);
        return binary;
    }
    let Some(workdir) = workdir else {
        return false;
    };
    let abs = workdir.join(gix::path::from_byte_slice(path));
    if side.mode & 0o170000 == 0o120000 {
        let Ok(target) = std::fs::read_link(&abs) else {
            return false;
        };
        let bytes: Vec<u8> = gix::path::into_bstr(target).into_owned().into();
        return looks_binary(&bytes);
    }
    if std::fs::symlink_metadata(&abs).is_ok_and(|md| md.len() > input_cap) {
        return true;
    }
    use std::io::Read;
    let Ok(file) = std::fs::File::open(&abs) else {
        return false;
    };
    let mut head = [0u8; 8192];
    let mut taken = file.take(8192);
    let mut filled = 0usize;
    loop {
        match taken.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    head[..filled].contains(&0)
}

/// The byte length of one side without materializing it — for the input
/// size cap. Blob header for objects, filesystem metadata for the worktree.
fn side_len(
    repo: &gix::Repository,
    workdir: Option<&std::path::Path>,
    path: &[u8],
    side: &Side,
) -> Option<u64> {
    if !side.oid.is_null() {
        return repo.find_header(side.oid).ok().map(|h| h.size());
    }
    let abs = workdir?.join(gix::path::from_byte_slice(path));
    std::fs::symlink_metadata(&abs).ok().map(|m| m.len())
}

fn normalize_ws(bytes: &[u8], mode: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    if mode & GIT_DIFF_IGNORE_ALL_SPACE != 0 {
        out.extend(bytes.iter().copied().filter(|b| !b" \t\r".contains(b)));
    } else {
        // -b: whitespace runs compare equal, trailing whitespace ignored.
        for line in bytes.split_inclusive(|&b| b == b'\n') {
            let (body, nl) = match line.last() {
                Some(b'\n') => (&line[..line.len() - 1], true),
                _ => (line, false),
            };
            let trimmed = body
                .iter()
                .rposition(|b| !b" \t\r".contains(b))
                .map(|i| &body[..=i])
                .unwrap_or(b"");
            let mut in_space = false;
            for &b in trimmed {
                if b == b' ' || b == b'\t' {
                    if !in_space {
                        out.push(b' ');
                    }
                    in_space = true;
                } else {
                    in_space = false;
                    out.push(b);
                }
            }
            if nl {
                out.push(b'\n');
            }
        }
    }
    out
}

/// The path→changes walk shared by `GIT_DIFF`, `GIT_PATCH`, and STATUS.
/// `ws` carries the ignore-whitespace bits (0 = exact). `stats` is the
/// status pipeline's stat cache, fed by proven content equalities.
#[allow(clippy::too_many_arguments)]
fn diff_flats(
    repo: &gix::Repository,
    workdir: Option<&std::path::Path>,
    old: &Flat,
    new: &Flat,
    ws: u8,
    renames: bool,
    input_cap: u64,
    cancel: &Cancel,
    mut stats: Option<&mut StatCache>,
) -> Result<Vec<Change>, u8> {
    let mut changes: Vec<Change> = Vec::new();
    let mut old_iter = old.iter().peekable();
    let mut new_iter = new.iter().peekable();
    loop {
        if cancel.is_cancelled() {
            return Err(GIT_STATUS_CANCELLED);
        }
        match (old_iter.peek(), new_iter.peek()) {
            (Some((op, ov)), Some((np, nv))) => {
                if op == np {
                    if (ov != nv || nv.worktree)
                        && let Some(st) = modified_status(
                            repo,
                            workdir,
                            op,
                            ov,
                            nv,
                            ws,
                            input_cap,
                            stats.as_deref_mut(),
                        )
                    {
                        changes.push(Change {
                            path: (*op).clone(),
                            st,
                            old: Some((*ov).clone()),
                            new: Some((*nv).clone()),
                        });
                    }
                    old_iter.next();
                    new_iter.next();
                } else if op < np {
                    changes.push(Change {
                        path: (*op).clone(),
                        st: b'D',
                        old: Some((*ov).clone()),
                        new: None,
                    });
                    old_iter.next();
                } else {
                    changes.push(Change {
                        path: (*np).clone(),
                        st: b'A',
                        old: None,
                        new: Some((*nv).clone()),
                    });
                    new_iter.next();
                }
            }
            (Some((op, ov)), None) => {
                changes.push(Change {
                    path: (*op).clone(),
                    st: b'D',
                    old: Some((*ov).clone()),
                    new: None,
                });
                old_iter.next();
            }
            (None, Some((np, nv))) => {
                changes.push(Change {
                    path: (*np).clone(),
                    st: b'A',
                    old: None,
                    new: Some((*nv).clone()),
                });
                new_iter.next();
            }
            (None, None) => break,
        }
    }
    if renames {
        join_renames(&mut changes);
    }
    Ok(changes)
}

/// Decide whether a same-path pair actually differs (hashing worktree
/// content and applying whitespace normalization as needed); the status
/// letter, or None when equal. With `stats`, a proven byte equality
/// between an index blob (`old`) and the worktree file (`new`) is
/// recorded so later snapshots skip the read; the stat is captured BEFORE
/// the read, so a write racing the read invalidates the recorded
/// signature, never the verdict.
#[allow(clippy::too_many_arguments)]
fn modified_status(
    repo: &gix::Repository,
    workdir: Option<&std::path::Path>,
    path: &[u8],
    old: &Side,
    new: &Side,
    ws: u8,
    input_cap: u64,
    stats: Option<&mut StatCache>,
) -> Option<u8> {
    let type_change = (old.mode & 0o170000) != (new.mode & 0o170000);
    let content_maybe_differs = new.worktree || old.worktree || old.oid != new.oid;
    if !content_maybe_differs {
        return if old.mode != new.mode {
            Some(if type_change { b'T' } else { b'M' })
        } else {
            None
        };
    }
    // Fast path: two content-addressed objects (no worktree side) with
    // differing oids provably differ — no need to read either blob. Avoids
    // reading full content of every changed file in a commit/index diff.
    if ws == 0 && !old.worktree && !new.worktree {
        return Some(if type_change { b'T' } else { b'M' });
    }
    // Never materialize a side larger than the input cap just to compare it:
    // an over-cap pair counts as changed without reading content, matching
    // the binary/patch paths' cap and keeping this pre-loop check bounded.
    if side_len(repo, workdir, path, old).unwrap_or(0) > input_cap
        || side_len(repo, workdir, path, new).unwrap_or(0) > input_cap
    {
        return Some(if type_change { b'T' } else { b'M' });
    }
    // Stat-cache candidate: index blob vs an unhashed worktree file under
    // an exact comparison. Snapshot the disk identity before reading.
    let cacheable = ws == 0 && !old.oid.is_null() && !old.worktree && new.worktree;
    let disk = match (cacheable, workdir) {
        (true, Some(workdir)) => {
            let abs = workdir.join(gix::path::from_byte_slice(path));
            std::fs::symlink_metadata(abs)
                .ok()
                .and_then(|md| (!md.is_dir()).then(|| DiskSig::of(&md)).flatten())
        }
        _ => None,
    };
    // Content check: worktree side re-hashes; whitespace modes compare
    // normalized bytes.
    let old_bytes = side_bytes(repo, workdir, path, old);
    let new_bytes = side_bytes(repo, workdir, path, new);
    match (old_bytes, new_bytes) {
        (Some(a), Some(b)) => {
            let equal = if ws == 0 {
                a == b
            } else {
                normalize_ws(&a, ws) == normalize_ws(&b, ws)
            };
            if let Some(cache) = stats
                && cacheable
            {
                match (equal, disk) {
                    (true, Some(disk)) => {
                        cache.insert(path.to_vec(), StatSig { disk, oid: old.oid });
                    }
                    _ => {
                        cache.remove(path);
                    }
                }
            }
            if equal {
                if old.mode != new.mode {
                    Some(if type_change { b'T' } else { b'M' })
                } else {
                    None
                }
            } else {
                Some(if type_change { b'T' } else { b'M' })
            }
        }
        _ => Some(b'M'),
    }
}

/// Exact-oid rename join: a deleted and an added entry with the same
/// non-null oid collapse into one rename (similarity 100).
fn join_renames(changes: &mut Vec<Change>) {
    let mut by_oid: std::collections::HashMap<gix::ObjectId, usize> = Default::default();
    for (idx, change) in changes.iter().enumerate() {
        if change.st == b'D'
            && let Some(old) = &change.old
            && !old.oid.is_null()
        {
            by_oid.insert(old.oid, idx);
        }
    }
    let mut any_joined = false;
    for idx in 0..changes.len() {
        if changes[idx].st != b'A' {
            continue;
        }
        let Some(new) = changes[idx].new.clone() else {
            continue;
        };
        if new.oid.is_null() {
            continue;
        }
        // Consume the matched delete from the map so it can never re-match;
        // this keeps rename joining O(N) rather than O(N^2).
        if let Some(del_idx) = by_oid.remove(&new.oid) {
            let old_path = changes[del_idx].path.clone();
            let old_side = changes[del_idx].old.clone();
            // Rename entries carry both paths, NUL-joined; the matching
            // deletion is marked consumed (`st` 0) and swept below.
            let mut both = old_path;
            both.push(0);
            both.extend_from_slice(&changes[idx].path);
            let change = &mut changes[idx];
            change.st = b'R';
            change.old = old_side;
            change.path = both;
            changes[del_idx].st = 0;
            any_joined = true;
        }
    }
    if any_joined {
        changes.retain(|change| change.st != 0);
    }
}

/// Split a rename's NUL-joined path back into (old, new).
fn rename_paths(change: &Change) -> (Vec<u8>, Vec<u8>) {
    if change.st == b'R'
        && let Some(pos) = change.path.iter().position(|&b| b == 0)
    {
        return (change.path[..pos].to_vec(), change.path[pos + 1..].to_vec());
    }
    (Vec::new(), change.path.clone())
}

/// Resolve request endpoints, substituting `merge-base(old, new)` for a
/// MERGE_BASE old side. Returns the endpoints plus the BASE oid to reveal.
fn resolve_endpoints(
    repo: &gix::Repository,
    memo: &crate::requests::MergeMemo,
    old: &GitEndpoint,
    new: &GitEndpoint,
    budget: usize,
    cancel: &Cancel,
) -> Result<(GitEndpoint, GitEndpoint, Option<GitOid>), u8> {
    if new.kind == GIT_ENDPOINT_MERGE_BASE {
        return Err(GIT_STATUS_INVALID);
    }
    if old.kind != GIT_ENDPOINT_MERGE_BASE {
        return Ok((*old, *new, None));
    }
    if new.kind != GIT_ENDPOINT_COMMIT {
        return Err(GIT_STATUS_INVALID);
    }
    let a = oid_from_wire(repo, &old.oid);
    let b = oid_from_wire(repo, &new.oid);
    let base = crate::requests::bounded_merge_base(repo, memo, a, b, budget, cancel)?
        .ok_or(GIT_STATUS_INVALID)?;
    let base_bytes = oid_bytes(base.as_ref());
    Ok((
        GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: base_bytes,
        },
        *new,
        Some(base_bytes),
    ))
}

impl RepoHandle {
    /// `GIT_DIFF`: file-level records between any two endpoints.
    pub fn diff(&self, req: &GitDiffRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8| msg_git_diff_resp(req.nonce, status, 0, &[]);
        // Reject undefined flag bits (docs/git.md: INVALID on unknown flags).
        const KNOWN_DIFF_FLAGS: u8 = GIT_DIFF_RENAMES
            | GIT_DIFF_UNTRACKED
            | GIT_DIFF_IGNORED
            | GIT_DIFF_IGNORE_SPACE_CHANGE
            | GIT_DIFF_IGNORE_ALL_SPACE;
        if req.flags & !KNOWN_DIFF_FLAGS != 0 {
            return fail(GIT_STATUS_INVALID);
        }
        let filter = match crate::unescape_wire(req.path) {
            Some(bytes) => bytes,
            None => return fail(GIT_STATUS_OTHER),
        };
        let (old_ep, new_ep, base) = match resolve_endpoints(
            &repo,
            &self.merge_memo,
            &req.old,
            &req.new,
            self.budgets.walk_max,
            cancel,
        ) {
            Ok(resolved) => resolved,
            Err(status) => return fail(status),
        };
        let untracked = req.flags & GIT_DIFF_UNTRACKED != 0;
        let ignored = req.flags & GIT_DIFF_IGNORED != 0;
        let ws = req.flags & (GIT_DIFF_IGNORE_SPACE_CHANGE | GIT_DIFF_IGNORE_ALL_SPACE);
        let truncated = std::cell::Cell::new(false);
        let sides = [&old_ep, &new_ep].map(|endpoint| {
            flatten(
                &repo,
                endpoint,
                &filter,
                untracked,
                ignored,
                &self.budgets,
                cancel,
                &truncated,
                None,
            )
        });
        let [old_flat, new_flat] = sides;
        let (old_flat, new_flat) = match (old_flat, new_flat) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(status), _) | (_, Err(status)) => return fail(status),
        };
        let workdir = repo.workdir().map(|p| p.to_path_buf());
        let input_cap = self
            .budgets
            .blob_max
            .min(blit_remote::MAX_DECOMPRESSED as u64);
        let changes = match diff_flats(
            &repo,
            workdir.as_deref(),
            &old_flat,
            &new_flat,
            ws,
            req.flags & GIT_DIFF_RENAMES != 0,
            input_cap,
            cancel,
            None,
        ) {
            Ok(changes) => changes,
            Err(status) => return fail(status),
        };

        let mut records = Vec::new();
        let mut flags = if truncated.get() {
            GIT_DIFF_TRUNCATED
        } else {
            0
        };
        if let Some(oid) = base {
            append_git_diff_record(&mut records, &GitDiffRecord::Base { oid });
        }
        let mut binary_memo: std::collections::HashMap<gix::ObjectId, bool> = Default::default();
        for (count, change) in changes.iter().enumerate() {
            if count >= self.budgets.entries_max || records.len() >= self.budgets.bytes_max {
                flags |= GIT_DIFF_TRUNCATED;
                break;
            }
            let (old_path, new_path) = rename_paths(change);
            let old_side = change.old.clone();
            let new_side = change.new.clone();
            let submodule = [&old_side, &new_side].iter().any(|s| {
                s.as_ref()
                    .is_some_and(|side| side.mode & 0o170000 == 0o160000)
            });
            let mut dflags = if submodule {
                GIT_DIFF_ENTRY_SUBMODULE
            } else {
                0
            };
            // BINARY dflag (docs/git.md DIFF_ENTRY): NUL in the first 8 KiB
            // of either present side (deletions included — the old blob is
            // available). Skipped for submodules (no bytes). Sniffed with
            // bounded reads, never whole-blob materialization.
            if !submodule {
                let mut sniff = |path: &[u8], side: &Option<Side>| {
                    side.as_ref().is_some_and(|s| {
                        side_is_binary(
                            &repo,
                            workdir.as_deref(),
                            path,
                            s,
                            input_cap,
                            &mut binary_memo,
                        )
                    })
                };
                if sniff(&old_path_or(&old_path, change), &old_side) || sniff(&new_path, &new_side)
                {
                    dflags |= GIT_DIFF_ENTRY_BINARY;
                }
            }
            append_git_diff_record(
                &mut records,
                &GitDiffRecord::Entry {
                    st: change.st,
                    similarity: if change.st == b'R' { 100 } else { 0 },
                    dflags,
                    old_mode: old_side.as_ref().map(|s| s.mode).unwrap_or(0),
                    new_mode: new_side.as_ref().map(|s| s.mode).unwrap_or(0),
                    old_oid: old_side
                        .as_ref()
                        .map(|s| oid_bytes(s.oid.as_ref()))
                        .unwrap_or(GIT_OID_NONE),
                    new_oid: new_side
                        .as_ref()
                        .map(|s| oid_bytes(s.oid.as_ref()))
                        .unwrap_or(GIT_OID_NONE),
                    old_path: &crate::escape_bstr(&old_path),
                    new_path: &crate::escape_bstr(&new_path),
                },
            );
        }
        msg_git_diff_resp(req.nonce, GIT_STATUS_OK, flags, &records)
    }

    /// `GIT_INDEX`: enumerate index entries under a prefix.
    pub fn index(&self, nonce: u16, path: &str, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8| msg_git_index_resp(nonce, status, 0, &[]);
        let filter = match crate::unescape_wire(path) {
            Some(bytes) => bytes,
            None => return fail(GIT_STATUS_OTHER),
        };
        let index = match repo.index_or_empty() {
            Ok(index) => index,
            Err(_) => return fail(GIT_STATUS_OTHER),
        };
        let mut records = Vec::new();
        let mut flags = 0u8;
        let mut count = 0usize;
        for entry in index.entries() {
            if cancel.is_cancelled() {
                return fail(GIT_STATUS_CANCELLED);
            }
            let entry_path = entry.path(&index);
            if !under_filter(entry_path, &filter) {
                continue;
            }
            if count >= self.budgets.entries_max || records.len() >= self.budgets.bytes_max {
                flags |= GIT_INDEX_TRUNCATED;
                break;
            }
            count += 1;
            let mut iflags = 0u8;
            if entry
                .flags
                .contains(gix::index::entry::Flags::INTENT_TO_ADD)
            {
                iflags |= GIT_INDEX_INTENT_TO_ADD;
            }
            if entry
                .flags
                .contains(gix::index::entry::Flags::SKIP_WORKTREE)
            {
                iflags |= GIT_INDEX_SKIP_WORKTREE;
            }
            append_git_index_record(
                &mut records,
                &GitIndexRecord::Entry {
                    stage: match entry.stage() {
                        gix::index::entry::Stage::Unconflicted => 0,
                        gix::index::entry::Stage::Base => 1,
                        gix::index::entry::Stage::Ours => 2,
                        gix::index::entry::Stage::Theirs => 3,
                    },
                    iflags,
                    mode: entry.mode.bits(),
                    size: u64::from(entry.stat.size),
                    mtime_ns: u64::from(entry.stat.mtime.secs) * 1_000_000_000
                        + u64::from(entry.stat.mtime.nsecs),
                    oid: oid_bytes(entry.id.as_ref()),
                    path: &crate::escape_bstr(entry_path),
                },
            );
        }
        msg_git_index_resp(nonce, GIT_STATUS_OK, flags, &records)
    }

    /// `GIT_PATCH`: aligned row records (default) or unified text.
    pub fn patch(&self, req: &GitPatchRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8| msg_git_patch_resp(req.nonce, status, 0, &[]);
        let filter = match crate::unescape_wire(req.path) {
            Some(bytes) => bytes,
            None => return fail(GIT_STATUS_OTHER),
        };
        let (old_ep, new_ep, base) = match resolve_endpoints(
            &repo,
            &self.merge_memo,
            &req.old,
            &req.new,
            self.budgets.walk_max,
            cancel,
        ) {
            Ok(resolved) => resolved,
            Err(status) => return fail(status),
        };
        let ws = req.flags & (GIT_DIFF_IGNORE_SPACE_CHANGE | GIT_DIFF_IGNORE_ALL_SPACE);
        let truncated = std::cell::Cell::new(false);
        let sides = [&old_ep, &new_ep].map(|endpoint| {
            flatten(
                &repo,
                endpoint,
                &filter,
                req.flags & GIT_DIFF_UNTRACKED != 0,
                req.flags & GIT_DIFF_IGNORED != 0,
                &self.budgets,
                cancel,
                &truncated,
                None,
            )
        });
        let [old_flat, new_flat] = sides;
        let (old_flat, new_flat) = match (old_flat, new_flat) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(status), _) | (_, Err(status)) => return fail(status),
        };
        let workdir = repo.workdir().map(|p| p.to_path_buf());
        // Input size cap: never materialize a side larger than the blob cap
        // just to line-diff it — treat it as binary (no rows).
        let input_cap = self
            .budgets
            .blob_max
            .min(blit_remote::MAX_DECOMPRESSED as u64);
        let changes = match diff_flats(
            &repo,
            workdir.as_deref(),
            &old_flat,
            &new_flat,
            ws,
            req.flags & GIT_DIFF_RENAMES != 0,
            input_cap,
            cancel,
            None,
        ) {
            Ok(changes) => changes,
            Err(status) => return fail(status),
        };

        let context = if req.context == 0 {
            3
        } else {
            req.context as usize
        };
        let text_mode = req.flags & GIT_PATCH_TEXT != 0;
        let max_len = if req.max_len == 0 {
            self.budgets.blob_max as usize
        } else {
            (req.max_len as usize).min(blit_remote::MAX_DECOMPRESSED)
        };
        let mut records: Vec<u8> = Vec::new();
        let mut text: Vec<u8> = Vec::new();
        let mut resp_flags = if text_mode { 0 } else { GIT_PATCH_STRUCTURED };
        if truncated.get() {
            resp_flags |= GIT_PATCH_TRUNCATED;
        }
        if !text_mode && let Some(oid) = base {
            append_git_patch_record(&mut records, &GitPatchRecord::Base { oid });
        }
        for change in &changes {
            if cancel.is_cancelled() {
                return fail(GIT_STATUS_CANCELLED);
            }
            let out_len = if text_mode { text.len() } else { records.len() };
            if out_len >= max_len || out_len >= self.budgets.bytes_max {
                resp_flags |= GIT_PATCH_TRUNCATED;
                break;
            }
            let (old_path, _new_path) = rename_paths(change);
            let old_read_path = old_path_or(&old_path, change);
            let new_read_path = change_new_path(change);
            let over_cap = |path: &[u8], side: &Option<Side>| {
                side.as_ref().is_some_and(|s| {
                    side_len(&repo, workdir.as_deref(), path, s).unwrap_or(0) > input_cap
                })
            };
            let too_large =
                over_cap(&old_read_path, &change.old) || over_cap(&new_read_path, &change.new);
            let (old_bytes, new_bytes, binary) = if too_large {
                (Vec::new(), Vec::new(), true)
            } else {
                let old_bytes = change
                    .old
                    .as_ref()
                    .and_then(|side| side_bytes(&repo, workdir.as_deref(), &old_read_path, side))
                    .unwrap_or_default();
                let new_bytes = change
                    .new
                    .as_ref()
                    .and_then(|side| side_bytes(&repo, workdir.as_deref(), &new_read_path, side))
                    .unwrap_or_default();
                let binary = looks_binary(&old_bytes) || looks_binary(&new_bytes);
                (old_bytes, new_bytes, binary)
            };
            if text_mode {
                append_text_patch(
                    &mut text, &old_path, change, binary, &old_bytes, &new_bytes, context, ws,
                );
            } else {
                append_git_patch_record(
                    &mut records,
                    &GitPatchRecord::File {
                        flags: if binary { GIT_PATCH_FILE_BINARY } else { 0 },
                        old_path: &crate::escape_bstr(&old_path),
                        new_path: &crate::escape_bstr(&change_new_path(change)),
                    },
                );
                if !binary {
                    append_rows(
                        &mut records,
                        &old_bytes,
                        &new_bytes,
                        context,
                        ws,
                        req.flags & GIT_PATCH_CHAR_SPANS != 0,
                        req.flags & GIT_PATCH_NO_SPANS != 0,
                    );
                }
            }
        }
        let payload = if text_mode { text } else { records };
        if payload.len() > max_len {
            return fail(GIT_STATUS_TOO_LARGE);
        }
        msg_git_patch_resp(req.nonce, GIT_STATUS_OK, resp_flags, &payload)
    }
}

fn old_path_or(old_path: &[u8], change: &Change) -> Vec<u8> {
    if old_path.is_empty() {
        change_new_path(change)
    } else {
        old_path.to_vec()
    }
}

fn change_new_path(change: &Change) -> Vec<u8> {
    rename_paths(change).1
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

/// Line-level changes between two byte buffers, on whitespace-normalized
/// text when `ws` is set. Returned ranges index the TRUE line lists.
fn line_changes(old: &[u8], new: &[u8], ws: u8) -> Vec<(Range<u32>, Range<u32>)> {
    use imara_diff::{Algorithm, Diff, InternedInput, sources::byte_lines};
    use std::borrow::Cow;
    let (old_cmp, new_cmp): (Cow<'_, [u8]>, Cow<'_, [u8]>) = if ws == 0 {
        (Cow::Borrowed(old), Cow::Borrowed(new))
    } else {
        (
            Cow::Owned(normalize_ws(old, ws)),
            Cow::Owned(normalize_ws(new, ws)),
        )
    };
    let input = InternedInput::new(byte_lines(&old_cmp), byte_lines(&new_cmp));
    let diff = Diff::compute(Algorithm::Histogram, &input);
    diff.hunks().map(|h| (h.before, h.after)).collect()
}

fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes
        .split_inclusive(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\n").unwrap_or(line))
        .collect()
}

/// Emit PATCH_ROW/PATCH_GAP records for one file.
fn append_rows(
    records: &mut Vec<u8>,
    old_bytes: &[u8],
    new_bytes: &[u8],
    context: usize,
    ws: u8,
    char_spans: bool,
    no_spans: bool,
) {
    let old_lines = split_lines(old_bytes);
    let new_lines = split_lines(new_bytes);
    let changes = line_changes(old_bytes, new_bytes, ws);
    let mut old_pos = 0usize; // next unemitted old line
    let mut new_pos = 0usize;
    let mut emitted_any = false;
    for (idx, (before, after)) in changes.iter().enumerate() {
        let (b0, b1) = (before.start as usize, before.end as usize);
        let (a0, a1) = (after.start as usize, after.end as usize);
        // Context gap before this hunk.
        let ctx_start = b0.saturating_sub(context);
        if ctx_start > old_pos {
            if emitted_any || old_pos > 0 {
                append_git_patch_record(
                    records,
                    &GitPatchRecord::Gap {
                        old_line: (old_pos + 1) as u32,
                        new_line: (new_pos + 1) as u32,
                    },
                );
            }
            new_pos += ctx_start - old_pos;
            old_pos = ctx_start;
        } else if !emitted_any && ctx_start > 0 && old_pos == 0 {
            new_pos = a0.saturating_sub(b0 - ctx_start);
            old_pos = ctx_start;
        }
        // Leading context rows.
        while old_pos < b0 {
            append_row(records, &old_lines, &new_lines, old_pos, new_pos, &[], &[]);
            old_pos += 1;
            new_pos += 1;
        }
        // Changed block: pair rows up, then one-sided remainders.
        let pairs = (b1 - b0).min(a1 - a0);
        for i in 0..pairs {
            let (old_spans, new_spans) = if no_spans {
                (Vec::new(), Vec::new())
            } else {
                intraline_spans(old_lines[b0 + i], new_lines[a0 + i], char_spans, ws)
            };
            append_row(
                records,
                &old_lines,
                &new_lines,
                b0 + i,
                a0 + i,
                &old_spans,
                &new_spans,
            );
        }
        for i in (b0 + pairs)..b1 {
            append_one_sided(records, Some((&old_lines, i)), None);
        }
        for i in (a0 + pairs)..a1 {
            append_one_sided(records, None, Some((&new_lines, i)));
        }
        old_pos = b1;
        new_pos = a1;
        // Trailing context rows — never cross into the next hunk's changed
        // block, or those changed lines would be emitted twice (once here
        // as span-less "unchanged", once by the next hunk).
        let next_b0 = changes
            .get(idx + 1)
            .map(|(b, _)| b.start as usize)
            .unwrap_or(old_lines.len());
        let ctx_end = (b1 + context).min(old_lines.len()).min(next_b0);
        while old_pos < ctx_end && new_pos < new_lines.len() {
            append_row(records, &old_lines, &new_lines, old_pos, new_pos, &[], &[]);
            old_pos += 1;
            new_pos += 1;
        }
        emitted_any = true;
    }
}

fn append_row(
    records: &mut Vec<u8>,
    old_lines: &[&[u8]],
    new_lines: &[&[u8]],
    old_idx: usize,
    new_idx: usize,
    old_spans: &[(u32, u32)],
    new_spans: &[(u32, u32)],
) {
    append_git_patch_record(
        records,
        &GitPatchRecord::Row {
            old_line: (old_idx + 1) as u32,
            new_line: (new_idx + 1) as u32,
            old_text: old_lines.get(old_idx).copied().unwrap_or(b""),
            new_text: new_lines.get(new_idx).copied().unwrap_or(b""),
            old_spans: old_spans.to_vec(),
            new_spans: new_spans.to_vec(),
        },
    );
}

fn append_one_sided(
    records: &mut Vec<u8>,
    old: Option<(&[&[u8]], usize)>,
    new: Option<(&[&[u8]], usize)>,
) {
    let (old_line, old_text) = old
        .map(|(lines, idx)| ((idx + 1) as u32, lines[idx]))
        .unwrap_or((0, b"".as_slice()));
    let (new_line, new_text) = new
        .map(|(lines, idx)| ((idx + 1) as u32, lines[idx]))
        .unwrap_or((0, b"".as_slice()));
    let full_span = |text: &[u8]| -> Vec<(u32, u32)> {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![(0, text.len() as u32)]
        }
    };
    append_git_patch_record(
        records,
        &GitPatchRecord::Row {
            old_line,
            new_line,
            old_text,
            new_text,
            old_spans: full_span(old_text),
            new_spans: full_span(new_text),
        },
    );
}

/// Word (default) or character tokens of one line, as byte ranges.
fn tokenize(line: &[u8], char_level: bool) -> Vec<Range<usize>> {
    if char_level {
        return (0..line.len()).map(|i| i..i + 1).collect();
    }
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let class = |b: u8| -> u8 {
        if b.is_ascii_alphanumeric() || b == b'_' {
            0
        } else if b == b' ' || b == b'\t' {
            1
        } else {
            2 // single punctuation
        }
    };
    let mut i = 0;
    while i < line.len() {
        let c = class(line[i]);
        let run_end = if c == 2 {
            i + 1
        } else {
            let mut j = i + 1;
            while j < line.len() && class(line[j]) == c {
                j += 1;
            }
            j
        };
        let _ = start;
        start = i;
        tokens.push(start..run_end);
        i = run_end;
    }
    tokens
}

/// Byte spans within one line, `(start, len)` pairs.
type Spans = Vec<(u32, u32)>;

/// Changed-byte spans within one modified line pair. With an
/// ignore-whitespace mode, spans covering only whitespace are dropped so
/// the pair renders as unchanged where only spacing moved.
fn intraline_spans(old_line: &[u8], new_line: &[u8], char_level: bool, ws: u8) -> (Spans, Spans) {
    use imara_diff::{Algorithm, Diff, InternedInput, Interner};
    let old_tokens = tokenize(old_line, char_level);
    let new_tokens = tokenize(new_line, char_level);
    // Manual interning: token = one byte range's slice.
    let mut input: InternedInput<&[u8]> = InternedInput {
        before: Vec::new(),
        after: Vec::new(),
        interner: Interner::new(old_tokens.len() + new_tokens.len()),
    };
    for range in &old_tokens {
        let token = input.interner.intern(&old_line[range.clone()]);
        input.before.push(token);
    }
    for range in &new_tokens {
        let token = input.interner.intern(&new_line[range.clone()]);
        input.after.push(token);
    }
    let diff = Diff::compute(Algorithm::Histogram, &input);
    let mut old_spans: Vec<(u32, u32)> = Vec::new();
    let mut new_spans: Vec<(u32, u32)> = Vec::new();
    let ws_only = |line: &[u8], range: &Range<usize>| -> bool {
        ws != 0 && line[range.clone()].iter().all(|b| b" \t\r".contains(b))
    };
    let push =
        |tokens: &[Range<usize>], line: &[u8], range: Range<u32>, spans: &mut Vec<(u32, u32)>| {
            let (start, end) = (range.start as usize, range.end as usize);
            if start >= end {
                return;
            }
            let byte_start = tokens[start].start;
            let byte_end = tokens[end - 1].end;
            if ws_only(line, &(byte_start..byte_end)) {
                return;
            }
            // Merge adjacent spans.
            if let Some(last) = spans.last_mut()
                && (last.0 + last.1) as usize == byte_start
            {
                last.1 += (byte_end - byte_start) as u32;
            } else {
                spans.push((byte_start as u32, (byte_end - byte_start) as u32));
            }
        };
    for hunk in diff.hunks() {
        push(&old_tokens, old_line, hunk.before, &mut old_spans);
        push(&new_tokens, new_line, hunk.after, &mut new_spans);
    }
    (old_spans, new_spans)
}

/// Minimal unified-diff text for `TEXT` mode consumers.
#[allow(clippy::too_many_arguments)]
fn append_text_patch(
    out: &mut Vec<u8>,
    old_path: &[u8],
    change: &Change,
    binary: bool,
    old_bytes: &[u8],
    new_bytes: &[u8],
    context: usize,
    ws: u8,
) {
    let new_path = change_new_path(change);
    let old_name: Vec<u8> = if old_path.is_empty() {
        new_path.clone()
    } else {
        old_path.to_vec()
    };
    out.extend_from_slice(
        format!(
            "diff --git a/{} b/{}\n",
            crate::escape_bstr(&old_name),
            crate::escape_bstr(&new_path)
        )
        .as_bytes(),
    );
    if binary {
        out.extend_from_slice(b"Binary files differ\n");
        return;
    }
    let a_label = if change.old.is_some() {
        format!("a/{}", crate::escape_bstr(&old_name))
    } else {
        "/dev/null".to_string()
    };
    let b_label = if change.new.is_some() {
        format!("b/{}", crate::escape_bstr(&new_path))
    } else {
        "/dev/null".to_string()
    };
    out.extend_from_slice(format!("--- {a_label}\n+++ {b_label}\n").as_bytes());
    let old_lines = split_lines(old_bytes);
    let new_lines = split_lines(new_bytes);
    // A final line lacking its newline takes git's "\ No newline at end of
    // file" marker, so the patch round-trips through `git apply` byte for
    // byte. `*_last` is a sentinel index no valid line matches when empty.
    let old_no_nl = old_bytes.last().is_some_and(|&b| b != b'\n');
    let new_no_nl = new_bytes.last().is_some_and(|&b| b != b'\n');
    let old_last = old_lines.len().wrapping_sub(1);
    let new_last = new_lines.len().wrapping_sub(1);
    let changes = line_changes(old_bytes, new_bytes, ws);
    // Group changes whose context windows touch into one @@ hunk, so the
    // emitted hunks never overlap (overlapping hunks are what `git apply`
    // rejects). Two changes merge when at most 2*context lines separate them.
    let mut i = 0;
    while i < changes.len() {
        let mut j = i;
        while j + 1 < changes.len() {
            let prev_end = changes[j].0.end as usize;
            let next_start = changes[j + 1].0.start as usize;
            if next_start <= prev_end + 2 * context {
                j += 1;
            } else {
                break;
            }
        }
        let group = &changes[i..=j];
        let first_b0 = group[0].0.start as usize;
        let last_b1 = group[group.len() - 1].0.end as usize;
        let ctx_start = first_b0.saturating_sub(context);
        let ctx_end = (last_b1 + context).min(old_lines.len());
        // New-side start aligns to old ctx_start; new count = old count plus
        // the net line delta of every change in the group.
        let new_start = (group[0].1.start as usize).saturating_sub(first_b0 - ctx_start);
        let old_count = ctx_end - ctx_start;
        let net: isize = group
            .iter()
            .map(|(b, a)| (a.end - a.start) as isize - (b.end - b.start) as isize)
            .sum();
        let new_count = (old_count as isize + net) as usize;
        out.extend_from_slice(
            format!(
                "@@ -{},{} +{},{} @@\n",
                ctx_start + 1,
                old_count,
                new_start + 1,
                new_count,
            )
            .as_bytes(),
        );
        let emit = |out: &mut Vec<u8>, prefix: u8, line: &[u8], no_nl: bool| {
            out.push(prefix);
            out.extend_from_slice(line);
            out.push(b'\n');
            if no_nl {
                out.extend_from_slice(b"\\ No newline at end of file\n");
            }
        };
        // Context lines are identical on both sides, so tracking the old
        // position alone suffices for emission (the new-side counts are in
        // the header).
        let mut old_pos = ctx_start;
        let _ = new_start;
        for (before, after) in group {
            let (b0, b1) = (before.start as usize, before.end as usize);
            let (a0, a1) = (after.start as usize, after.end as usize);
            // Inner/leading context up to this change.
            while old_pos < b0 {
                emit(
                    out,
                    b' ',
                    old_lines[old_pos],
                    old_no_nl && old_pos == old_last,
                );
                old_pos += 1;
            }
            for (off, line) in old_lines[b0..b1].iter().copied().enumerate() {
                emit(out, b'-', line, old_no_nl && b0 + off == old_last);
            }
            for (off, line) in new_lines[a0..a1].iter().copied().enumerate() {
                emit(out, b'+', line, new_no_nl && a0 + off == new_last);
            }
            old_pos = b1;
        }
        // Trailing context.
        while old_pos < ctx_end {
            emit(
                out,
                b' ',
                old_lines[old_pos],
                old_no_nl && old_pos == old_last,
            );
            old_pos += 1;
        }
        i = j + 1;
    }
}

/// STATUS records for the state stream: staged = HEAD×INDEX, unstaged =
/// INDEX×WORKTREE, joined by path; conflicts from index stages.
/// `untracked`/`ignored` are the engine's superset demand across
/// subscribers; `caches` carries the engine's HEAD-flatten memo and
/// worktree stat cache.
pub(crate) fn append_status_records(
    repo: &gix::Repository,
    untracked: bool,
    ignored: bool,
    budgets: &Budgets,
    caches: &mut StatusCaches,
    records: &mut Vec<u8>,
    flags: &mut u8,
) {
    let cancel = Cancel::default();
    let head_ep = match repo.head_id() {
        Ok(id) => GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: oid_bytes(id.as_ref()),
        },
        Err(_) => GitEndpoint {
            kind: GIT_ENDPOINT_EMPTY,
            oid: GIT_OID_NONE,
        },
    };
    let index_ep = GitEndpoint {
        kind: GIT_ENDPOINT_INDEX,
        oid: GIT_OID_NONE,
    };
    let worktree_ep = GitEndpoint {
        kind: GIT_ENDPOINT_WORKTREE,
        oid: GIT_OID_NONE,
    };
    let truncated = std::cell::Cell::new(false);
    let StatusCaches { head_flat, stats } = caches;
    // The HEAD flatten is memoized by tree oid: a worktree-driven
    // recompute with an unmoved HEAD reuses the previous decode.
    let head_tree = (head_ep.kind == GIT_ENDPOINT_COMMIT)
        .then(|| {
            repo.find_object(oid_from_wire(repo, &head_ep.oid))
                .ok()
                .and_then(|object| object.peel_to_tree().ok())
                .map(|tree| tree.id)
        })
        .flatten();
    let head_flat: &Flat = match head_tree {
        Some(tree_id) if head_flat.as_ref().is_some_and(|(id, _)| *id == tree_id) => {
            &head_flat.as_ref().expect("checked above").1
        }
        cached => {
            let Ok(flat) = flatten(
                repo, &head_ep, b"", false, ignored, budgets, &cancel, &truncated, None,
            ) else {
                return;
            };
            // An unborn/empty HEAD memoizes under the null oid: it never
            // matches a real tree, and the empty flatten is free anyway.
            let key = cached.unwrap_or_else(|| zero_id(repo));
            &head_flat.insert((key, flat)).1
        }
    };
    let index_flat = match flatten(
        repo, &index_ep, b"", false, ignored, budgets, &cancel, &truncated, None,
    ) {
        Ok(flat) => flat,
        Err(_) => return,
    };
    let worktree_flat = match flatten(
        repo,
        &worktree_ep,
        b"",
        untracked,
        ignored,
        budgets,
        &cancel,
        &truncated,
        Some(stats),
    ) {
        Ok(flat) => flat,
        Err(_) => return,
    };
    if truncated.get() {
        *flags |= GIT_STATE_STATUS_TRUNCATED;
    }
    let workdir = repo.workdir().map(|p| p.to_path_buf());
    let input_cap = budgets.blob_max.min(blit_remote::MAX_DECOMPRESSED as u64);
    let (Ok(staged), Ok(unstaged)) = (
        diff_flats(
            repo,
            workdir.as_deref(),
            head_flat,
            &index_flat,
            0,
            true,
            input_cap,
            &cancel,
            None,
        ),
        diff_flats(
            repo,
            workdir.as_deref(),
            &index_flat,
            &worktree_flat,
            0,
            false,
            input_cap,
            &cancel,
            Some(stats),
        ),
    ) else {
        return;
    };
    // Entries for paths no longer on disk (or no longer tracked) are dead.
    stats.retain(|path, _| worktree_flat.contains_key(path));

    // Conflicted paths (any non-zero stage in the index).
    let mut conflicted: std::collections::HashSet<Vec<u8>> = Default::default();
    if let Ok(index) = repo.index_or_empty() {
        for entry in index.entries() {
            if entry.stage() != gix::index::entry::Stage::Unconflicted {
                conflicted.insert(entry.path(&index).to_vec());
            }
        }
    }

    // Join staged and unstaged by (new-side) path.
    #[derive(Default)]
    struct Cell {
        staged: u8,
        unstaged: u8,
        old_path: Vec<u8>,
    }
    let mut cells: BTreeMap<Vec<u8>, Cell> = BTreeMap::new();
    for change in &staged {
        let (old_path, new_path) = rename_paths(change);
        let cell = cells.entry(new_path).or_default();
        cell.staged = change.st;
        cell.old_path = old_path;
    }
    for change in &unstaged {
        let (_, new_path) = rename_paths(change);
        let untracked_entry = change.st == b'A'
            && change
                .new
                .as_ref()
                .is_some_and(|side| side.worktree && side.oid.is_null())
            && !index_flat.contains_key(&new_path);
        let cell = cells.entry(new_path).or_default();
        if untracked_entry {
            // Ignored files carry '!'; plain untracked carry '?'
            // (docs/git.md STATUS record porcelain letters).
            let letter = if change.new.as_ref().is_some_and(|s| s.ignored) {
                b'!'
            } else {
                b'?'
            };
            // Don't clobber a staged change already recorded for this path:
            // deleting a tracked file then recreating it leaves the index
            // deletion (staged 'D') beside a new untracked file. git reports
            // both; keep the staged column and only mark the worktree side.
            if cell.staged == 0 {
                cell.staged = letter;
            }
            cell.unstaged = letter;
        } else {
            cell.unstaged = change.st;
        }
    }
    for path in &conflicted {
        let cell = cells.entry(path.clone()).or_default();
        cell.staged = b'U';
        cell.unstaged = b'U';
    }

    for (count, (path, cell)) in cells.iter().enumerate() {
        if count >= budgets.entries_max {
            *flags |= GIT_STATE_STATUS_TRUNCATED;
            break;
        }
        let entry_flags = if conflicted.contains(path) {
            GIT_STATUS_ENTRY_CONFLICTED
        } else {
            0
        };
        append_git_state_record(
            records,
            &GitStateRecord::Status {
                staged: if cell.staged == 0 { b' ' } else { cell.staged },
                unstaged: if cell.unstaged == 0 {
                    b' '
                } else {
                    cell.unstaged
                },
                flags: entry_flags,
                old_path: &crate::escape_bstr(&cell.old_path),
                path: &crate::escape_bstr(path),
            },
        );
    }
}
