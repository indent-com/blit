//! Stateless request handlers: log, tree, blob, merge-base (docs/git.md).
//! Each takes the wire request, does bounded work against a thread-local
//! repository, and returns the complete response message.

use blit_remote::git::{
    GIT_BLOB_WHOLE, GIT_COMMIT_LOSSY_ENCODING, GIT_COMMITS_MORE, GIT_LOG_FIRST_PARENT,
    GIT_LOG_FOLLOW, GIT_LOG_FULL_MESSAGE, GIT_LOG_PATH_OIDS, GIT_LOG_TOPO, GIT_OID_NONE,
    GIT_OTYPE_BLOB, GIT_OTYPE_COMMIT, GIT_OTYPE_TREE, GIT_STATUS_BUDGET, GIT_STATUS_CANCELLED,
    GIT_STATUS_INVALID, GIT_STATUS_NOT_FOUND, GIT_STATUS_OK, GIT_STATUS_OTHER,
    GIT_STATUS_TOO_LARGE, GIT_STATUS_WRONG_TYPE, GIT_TREE_TRUNCATED, GitBlobRequest,
    GitCommitRecord, GitLogRequest, GitOid, GitTreeRecord, GitTreeRequest,
    append_git_commit_record, append_git_tree_record, msg_git_base_resp, msg_git_blob_resp,
    msg_git_commits, msg_git_resolve_resp, msg_git_tree_resp,
};

use crate::{Budgets, Cancel, RepoHandle, commit_text, is_zero_oid, oid_bytes, oid_from_wire};

impl RepoHandle {
    /// `GIT_LOG`: commits in `hides..tips`, paginated via a stateless
    /// frontier. Budget exhaustion returns the partial page with `MORE`.
    pub fn log(&self, req: &GitLogRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8| msg_git_commits(req.nonce, status, 0, &[], &[]);

        // Reject undefined flag bits (docs/git.md: INVALID on unknown flags).
        const KNOWN_LOG_FLAGS: u8 = GIT_LOG_FIRST_PARENT
            | GIT_LOG_TOPO
            | GIT_LOG_FULL_MESSAGE
            | GIT_LOG_FOLLOW
            | GIT_LOG_PATH_OIDS;
        if req.flags & !KNOWN_LOG_FLAGS != 0 {
            return fail(GIT_STATUS_INVALID);
        }

        let mut tips: Vec<gix::ObjectId> = Vec::new();
        if req.tips.is_empty() {
            match repo.head_id() {
                Ok(id) => tips.push(id.detach()),
                // Unborn branch: an empty log, not an error.
                Err(_) => return msg_git_commits(req.nonce, GIT_STATUS_OK, 0, &[], &[]),
            }
        } else {
            for oid in &req.tips {
                tips.push(oid_from_wire(&repo, oid));
            }
        }
        let hides: Vec<gix::ObjectId> = req
            .hides
            .iter()
            .map(|oid| oid_from_wire(&repo, oid))
            .collect();
        let limit = if req.limit == 0 {
            self.budgets.log_default
        } else {
            (req.limit as usize).min(self.budgets.log_max)
        };
        let path_filter = if req.path.is_empty() {
            None
        } else {
            match crate::unescape_wire(req.path) {
                Some(bytes) => Some(bytes),
                None => return fail(GIT_STATUS_OTHER),
            }
        };
        match walk_log(
            &repo,
            tips,
            hides,
            req.flags,
            limit,
            path_filter,
            &self.budgets,
            cancel,
        ) {
            Ok((records, frontier, more)) => {
                let flags = if more { GIT_COMMITS_MORE } else { 0 };
                msg_git_commits(req.nonce, GIT_STATUS_OK, flags, &frontier, &records)
            }
            Err(status) => fail(status),
        }
    }

    /// `GIT_TREE`: one level of a tree, oid peeled and `path` descended.
    pub fn tree(&self, req: &GitTreeRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let nonce = req.nonce;
        let repo = self.local();
        let fail = |status: u8| msg_git_tree_resp(nonce, status, 0, &[]);
        if req.flags != 0 {
            return fail(GIT_STATUS_INVALID);
        }
        let tree = match resolve_tree(&repo, &req.oid, req.path) {
            Ok(tree) => tree,
            Err(status) => return fail(status),
        };
        // Resuming a truncated listing: tree entries are already in git's
        // own order, which the cursor makes normative.
        let after = match crate::unescape_wire(req.after) {
            Some(bytes) => bytes,
            None => return fail(GIT_STATUS_OTHER),
        };
        let mut records = Vec::new();
        let mut flags = 0u8;
        let mut count = 0usize;
        let mut last = Vec::new();
        for entry in tree.iter() {
            if cancel.is_cancelled() {
                return fail(GIT_STATUS_CANCELLED);
            }
            let Ok(entry) = entry else {
                return fail(GIT_STATUS_OTHER);
            };
            let filename = entry.filename();
            let mode = entry.mode();
            // The cursor is compared in the order the entries arrive in,
            // which is git's: a subtree sorts as if its name ended in `/`.
            // Comparing raw names instead disagrees with that order wherever
            // a blob and a subtree share a prefix — `lib.rs` sorts before
            // `lib` in a tree (`.` is 0x2e, `/` is 0x2f) but after it
            // bytewise — and a page boundary landing between them silently
            // dropped the subtree from the listing.
            let key = git_order_key(filename.as_ref(), mode);
            if !after.is_empty() && key <= after {
                continue;
            }
            if count >= self.budgets.entries_max || records.len() >= self.budgets.bytes_max {
                flags |= GIT_TREE_TRUNCATED;
                append_git_tree_record(
                    &mut records,
                    &GitTreeRecord::Cursor {
                        after: &crate::escape_bstr(&last),
                        pos: 0,
                    },
                );
                break;
            }
            count += 1;
            last = key;
            let name = crate::escape_bstr(filename);
            append_git_tree_record(
                &mut records,
                &GitTreeRecord::Entry {
                    otype: otype_of_mode(mode.value() as u32),
                    mode: mode.value() as u32,
                    oid: oid_bytes(entry.oid()),
                    name: &name,
                },
            );
        }
        msg_git_tree_resp(nonce, GIT_STATUS_OK, flags, &records)
    }

    /// `GIT_BLOB`: object bytes from `offset`, size-capped, cache-forever.
    ///
    /// A window is the default: a viewer that would happily render the head
    /// of a 20 MiB generated file gets it, where the whole-object-only read
    /// gave it nothing at all. `WHOLE` asks for the old behavior — the
    /// entire object or `TOO_LARGE` — for a caller that must hash or parse
    /// the file and gains nothing from a prefix.
    pub fn blob(&self, req: &GitBlobRequest<'_>) -> Vec<u8> {
        let nonce = req.nonce;
        let (oid, path, max_len) = (&req.oid, req.path, req.max_len);
        let repo = self.local();
        let fail = |status: u8, size: u64| msg_git_blob_resp(nonce, status, size, &[]);
        if req.flags & !GIT_BLOB_WHOLE != 0 {
            return fail(GIT_STATUS_INVALID, 0);
        }
        let blob_id = if path.is_empty() {
            oid_from_wire(&repo, oid)
        } else {
            match resolve_tree_entry(&repo, oid, path) {
                Ok((_mode, id)) => id,
                Err(status) => return fail(status, 0),
            }
        };
        // Header before object: TOO_LARGE must report the true size
        // (docs/git.md) without ever materializing an over-cap blob.
        let header = match repo.find_header(blob_id) {
            Ok(header) => header,
            Err(_) => return fail(GIT_STATUS_NOT_FOUND, 0),
        };
        if header.kind() != gix::object::Kind::Blob {
            return fail(GIT_STATUS_WRONG_TYPE, 0);
        }
        let size = header.size();
        let cap = if max_len == 0 {
            self.budgets.blob_max
        } else {
            u64::from(max_len).min(self.budgets.blob_max)
        }
        .min(blit_remote::MAX_DECOMPRESSED as u64);
        // `size` is always the true object size, so a client always knows
        // how much of the object it is holding.
        if req.offset > size {
            return fail(GIT_STATUS_INVALID, size);
        }
        // Without WHOLE there is no refusal: the client gets what fits from
        // `offset` and walks, comparing `offset + data.len()` against `size`
        // to know when it is done.
        if req.flags & GIT_BLOB_WHOLE != 0 && size > cap {
            return fail(GIT_STATUS_TOO_LARGE, size);
        }
        match repo.find_object(blob_id) {
            Ok(obj) => {
                let start = req.offset as usize;
                let end = start.saturating_add(cap as usize).min(obj.data.len());
                let window = obj.data.get(start..end).unwrap_or(&[]);
                msg_git_blob_resp(nonce, GIT_STATUS_OK, size, window)
            }
            Err(_) => fail(GIT_STATUS_NOT_FOUND, 0),
        }
    }

    /// `GIT_BASE`: merge base of two or more commits, best-first.
    pub fn base(&self, nonce: u16, oids: &[GitOid], cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8| msg_git_base_resp(nonce, status, &[]);
        if oids.len() < 2 {
            return fail(GIT_STATUS_OTHER);
        }
        let ids: Vec<gix::ObjectId> = oids.iter().map(|o| oid_from_wire(&repo, o)).collect();
        for id in &ids {
            match repo.find_header(*id) {
                Ok(header) if header.kind() == gix::object::Kind::Commit => {}
                Ok(_) => return fail(GIT_STATUS_WRONG_TYPE),
                Err(_) => return fail(GIT_STATUS_NOT_FOUND),
            }
        }
        // Octopus: fold pairwise. Disjoint histories yield an empty list.
        let mut base = ids[0];
        for id in &ids[1..] {
            match bounded_merge_base(
                &repo,
                &self.merge_memo,
                base,
                *id,
                self.budgets.walk_max,
                cancel,
            ) {
                Ok(Some(found)) => base = found,
                Ok(None) => return msg_git_base_resp(nonce, GIT_STATUS_OK, &[]),
                Err(status) => return fail(status),
            }
        }
        msg_git_base_resp(nonce, GIT_STATUS_OK, &[oid_bytes(base.as_ref())])
    }
}

/// Resolve a revision spec to `(tips, hides)` commit oids, ready for a log
/// walk. Handles a single rev, `A..B` (range), `A...B` (symmetric via
/// merge-base), and the parent forms. Each endpoint is peeled to a commit.
/// Resolve a whitespace-separated list of revision specs, merging tips
/// and hides across tokens — `base..a b ^c` composes exactly like the
/// git CLI's rev-list arguments, so one spec string can log from a base
/// to multiple heads. Each token keeps every gix spec form.
pub(crate) fn resolve_spec(
    repo: &gix::Repository,
    memo: &MergeMemo,
    spec: &str,
    budget: usize,
    cancel: &Cancel,
) -> Result<(Vec<gix::ObjectId>, Vec<gix::ObjectId>), u8> {
    let mut tokens = spec.split_whitespace();
    let first = tokens.next().unwrap_or(spec);
    let (mut tips, mut hides) = resolve_one(repo, memo, first, budget, cancel)?;
    for token in tokens {
        let (t, h) = resolve_one(repo, memo, token, budget, cancel)?;
        tips.extend(t);
        hides.extend(h);
    }
    tips.dedup();
    hides.dedup();
    Ok((tips, hides))
}

fn resolve_one(
    repo: &gix::Repository,
    memo: &MergeMemo,
    spec: &str,
    budget: usize,
    cancel: &Cancel,
) -> Result<(Vec<gix::ObjectId>, Vec<gix::ObjectId>), u8> {
    use gix::revision::plumbing::Spec;
    let parsed = repo
        .rev_parse(spec)
        .map_err(|_| GIT_STATUS_NOT_FOUND)?
        .detach();
    // Peel an object to a commit; non-committish specs are WRONG_TYPE.
    let commit = |id: gix::ObjectId| -> Result<gix::ObjectId, u8> {
        repo.find_object(id)
            .map_err(|_| GIT_STATUS_NOT_FOUND)?
            .peel_to_kind(gix::object::Kind::Commit)
            .map(|o| o.id)
            .map_err(|_| GIT_STATUS_WRONG_TYPE)
    };
    // Commit oids of an object's parents (for the `^@` / `^!` forms).
    let parents = |id: gix::ObjectId| -> Result<Vec<gix::ObjectId>, u8> {
        let c = commit(id)?;
        Ok(repo
            .find_commit(c)
            .map_err(|_| GIT_STATUS_NOT_FOUND)?
            .parent_ids()
            .map(|p| p.detach())
            .collect())
    };
    match parsed {
        Spec::Include(a) => Ok((vec![commit(a)?], vec![])),
        Spec::Exclude(a) => Ok((vec![], vec![commit(a)?])),
        Spec::Range { from, to } => Ok((vec![commit(to)?], vec![commit(from)?])),
        Spec::Merge { theirs, ours } => {
            let (t, o) = (commit(theirs)?, commit(ours)?);
            // `A...B` hides ALL merge bases, so its symmetric difference is
            // right even in criss-cross histories with more than one base.
            let bases = bounded_merge_bases(repo, memo, t, o, budget, cancel)?;
            Ok((vec![t, o], bases))
        }
        Spec::IncludeOnlyParents(a) => Ok((parents(a)?, vec![])),
        // `a^!` is `a` with all its parents hidden — reachable set {a}.
        Spec::ExcludeParents(a) => Ok((vec![commit(a)?], parents(a)?)),
    }
}

impl RepoHandle {
    /// `GIT_RESOLVE`: turn a revision spec into log tips/hides.
    pub fn resolve(&self, nonce: u16, spec: &str, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        match resolve_spec(&repo, &self.merge_memo, spec, self.budgets.walk_max, cancel) {
            Ok((tips, hides)) => {
                let tips: Vec<GitOid> = tips.iter().map(|o| oid_bytes(o.as_ref())).collect();
                let hides: Vec<GitOid> = hides.iter().map(|o| oid_bytes(o.as_ref())).collect();
                msg_git_resolve_resp(nonce, GIT_STATUS_OK, &tips, &hides)
            }
            Err(status) => msg_git_resolve_resp(nonce, status, &[], &[]),
        }
    }
}

/// Merge bases memoized by oid pair (docs/design/git.md): the state
/// engine re-resolves `A...B` specs on every ref settle and diff/patch
/// resolve MERGE_BASE endpoints per request, all against immutable
/// history, so a pair's answer never changes. Bounded: a full map resets
/// rather than grows — recomputation is cheap with the early-terminating
/// walk below.
#[derive(Default)]
pub(crate) struct MergeMemo(
    std::sync::Mutex<std::collections::HashMap<(gix::ObjectId, gix::ObjectId), Vec<gix::ObjectId>>>,
);

impl MergeMemo {
    const CAP: usize = 1024;

    fn get(&self, key: &(gix::ObjectId, gix::ObjectId)) -> Option<Vec<gix::ObjectId>> {
        self.0.lock().unwrap().get(key).cloned()
    }

    fn put(&self, key: (gix::ObjectId, gix::ObjectId), bases: Vec<gix::ObjectId>) {
        let mut map = self.0.lock().unwrap();
        if map.len() >= Self::CAP {
            map.clear();
        }
        map.insert(key, bases);
    }
}

// Ancestry paint flags for the interleaved merge-base walk (the shape of
// git's own paint_down_to_common): which seed reaches a commit, whether
// the region below a found base is settled, and whether the commit was
// already reported.
const BASE_P1: u8 = 1;
const BASE_P2: u8 = 2;
const BASE_STALE: u8 = 4;
const BASE_RESULT: u8 = 8;

/// The `(commit time, parents)` of a commit, memoized — the paint walk
/// touches a commit once for its time and once for its parents.
fn commit_info(
    repo: &gix::Repository,
    cache: &mut std::collections::HashMap<gix::ObjectId, (i64, Vec<gix::ObjectId>)>,
    id: gix::ObjectId,
) -> Result<(i64, Vec<gix::ObjectId>), u8> {
    if let Some(found) = cache.get(&id) {
        return Ok(found.clone());
    }
    let commit = repo.find_commit(id).map_err(|_| GIT_STATUS_OTHER)?;
    let time = commit.time().map(|t| t.seconds).unwrap_or(0);
    let parents: Vec<gix::ObjectId> = commit.parent_ids().map(|p| p.detach()).collect();
    cache.insert(id, (time, parents.clone()));
    Ok((time, parents))
}

/// Candidate merge bases of `a` and `b`: an interleaved newest-first walk
/// from both seeds that stops once every queued commit sits below a found
/// base — neither side's full ancestry is ever materialized. Results come
/// newest-first and may still contain redundant entries (an ancestor of
/// another candidate) in criss-cross histories; callers reduce them.
/// Capped at `budget` visited commits and cancellable (docs/git.md walk
/// budget); like git itself, ordering leans on commit times, so extreme
/// clock skew can degrade the answer, never the bounds.
fn paint_bases(
    repo: &gix::Repository,
    a: gix::ObjectId,
    b: gix::ObjectId,
    budget: usize,
    cancel: &Cancel,
) -> Result<Vec<gix::ObjectId>, u8> {
    if a == b {
        return Ok(vec![a]);
    }
    let mut info: std::collections::HashMap<gix::ObjectId, (i64, Vec<gix::ObjectId>)> =
        Default::default();
    let mut flags: std::collections::HashMap<gix::ObjectId, u8> = Default::default();
    let mut heap: std::collections::BinaryHeap<(i64, gix::ObjectId)> = Default::default();
    // Heap entries whose commit is not (yet) STALE, per oid and in total:
    // the loop runs while any such entry remains — git's
    // queue_has_nonstale, tracked incrementally instead of by scanning.
    let mut counted: std::collections::HashMap<gix::ObjectId, usize> = Default::default();
    let mut live = 0usize;
    let mut results: Vec<gix::ObjectId> = Vec::new();

    for (id, flag) in [(a, BASE_P1), (b, BASE_P2)] {
        let (time, _) = commit_info(repo, &mut info, id)?;
        flags.insert(id, flag);
        heap.push((time, id));
        *counted.entry(id).or_default() += 1;
        live += 1;
    }
    let mut visited = 0usize;
    while live > 0 {
        let Some((_, id)) = heap.pop() else { break };
        if cancel.is_cancelled() {
            return Err(GIT_STATUS_CANCELLED);
        }
        visited += 1;
        if visited > budget {
            return Err(GIT_STATUS_BUDGET);
        }
        if let Some(n) = counted.get_mut(&id) {
            *n -= 1;
            live -= 1;
            if *n == 0 {
                counted.remove(&id);
            }
        }
        let f = flags.get(&id).copied().unwrap_or(0);
        let mut pass = f & (BASE_P1 | BASE_P2 | BASE_STALE);
        if pass == (BASE_P1 | BASE_P2) {
            if f & BASE_RESULT == 0 {
                flags.insert(id, f | BASE_RESULT);
                results.push(id);
            }
            pass |= BASE_STALE;
        }
        let (_, parents) = commit_info(repo, &mut info, id)?;
        for parent in parents {
            let entry = flags.entry(parent).or_insert(0);
            if *entry & pass == pass {
                continue; // nothing new to propagate
            }
            let was_stale = *entry & BASE_STALE != 0;
            *entry |= pass;
            let now_stale = *entry & BASE_STALE != 0;
            if now_stale && !was_stale {
                live -= counted.remove(&parent).unwrap_or(0);
            }
            let (time, _) = commit_info(repo, &mut info, parent)?;
            heap.push((time, parent));
            if !now_stale {
                *counted.entry(parent).or_default() += 1;
                live += 1;
            }
        }
    }
    Ok(results)
}

/// Reduce paint candidates to the maximal ones: a candidate that is an
/// ancestor of another candidate is redundant. `paint_bases(x, y)` equals
/// exactly `[x]` iff `x` is an ancestor of `y` — the P1 seed reports
/// itself once reached from `y` and everything below it is painted STALE
/// — so the ancestry test reuses the same bounded walk.
fn reduce_bases(
    repo: &gix::Repository,
    candidates: Vec<gix::ObjectId>,
    budget: usize,
    cancel: &Cancel,
) -> Result<Vec<gix::ObjectId>, u8> {
    if candidates.len() <= 1 {
        return Ok(candidates);
    }
    let mut redundant = vec![false; candidates.len()];
    for i in 0..candidates.len() {
        for j in 0..candidates.len() {
            if i == j || redundant[i] {
                continue;
            }
            if paint_bases(repo, candidates[i], candidates[j], budget, cancel)? == [candidates[i]] {
                redundant[i] = true;
            }
        }
    }
    Ok(candidates
        .into_iter()
        .zip(redundant)
        .filter_map(|(id, dead)| (!dead).then_some(id))
        .collect())
}

/// Best merge base of `a` and `b` — the newest maximal common ancestor —
/// memoized, bounded, and cancellable (docs/git.md walk budget).
pub(crate) fn bounded_merge_base(
    repo: &gix::Repository,
    memo: &MergeMemo,
    a: gix::ObjectId,
    b: gix::ObjectId,
    budget: usize,
    cancel: &Cancel,
) -> Result<Option<gix::ObjectId>, u8> {
    Ok(bounded_merge_bases(repo, memo, a, b, budget, cancel)?
        .first()
        .copied())
}

/// All merge bases of `a` and `b` — the maximal common ancestors, best
/// (newest) first — memoized by oid pair. Backs `A...B`: hiding the full
/// set makes the symmetric difference match git when two branches share
/// more than one merge base (criss-cross merges).
pub(crate) fn bounded_merge_bases(
    repo: &gix::Repository,
    memo: &MergeMemo,
    a: gix::ObjectId,
    b: gix::ObjectId,
    budget: usize,
    cancel: &Cancel,
) -> Result<Vec<gix::ObjectId>, u8> {
    let key = if a <= b { (a, b) } else { (b, a) };
    if let Some(bases) = memo.get(&key) {
        return Ok(bases);
    }
    let candidates = paint_bases(repo, a, b, budget, cancel)?;
    let bases = reduce_bases(repo, candidates, budget, cancel)?;
    memo.put(key, bases.clone());
    Ok(bases)
}

/// Walk `hides..tips` into a `(records, frontier, more)` triple — the core
/// shared by the stateless `GIT_LOG` and the watched `GIT_LOG_PAGE`. Tips
/// and hides are already resolved to (unvalidated) oids; `limit` is the
/// clamped commit cap.
#[allow(clippy::too_many_arguments)]
pub(crate) fn walk_log(
    repo: &gix::Repository,
    tips: Vec<gix::ObjectId>,
    hides: Vec<gix::ObjectId>,
    flags: u8,
    limit: usize,
    path_filter: Option<Vec<u8>>,
    budgets: &Budgets,
    cancel: &Cancel,
) -> Result<(Vec<u8>, Vec<GitOid>, bool), u8> {
    for id in tips.iter().chain(hides.iter()) {
        match repo.find_header(*id) {
            Ok(header) if header.kind() == gix::object::Kind::Commit => {}
            Ok(_) => return Err(GIT_STATUS_WRONG_TYPE),
            Err(_) => return Err(GIT_STATUS_NOT_FOUND),
        }
    }

    let follow = flags & GIT_LOG_FOLLOW != 0;
    if follow && path_filter.is_none() {
        return Err(GIT_STATUS_OTHER);
    }
    // FOLLOW tracks a single file (docs/git.md): a directory path is
    // WRONG_TYPE. Check against the resolved tips.
    if follow && let Some(filter) = &path_filter {
        for tip in &tips {
            if let Some((mode, _)) = entry_at(repo, *tip, filter)
                && mode & 0o170000 == 0o040000
            {
                return Err(GIT_STATUS_WRONG_TYPE);
            }
        }
    }

    let mut walk = repo.rev_walk(tips);
    if flags & GIT_LOG_FIRST_PARENT != 0 {
        walk = walk.first_parent_only();
    }
    let walk = walk
        .with_hidden(hides)
        .sorting(gix::revision::walk::Sorting::ByCommitTime(
            Default::default(),
        ));
    let Ok(iter) = walk.all() else {
        return Err(GIT_STATUS_OTHER);
    };

    // The filtered path (as followed at this commit) and its tree entry.
    type PathAt = (Vec<u8>, Option<(u32, gix::ObjectId)>);
    // One page entry: a commit that passed the path filter (or any commit
    // when unfiltered), with the followed path's entry captured at match
    // time so FOLLOW adoption and TOPO reordering cannot skew PATH_OIDS.
    struct Matched {
        id: gix::ObjectId,
        parents: Vec<gix::ObjectId>,
        path_at: Option<PathAt>,
    }

    // Collect one page of MATCHING commits — the path filter runs during
    // collection, so a filtered page fills up to `limit` instead of
    // shipping mostly-empty pages, still capped by `walk_max` visited.
    // The frontier is the walk's pending boundary: parents seen that were
    // never visited (skipped commits stay skipped — they matched nothing).
    let mut visited = 0usize;
    let mut more = false;
    let mut current_path = path_filter.clone();
    let mut records: Vec<u8> = Vec::new();
    let mut page: Vec<Matched> = Vec::new();
    let mut visited_set: std::collections::HashSet<gix::ObjectId> = Default::default();
    let mut parent_seen: Vec<gix::ObjectId> = Vec::new();
    let mut parent_set: std::collections::HashSet<gix::ObjectId> = Default::default();
    // Per-page entry_at memo: a commit's tree lookup runs once, not once
    // as itself and once as its child's parent. Cleared when FOLLOW
    // adopts a new name (the memo is keyed for one path), and capped so a
    // long filtered walk stays bounded.
    let mut entry_memo: std::collections::HashMap<gix::ObjectId, Option<(u32, gix::ObjectId)>> =
        Default::default();
    const ENTRY_MEMO_CAP: usize = 8192;

    for info in iter {
        if cancel.is_cancelled() {
            return Err(GIT_STATUS_CANCELLED);
        }
        let Ok(info) = info else {
            return Err(GIT_STATUS_OTHER);
        };
        visited += 1;
        if page.len() >= limit || visited >= budgets.walk_max {
            more = true;
            break;
        }
        let parents: Vec<gix::ObjectId> = info.parent_ids.iter().copied().collect();
        visited_set.insert(info.id);
        for parent in &parents {
            if parent_set.insert(*parent) {
                parent_seen.push(*parent);
            }
        }
        let mut path_at = None;
        let mut adopt: Option<Vec<u8>> = None;
        if let Some(filter) = &current_path {
            // Path filter: only commits whose entry at the path differs
            // from their first parent's.
            if entry_memo.len() > ENTRY_MEMO_CAP {
                entry_memo.clear();
            }
            let parent_id = parents.first().copied();
            let entry = entry_at_memo(repo, &mut entry_memo, info.id, filter);
            let parent_entry =
                parent_id.and_then(|p| entry_at_memo(repo, &mut entry_memo, p, filter));
            let changed = entry.as_ref().map(|e| e.1) != parent_entry.as_ref().map(|e| e.1);
            if !changed {
                continue;
            }
            // The file exists here but not at the parent under this
            // name: a rename happened at this commit. Find its
            // pre-rename path in the parent tree and follow that for
            // older commits, so history before the rename is kept.
            if follow
                && let Some((_, blob)) = &entry
                && parent_entry.is_none()
                && let Some(pid) = parent_id
            {
                match find_blob_path(repo, Some(pid), blob, budgets.entries_max, cancel) {
                    Ok(Some(renamed)) => adopt = Some(renamed),
                    Ok(None) => {}
                    Err(status) => return Err(status),
                }
            }
            path_at = Some((filter.clone(), entry));
        }
        page.push(Matched {
            id: info.id,
            parents,
            path_at,
        });
        // Adoption applies to commits OLDER than this one; the memo is
        // path-keyed, so it resets with the name.
        if let Some(renamed) = adopt {
            current_path = Some(renamed);
            entry_memo.clear();
        }
    }

    // Topological delivery: parents never before children within the
    // page (a bounded local sort; the walk itself is date-ordered).
    // Kahn's algorithm over child→parent edges: a commit is ready once
    // every in-page child is placed.
    if flags & GIT_LOG_TOPO != 0 {
        let index_of: std::collections::HashMap<gix::ObjectId, usize> = page
            .iter()
            .enumerate()
            .map(|(idx, m)| (m.id, idx))
            .collect();
        let mut pending_children = vec![0usize; page.len()];
        for m in &page {
            for parent in &m.parents {
                if let Some(&idx) = index_of.get(parent) {
                    pending_children[idx] += 1;
                }
            }
        }
        let mut queue: std::collections::VecDeque<usize> = (0..page.len())
            .filter(|&idx| pending_children[idx] == 0)
            .collect();
        let mut order: Vec<usize> = Vec::with_capacity(page.len());
        while let Some(idx) = queue.pop_front() {
            order.push(idx);
            for parent in &page[idx].parents {
                if let Some(&pidx) = index_of.get(parent) {
                    pending_children[pidx] -= 1;
                    if pending_children[pidx] == 0 {
                        queue.push_back(pidx);
                    }
                }
            }
        }
        if order.len() == page.len() {
            let mut slots: Vec<Option<Matched>> = page.into_iter().map(Some).collect();
            page = order
                .into_iter()
                .map(|idx| slots[idx].take().expect("each index placed once"))
                .collect();
        }
        // else: a cycle cannot happen in a DAG; keep walk order as the
        // safety valve.
    }

    let mut truncated_at: Option<usize> = None;
    for (idx, matched) in page.iter().enumerate() {
        if records.len() >= budgets.bytes_max {
            truncated_at = Some(idx);
            more = true;
            break;
        }
        let Ok(commit) = repo.find_commit(matched.id) else {
            return Err(GIT_STATUS_OTHER);
        };
        if !append_commit(repo, &commit, flags, &mut records) {
            return Err(GIT_STATUS_OTHER);
        }
        if flags & GIT_LOG_PATH_OIDS != 0
            && let Some((path, entry)) = &matched.path_at
        {
            let (otype, mode, oid) = match entry {
                Some((mode, blob_id)) => (otype_of_mode(*mode), *mode, oid_bytes(blob_id.as_ref())),
                None => (GIT_OTYPE_BLOB, 0, GIT_OID_NONE),
            };
            append_git_commit_record(
                &mut records,
                &GitCommitRecord::PathAt {
                    otype,
                    mode,
                    oid,
                    path: &crate::escape_bstr(path),
                },
            );
        }
    }

    // Frontier: matched-but-unemitted commits (byte-budget cut) resume
    // from themselves; everything else resumes from the walk's pending
    // boundary — parents seen that were never visited. Extra tips that
    // are ancestors of a resumed commit are harmless: the continuation
    // walk deduplicates.
    let walked_upto = truncated_at.unwrap_or(page.len());
    let mut frontier: Vec<GitOid> = Vec::new();
    let mut seen: std::collections::HashSet<gix::ObjectId> = Default::default();
    for matched in &page[walked_upto..] {
        if seen.insert(matched.id) {
            frontier.push(oid_bytes(matched.id.as_ref()));
        }
    }
    for parent in &parent_seen {
        if !visited_set.contains(parent) && seen.insert(*parent) {
            frontier.push(oid_bytes(parent.as_ref()));
        }
    }
    if !more {
        frontier.clear();
    }
    Ok((records, frontier, more))
}

/// [`entry_at`] with a per-page memo — the filter loop looks a commit up
/// once as itself and once as its child's parent.
fn entry_at_memo(
    repo: &gix::Repository,
    memo: &mut std::collections::HashMap<gix::ObjectId, Option<(u32, gix::ObjectId)>>,
    commit: gix::ObjectId,
    path: &[u8],
) -> Option<(u32, gix::ObjectId)> {
    if let Some(found) = memo.get(&commit) {
        return *found;
    }
    let entry = entry_at(repo, commit, path);
    memo.insert(commit, entry);
    entry
}

/// The `(mode, oid)` of the entry at `path` in a commit's tree.
fn entry_at(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    path: &[u8],
) -> Option<(u32, gix::ObjectId)> {
    let tree = repo.find_commit(commit).ok()?.tree().ok()?;
    let entry = tree
        .lookup_entry_by_path(gix::path::from_byte_slice(path))
        .ok()??;
    Some((entry.mode().value() as u32, entry.oid().to_owned()))
}

/// Find a path in `commit`'s tree holding blob `blob` (rename source for
/// FOLLOW). A bounded, cancellable manual walk — the old whole-tree
/// Recorder was unbounded per rename point. `Ok(None)` when not found or
/// the budget was hit; the search only guides FOLLOW, so giving up is safe.
fn find_blob_path(
    repo: &gix::Repository,
    commit: Option<gix::ObjectId>,
    blob: &gix::ObjectId,
    budget: usize,
    cancel: &Cancel,
) -> Result<Option<Vec<u8>>, u8> {
    let Some(commit) = commit else {
        return Ok(None);
    };
    let Ok(tree) = repo
        .find_commit(commit)
        .map_err(|_| ())
        .and_then(|c| c.tree().map_err(|_| ()))
    else {
        return Ok(None);
    };
    let mut stack: Vec<(gix::Tree<'_>, Vec<u8>)> = vec![(tree, Vec::new())];
    let mut visited = 0usize;
    while let Some((tree, prefix)) = stack.pop() {
        for entry in tree.iter() {
            if cancel.is_cancelled() {
                return Err(GIT_STATUS_CANCELLED);
            }
            visited += 1;
            if visited > budget {
                return Ok(None);
            }
            let Ok(entry) = entry else {
                return Ok(None);
            };
            let mut path = prefix.clone();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(entry.filename());
            if entry.oid() == blob.as_ref() {
                return Ok(Some(path));
            }
            if entry.mode().is_tree()
                && let Ok(obj) = entry.object()
                && let Ok(sub) = obj.peel_to_tree()
            {
                stack.push((sub, path));
            }
        }
    }
    Ok(None)
}

fn append_commit(
    repo: &gix::Repository,
    commit: &gix::Commit<'_>,
    req_flags: u8,
    records: &mut Vec<u8>,
) -> bool {
    let _ = repo;
    let Ok(commit_ref) = commit.decode() else {
        return false;
    };
    let author = commit_ref.author();
    let committer = commit_ref.committer();
    // The commit's declared encoding applies to all its text.
    let enc: Option<&[u8]> = commit_ref.encoding.map(|e| e.as_ref());
    let (author_name, l1) = commit_text(author.name, enc);
    let (author_email, l2) = commit_text(author.email, enc);
    let (committer_name, l3) = commit_text(committer.name, enc);
    let (committer_email, l4) = commit_text(committer.email, enc);
    let message_bytes: &[u8] = if req_flags & GIT_LOG_FULL_MESSAGE != 0 {
        commit_ref.message
    } else {
        let msg: &[u8] = commit_ref.message;
        let end = msg.iter().position(|&b| b == b'\n').unwrap_or(msg.len());
        &msg[..end]
    };
    let (message, l5) = commit_text(message_bytes, enc);
    let lossy = l1 || l2 || l3 || l4 || l5;
    let author_time = author.time().map(|t| t.seconds).unwrap_or(0);
    let author_tz = author.time().map(|t| (t.offset / 60) as i16).unwrap_or(0);
    let committer_time = committer.time().map(|t| t.seconds).unwrap_or(0);
    let committer_tz = committer
        .time()
        .map(|t| (t.offset / 60) as i16)
        .unwrap_or(0);
    append_git_commit_record(
        records,
        &GitCommitRecord::Commit {
            flags: if lossy { GIT_COMMIT_LOSSY_ENCODING } else { 0 },
            oid: oid_bytes(commit.id().as_ref()),
            tree: oid_bytes(commit_ref.tree().as_ref()),
            parents: commit_ref
                .parents()
                .map(|p| oid_bytes(p.as_ref()))
                .collect(),
            author_time,
            author_tz,
            committer_time,
            committer_tz,
            author_name: &author_name,
            author_email: &author_email,
            committer_name: &committer_name,
            committer_email: &committer_email,
            message: &message,
        },
    );
    true
}

/// An entry's sort key in git's canonical tree order: the name, with `/`
/// appended for a subtree. Git orders a tree as if every directory name
/// ended in a slash, and the `GIT_TREE` cursor is compared in that order —
/// so the cursor carries the key, not the bare name, and a boundary between
/// `lib` and `lib.rs` resumes correctly in either arrangement.
fn git_order_key(filename: &[u8], mode: gix::object::tree::EntryMode) -> Vec<u8> {
    let mut key = filename.to_vec();
    if mode.is_tree() {
        key.push(b'/');
    }
    key
}

pub(crate) fn otype_of_mode(mode: u32) -> u8 {
    match mode & 0o170000 {
        0o040000 => GIT_OTYPE_TREE,
        0o160000 => GIT_OTYPE_COMMIT,
        _ => GIT_OTYPE_BLOB,
    }
}

/// Peel `oid` (commit/tag/tree) to a tree and descend `path`.
pub(crate) fn resolve_tree<'r>(
    repo: &'r gix::Repository,
    oid: &GitOid,
    path: &str,
) -> Result<gix::Tree<'r>, u8> {
    if is_zero_oid(oid) {
        return Err(GIT_STATUS_NOT_FOUND);
    }
    let id = oid_from_wire(repo, oid);
    let object = repo.find_object(id).map_err(|_| GIT_STATUS_NOT_FOUND)?;
    let tree = object.peel_to_tree().map_err(|_| GIT_STATUS_WRONG_TYPE)?;
    if path.is_empty() {
        return Ok(tree);
    }
    let bytes = crate::unescape_wire(path).ok_or(GIT_STATUS_OTHER)?;
    let entry = tree
        .lookup_entry_by_path(gix::path::from_byte_slice(&bytes))
        .map_err(|_| GIT_STATUS_OTHER)?
        .ok_or(GIT_STATUS_NOT_FOUND)?;
    entry
        .object()
        .map_err(|_| GIT_STATUS_NOT_FOUND)?
        .peel_to_tree()
        .map_err(|_| GIT_STATUS_WRONG_TYPE)
}

/// Resolve `oid` + non-empty `path` to the `(mode, oid)` of a tree entry.
pub(crate) fn resolve_tree_entry(
    repo: &gix::Repository,
    oid: &GitOid,
    path: &str,
) -> Result<(u32, gix::ObjectId), u8> {
    if is_zero_oid(oid) {
        return Err(GIT_STATUS_NOT_FOUND);
    }
    let id = oid_from_wire(repo, oid);
    let object = repo.find_object(id).map_err(|_| GIT_STATUS_NOT_FOUND)?;
    let tree = object.peel_to_tree().map_err(|_| GIT_STATUS_WRONG_TYPE)?;
    let bytes = crate::unescape_wire(path).ok_or(GIT_STATUS_OTHER)?;
    let entry = tree
        .lookup_entry_by_path(gix::path::from_byte_slice(&bytes))
        .map_err(|_| GIT_STATUS_OTHER)?
        .ok_or(GIT_STATUS_NOT_FOUND)?;
    Ok((entry.mode().value() as u32, entry.oid().to_owned()))
}
