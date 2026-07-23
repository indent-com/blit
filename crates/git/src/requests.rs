//! Stateless request handlers: log, tree, blob, merge-base (docs/git.md).
//! Each takes the wire request, does bounded work against a thread-local
//! repository, and returns the complete response message.

use blit_remote::git::{
    GIT_COMMIT_LOSSY_ENCODING, GIT_COMMITS_MORE, GIT_LOG_FIRST_PARENT, GIT_LOG_FOLLOW,
    GIT_LOG_FULL_MESSAGE, GIT_LOG_PATH_OIDS, GIT_LOG_TOPO, GIT_OID_NONE, GIT_OTYPE_BLOB,
    GIT_OTYPE_COMMIT, GIT_OTYPE_TREE, GIT_STATUS_BUDGET, GIT_STATUS_CANCELLED, GIT_STATUS_INVALID,
    GIT_STATUS_NOT_FOUND, GIT_STATUS_OK, GIT_STATUS_OTHER, GIT_STATUS_TOO_LARGE,
    GIT_STATUS_WRONG_TYPE, GIT_TREE_TRUNCATED, GitCommitRecord, GitLogRequest, GitOid,
    GitTreeRecord, append_git_commit_record, append_git_tree_record, msg_git_base_resp,
    msg_git_blob_resp, msg_git_commits, msg_git_resolve_resp, msg_git_tree_resp,
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
    pub fn tree(&self, nonce: u16, oid: &GitOid, path: &str, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8| msg_git_tree_resp(nonce, status, 0, &[]);
        let tree = match resolve_tree(&repo, oid, path) {
            Ok(tree) => tree,
            Err(status) => return fail(status),
        };
        let mut records = Vec::new();
        let mut flags = 0u8;
        for (count, entry) in tree.iter().enumerate() {
            if cancel.is_cancelled() {
                return fail(GIT_STATUS_CANCELLED);
            }
            let Ok(entry) = entry else {
                return fail(GIT_STATUS_OTHER);
            };
            if count >= self.budgets.entries_max || records.len() >= self.budgets.bytes_max {
                flags |= GIT_TREE_TRUNCATED;
                break;
            }
            let mode = entry.mode();
            let name = crate::escape_bstr(entry.filename());
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

    /// `GIT_BLOB`: raw object bytes, size-capped, cache-forever.
    pub fn blob(&self, nonce: u16, oid: &GitOid, path: &str, max_len: u32) -> Vec<u8> {
        let repo = self.local();
        let fail = |status: u8, size: u64| msg_git_blob_resp(nonce, status, size, &[]);
        let blob_id = if path.is_empty() {
            oid_from_wire(&repo, oid)
        } else {
            match resolve_tree_entry(&repo, oid, path) {
                Ok((_mode, id)) => id,
                Err(status) => return fail(status, 0),
            }
        };
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
        if size > cap {
            return fail(GIT_STATUS_TOO_LARGE, size);
        }
        match repo.find_object(blob_id) {
            Ok(obj) => msg_git_blob_resp(nonce, GIT_STATUS_OK, size, &obj.data),
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
            match bounded_merge_base(&repo, base, *id, self.budgets.walk_max, cancel) {
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
pub(crate) fn resolve_spec(
    repo: &gix::Repository,
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
            let base = bounded_merge_base(repo, t, o, budget, cancel)?;
            Ok((vec![t, o], base.into_iter().collect()))
        }
        Spec::IncludeOnlyParents(a) => Ok((parents(a)?, vec![])),
        Spec::ExcludeParents(a) => Ok((vec![], parents(a)?)),
    }
}

impl RepoHandle {
    /// `GIT_RESOLVE`: turn a revision spec into log tips/hides.
    pub fn resolve(&self, nonce: u16, spec: &str, cancel: &Cancel) -> Vec<u8> {
        let repo = self.local();
        match resolve_spec(&repo, spec, self.budgets.walk_max, cancel) {
            Ok((tips, hides)) => {
                let tips: Vec<GitOid> = tips.iter().map(|o| oid_bytes(o.as_ref())).collect();
                let hides: Vec<GitOid> = hides.iter().map(|o| oid_bytes(o.as_ref())).collect();
                msg_git_resolve_resp(nonce, GIT_STATUS_OK, &tips, &hides)
            }
            Err(status) => msg_git_resolve_resp(nonce, status, &[], &[]),
        }
    }
}

/// Best merge base of `a` and `b`, bounded and cancellable. Paints `a`'s
/// ancestors, then returns the newest ancestor of `b` among them (the
/// merge base for the common fast-forward/branch topology). Both walks are
/// capped at `budget` commits and check `cancel`, so a disjoint or huge
/// history cannot spin forever (docs/git.md walk budget).
pub(crate) fn bounded_merge_base(
    repo: &gix::Repository,
    a: gix::ObjectId,
    b: gix::ObjectId,
    budget: usize,
    cancel: &Cancel,
) -> Result<Option<gix::ObjectId>, u8> {
    let mut a_anc: std::collections::HashSet<gix::ObjectId> = Default::default();
    let iter = repo.rev_walk([a]).all().map_err(|_| GIT_STATUS_OTHER)?;
    for (n, item) in iter.enumerate() {
        if cancel.is_cancelled() {
            return Err(GIT_STATUS_CANCELLED);
        }
        if n >= budget {
            return Err(GIT_STATUS_BUDGET);
        }
        a_anc.insert(item.map_err(|_| GIT_STATUS_OTHER)?.id);
    }
    let iter = repo.rev_walk([b]).all().map_err(|_| GIT_STATUS_OTHER)?;
    for (n, item) in iter.enumerate() {
        if cancel.is_cancelled() {
            return Err(GIT_STATUS_CANCELLED);
        }
        if n >= budget {
            return Err(GIT_STATUS_BUDGET);
        }
        let id = item.map_err(|_| GIT_STATUS_OTHER)?.id;
        if a_anc.contains(&id) {
            return Ok(Some(id));
        }
    }
    Ok(None)
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

    // Collect one page. The frontier is the pending boundary: parents
    // of walked commits that were not themselves walked. Re-walking
    // from it with the same hides continues where this page stopped
    // (date order can duplicate across pages under extreme clock skew;
    // topological delivery never does). `page` carries id+parents in
    // delivery order and is the single source for the frontier.
    let mut visited = 0usize;
    let mut more = false;
    let mut current_path = path_filter.clone();
    let mut records: Vec<u8> = Vec::new();
    let mut page: Vec<(gix::ObjectId, Vec<gix::ObjectId>)> = Vec::new();

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
        page.push((info.id, info.parent_ids.iter().copied().collect()));
    }

    // Topological delivery: parents never before children within the
    // page (a bounded local sort; the walk itself is date-ordered).
    if flags & GIT_LOG_TOPO != 0 {
        let ids: std::collections::HashSet<gix::ObjectId> =
            page.iter().map(|(id, _)| *id).collect();
        let mut placed: std::collections::HashSet<gix::ObjectId> = Default::default();
        let mut ordered: Vec<(gix::ObjectId, Vec<gix::ObjectId>)> = Vec::new();
        // Children first: repeatedly place commits whose in-page
        // children are all placed.
        let mut children: std::collections::HashMap<gix::ObjectId, Vec<gix::ObjectId>> =
            Default::default();
        for (id, parents) in &page {
            for parent in parents {
                if ids.contains(parent) {
                    children.entry(*parent).or_default().push(*id);
                }
            }
        }
        let mut remaining = page.clone();
        while !remaining.is_empty() {
            let before = ordered.len();
            remaining.retain(|(id, parents)| {
                let ready = children
                    .get(id)
                    .map(|kids| kids.iter().all(|k| placed.contains(k)))
                    .unwrap_or(true);
                if ready {
                    placed.insert(*id);
                    ordered.push((*id, parents.clone()));
                    false
                } else {
                    true
                }
            });
            if ordered.len() == before {
                // Cycle cannot happen in a DAG; safety valve.
                ordered.append(&mut remaining);
            }
        }
        page = ordered;
    }

    let mut bytes_emitted = 0usize;
    let mut truncated_at: Option<usize> = None;
    for (idx, (id, _parents)) in page.iter().enumerate() {
        if bytes_emitted >= budgets.bytes_max {
            truncated_at = Some(idx);
            more = true;
            break;
        }
        let Ok(commit) = repo.find_commit(*id) else {
            return Err(GIT_STATUS_OTHER);
        };
        let before = records.len();
        // FOLLOW rename adoption is applied only after this commit's
        // record (and PATH_OIDS) so they use the current name.
        let mut adopt: Option<Vec<u8>> = None;
        if let Some(filter) = &current_path {
            // Path filter: only commits whose entry at the path
            // differs from their first parent's.
            let parent_id = commit.parent_ids().next().map(|p| p.detach());
            let entry = entry_at(repo, *id, filter);
            let parent_entry = parent_id.and_then(|p| entry_at(repo, p, filter));
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
        }
        if !append_commit(repo, &commit, flags, &mut records) {
            return Err(GIT_STATUS_OTHER);
        }
        if flags & GIT_LOG_PATH_OIDS != 0
            && let Some(filter) = &current_path
        {
            let (otype, mode, oid, path) = match entry_at(repo, *id, filter) {
                Some((mode, blob_id)) => (
                    otype_of_mode(mode),
                    mode,
                    oid_bytes(blob_id.as_ref()),
                    crate::escape_bstr(filter),
                ),
                None => (GIT_OTYPE_BLOB, 0, GIT_OID_NONE, crate::escape_bstr(filter)),
            };
            append_git_commit_record(
                &mut records,
                &GitCommitRecord::PathAt {
                    otype,
                    mode,
                    oid,
                    path: &path,
                },
            );
        }
        if let Some(renamed) = adopt {
            current_path = Some(renamed);
        }
        bytes_emitted += records.len() - before;
    }

    // Frontier: the walk boundary, derived from `page` — the SAME
    // vector `truncated_at` indexes, and the same (reordered) order the
    // records were delivered in. Deriving it from a differently-ordered
    // vector would slice a mismatched subset and lose/duplicate commits.
    let walked_upto = truncated_at.unwrap_or(page.len());
    let walked_set: std::collections::HashSet<gix::ObjectId> =
        page[..walked_upto].iter().map(|(id, _)| *id).collect();
    let mut frontier: Vec<GitOid> = Vec::new();
    let mut seen: std::collections::HashSet<gix::ObjectId> = Default::default();
    for (_, parents) in &page[..walked_upto] {
        for parent in parents {
            if !walked_set.contains(parent) && seen.insert(*parent) {
                frontier.push(oid_bytes(parent.as_ref()));
            }
        }
    }
    // Everything collected but not emitted (byte-budget cut) resumes
    // from itself.
    for (id, _) in &page[walked_upto..] {
        if seen.insert(*id) {
            frontier.push(oid_bytes(id.as_ref()));
        }
    }
    if !more {
        frontier.clear();
    }
    Ok((records, frontier, more))
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
