//! The `GIT_STATE` engine (docs/git.md): one thread per opened repo owning
//! the mutable-state stream. Every snapshot is complete — the client
//! obligation is "replace the map" — and pacing is coalescing: at most one
//! snapshot in flight, the latest state wins once acked.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use blit_remote::git::{
    GIT_CLOSED_BACKEND_FAILED, GIT_CLOSED_RESOURCE_LIMIT, GIT_HEAD_DETACHED, GIT_HEAD_UNBORN,
    GIT_OID_NONE, GIT_OP_BISECT, GIT_OP_CHERRY_PICK, GIT_OP_MERGE, GIT_OP_REBASE, GIT_OP_REVERT,
    GIT_REF_PEELED_VALID, GIT_REF_SYMBOLIC, GIT_STATE_REFS_TRUNCATED, GIT_STATUS_OK,
    GIT_UPSTREAM_COUNTS_VALID, GIT_UPSTREAM_GONE, GitStateRecord, append_git_state_record,
    msg_git_closed, msg_git_state,
};

use crate::{Outbox, RepoHandle, oid_bytes};

/// `GIT_CLOSED` reason for a native-watch arming failure: resource limit
/// for descriptor/watch exhaustion, backend failure otherwise.
fn watch_close_reason(err: &notify::Error) -> u8 {
    match &err.kind {
        notify::ErrorKind::MaxFilesWatch => GIT_CLOSED_RESOURCE_LIMIT,
        notify::ErrorKind::Io(e) => match e.raw_os_error() {
            Some(23) | Some(24) | Some(28) => GIT_CLOSED_RESOURCE_LIMIT,
            _ => GIT_CLOSED_BACKEND_FAILED,
        },
        _ => GIT_CLOSED_BACKEND_FAILED,
    }
}

/// Per-open state-stream options (`GIT_OPEN` flags + settle windows).
#[derive(Clone, Debug)]
pub struct StateOptions {
    /// Emit `GIT_STATE` snapshots. False for a log-only engine started
    /// solely to drive `GIT_LOG_WATCH` subscriptions.
    pub wants_state: bool,
    pub status: bool,
    pub untracked: bool,
    pub ignored: bool,
    pub tracking: bool,
    pub refs_latency: Duration,
    pub status_latency: Duration,
}

impl Default for StateOptions {
    fn default() -> Self {
        StateOptions {
            wants_state: true,
            status: false,
            untracked: false,
            ignored: false,
            tracking: false,
            refs_latency: crate::env_latency("BLIT_GIT_REFS_LATENCY_MS", 50, 1000),
            status_latency: crate::env_latency("BLIT_GIT_STATUS_LATENCY_MS", 500, 10_000),
        }
    }
}

enum StateMsg {
    Ack(u32),
    Dirty {
        status_side: bool,
    },
    /// Subscribe to a live log of `spec`.
    WatchLog {
        log_id: u16,
        flags: u8,
        limit: u16,
        spec: String,
    },
    UnwatchLog {
        log_id: u16,
    },
    LogAck {
        log_id: u16,
        update_id: u32,
    },
    Stop,
}

/// Handle to a running state engine; dropping it stops the engine.
pub struct StateHandle {
    tx: Sender<StateMsg>,
}

impl StateHandle {
    pub fn ack(&self, state_id: u32) {
        let _ = self.tx.send(StateMsg::Ack(state_id));
    }

    pub fn watch_log(&self, log_id: u16, flags: u8, limit: u16, spec: String) {
        let _ = self.tx.send(StateMsg::WatchLog {
            log_id,
            flags,
            limit,
            spec,
        });
    }

    pub fn unwatch_log(&self, log_id: u16) {
        let _ = self.tx.send(StateMsg::UnwatchLog { log_id });
    }

    pub fn log_ack(&self, log_id: u16, update_id: u32) {
        let _ = self.tx.send(StateMsg::LogAck { log_id, update_id });
    }

    pub fn stop(&self) {
        let _ = self.tx.send(StateMsg::Stop);
    }
}

impl Drop for StateHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(StateMsg::Stop);
    }
}

impl RepoHandle {
    /// Spawn the state engine: an immediate first snapshot, then snapshots
    /// after settled changes, at most one unacked at a time.
    pub fn start_state(&self, repo_id: u16, opts: StateOptions, outbox: Outbox) -> StateHandle {
        let (tx, rx) = std::sync::mpsc::channel();
        let engine = Engine {
            repo: self.clone(),
            repo_id,
            opts,
            rx,
            outbox,
            next_state_id: 1,
            unacked: None,
            state_dirty: true,
            refs_due: None,
            status_due: None,
            log_subs: Default::default(),
            _watch: None,
        };
        let watch_tx = tx.clone();
        std::thread::Builder::new()
            .name(format!("blit-git-state-{repo_id}"))
            .spawn(move || engine.run(watch_tx))
            .expect("spawn git state engine");
        StateHandle { tx }
    }
}

/// One live log subscription (`GIT_LOG_WATCH`).
struct LogSub {
    flags: u8,
    limit: u16,
    spec: String,
    /// Last resolved endpoints; a page is re-sent only when these move.
    endpoints: Option<(Vec<gix::ObjectId>, Vec<gix::ObjectId>)>,
    /// A ref moved (or first registration): re-resolve on the next tick.
    dirty: bool,
    /// The one in-flight page id, if any (coalescing pacing).
    unacked: Option<u32>,
    next_update_id: u32,
}

struct Engine {
    repo: RepoHandle,
    repo_id: u16,
    opts: StateOptions,
    rx: Receiver<StateMsg>,
    outbox: Outbox,
    next_state_id: u32,
    /// The one in-flight `GIT_STATE` snapshot id, if any.
    unacked: Option<u32>,
    /// `GIT_STATE` needs a (re-)emit once the ack window frees.
    state_dirty: bool,
    /// Earliest settle deadline for a pending ref/HEAD/op/stash change.
    refs_due: Option<Instant>,
    /// Earliest settle deadline for a pending worktree-status change. Kept
    /// separate so a slow status window never delays a ref/HEAD update —
    /// the snapshot fires at whichever deadline comes first.
    status_due: Option<Instant>,
    /// Live log subscriptions, keyed by client-assigned `log_id`.
    log_subs: std::collections::HashMap<u16, LogSub>,
    _watch: Option<notify::RecommendedWatcher>,
}

impl Engine {
    /// The earliest pending deadline, if any side is dirty.
    fn next_due(&self) -> Option<Instant> {
        [self.refs_due, self.status_due].into_iter().flatten().min()
    }

    fn run(mut self, watch_tx: Sender<StateMsg>) {
        match self.arm_watch(watch_tx) {
            Ok(watcher) => self._watch = watcher,
            Err(reason) => {
                let _ = (self.outbox)(msg_git_closed(self.repo_id, reason));
                return;
            }
        }
        loop {
            let now = Instant::now();
            // Fire elapsed settle timers into dirty flags. A ref change
            // dirties both the state snapshot and every log subscription
            // (its endpoints may have moved); a status change dirties only
            // state.
            if self.refs_due.is_some_and(|d| now >= d) {
                self.refs_due = None;
                self.state_dirty = true;
                for sub in self.log_subs.values_mut() {
                    sub.dirty = true;
                }
            }
            if self.status_due.is_some_and(|d| now >= d) {
                self.status_due = None;
                self.state_dirty = true;
            }

            // Emit GIT_STATE when wanted, dirty, and the ack window is free.
            if self.opts.wants_state && self.state_dirty && self.unacked.is_none() {
                self.state_dirty = false;
                let state_id = self.next_state_id;
                self.next_state_id = self.next_state_id.wrapping_add(1);
                let (flags, records) = self.snapshot();
                if !(self.outbox)(msg_git_state(self.repo_id, state_id, flags, &records)) {
                    return;
                }
                self.unacked = Some(state_id);
            }

            // Emit log pages for subscriptions whose endpoints moved.
            if self.service_log_subs() {
                return; // client gone
            }

            let timeout = match self.next_due() {
                Some(due) => due.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            match self.rx.recv_timeout(timeout) {
                Ok(StateMsg::Ack(id)) => {
                    if self.unacked == Some(id) {
                        self.unacked = None;
                    }
                }
                Ok(StateMsg::Dirty { status_side }) => {
                    // Arm the matching side's window; same-side events
                    // debounce (extend), but a ref event never inherits the
                    // coarser status window and vice versa.
                    let slot = if status_side {
                        &mut self.status_due
                    } else {
                        &mut self.refs_due
                    };
                    let latency = if status_side {
                        self.opts.status_latency
                    } else {
                        self.opts.refs_latency
                    };
                    let due = Instant::now() + latency;
                    match *slot {
                        Some(existing) if existing >= due => {}
                        _ => *slot = Some(due),
                    }
                }
                Ok(StateMsg::WatchLog {
                    log_id,
                    flags,
                    limit,
                    spec,
                }) => {
                    // Re-watching an existing id replaces it; a new id past the
                    // cap is refused with a BUDGET page so the client unblocks
                    // rather than waiting forever for a subscription that never
                    // registered.
                    let max_log_subs = self.repo.budgets.max_log_subs;
                    if !self.log_subs.contains_key(&log_id) && self.log_subs.len() >= max_log_subs {
                        let msg = blit_remote::git::msg_git_log_page(
                            log_id,
                            1,
                            blit_remote::git::GIT_STATUS_BUDGET,
                            0,
                            &[],
                            &[],
                        );
                        if !(self.outbox)(msg) {
                            return;
                        }
                        continue;
                    }
                    self.log_subs.insert(
                        log_id,
                        LogSub {
                            flags,
                            limit,
                            spec,
                            endpoints: None,
                            dirty: true,
                            unacked: None,
                            next_update_id: 1,
                        },
                    );
                }
                Ok(StateMsg::UnwatchLog { log_id }) => {
                    self.log_subs.remove(&log_id);
                }
                Ok(StateMsg::LogAck { log_id, update_id }) => {
                    if let Some(sub) = self.log_subs.get_mut(&log_id)
                        && sub.unacked == Some(update_id)
                    {
                        sub.unacked = None;
                    }
                }
                Ok(StateMsg::Stop) => return,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Re-resolve and re-send any dirty log subscription whose ack window is
    /// free and whose endpoints actually moved. Returns true if the client
    /// outbox is gone.
    fn service_log_subs(&mut self) -> bool {
        if self.log_subs.is_empty() {
            return false;
        }
        let repo = self.repo.local();
        let budgets = self.repo.budgets.clone();
        let ids: Vec<u16> = self
            .log_subs
            .keys()
            .copied()
            .filter(|id| {
                let sub = &self.log_subs[id];
                sub.dirty && sub.unacked.is_none()
            })
            .collect();
        for log_id in ids {
            let cancel = crate::Cancel::default();
            let sub = self.log_subs.get_mut(&log_id).expect("present");
            sub.dirty = false;
            // Resolve the spec to endpoints.
            let resolved =
                crate::requests::resolve_spec(&repo, &sub.spec, budgets.walk_max, &cancel);
            let (page, status, tips, hides) = match resolved {
                Ok((tips, hides)) => {
                    // Skip when endpoints are unchanged and a page was sent.
                    if sub.endpoints.as_ref() == Some(&(tips.clone(), hides.clone())) {
                        continue;
                    }
                    let limit = if sub.limit == 0 {
                        budgets.log_default
                    } else {
                        (sub.limit as usize).min(budgets.log_max)
                    };
                    match crate::requests::walk_log(
                        &repo,
                        tips.clone(),
                        hides.clone(),
                        sub.flags,
                        limit,
                        None,
                        &budgets,
                        &cancel,
                    ) {
                        Ok((records, frontier, more)) => {
                            let flags = if more {
                                blit_remote::git::GIT_COMMITS_MORE
                            } else {
                                0
                            };
                            let update_id = sub.next_update_id;
                            sub.next_update_id = sub.next_update_id.wrapping_add(1);
                            let msg = blit_remote::git::msg_git_log_page(
                                log_id,
                                update_id,
                                GIT_STATUS_OK,
                                flags,
                                &frontier,
                                &records,
                            );
                            sub.unacked = Some(update_id);
                            (msg, GIT_STATUS_OK, Some(tips), Some(hides))
                        }
                        Err(status) => {
                            let update_id = sub.next_update_id;
                            sub.next_update_id = sub.next_update_id.wrapping_add(1);
                            let msg = blit_remote::git::msg_git_log_page(
                                log_id,
                                update_id,
                                status,
                                0,
                                &[],
                                &[],
                            );
                            sub.unacked = Some(update_id);
                            (msg, status, None, None)
                        }
                    }
                }
                Err(status) => {
                    // Unresolvable (e.g. a ref that does not exist yet):
                    // report the status, keep the sub alive so it recovers
                    // when the ref appears.
                    let update_id = sub.next_update_id;
                    sub.next_update_id = sub.next_update_id.wrapping_add(1);
                    let msg =
                        blit_remote::git::msg_git_log_page(log_id, update_id, status, 0, &[], &[]);
                    sub.unacked = Some(update_id);
                    (msg, status, None, None)
                }
            };
            let _ = status;
            if let (Some(t), Some(h)) = (tips, hides) {
                sub.endpoints = Some((t, h));
            } else {
                sub.endpoints = None;
            }
            if !(self.outbox)(page) {
                return true;
            }
        }
        false
    }

    /// Watch the gitdir's state files (and the worktree for status).
    /// `Err(reason)` on a failure that would leave state stale — the caller
    /// closes the repo rather than silently never updating.
    fn arm_watch(&self, tx: Sender<StateMsg>) -> Result<Option<notify::RecommendedWatcher>, u8> {
        use notify::Watcher;
        let repo = self.repo.local();
        let gitdir = repo.git_dir().to_path_buf();
        let common = repo.common_dir().to_path_buf();
        let workdir = repo.workdir().map(|p| p.to_path_buf());
        let with_status = self.opts.status && workdir.is_some();
        let workdir_probe = workdir.clone();
        // The dominant gitdir churn (fetch/gc/commit/hash-object) writes
        // under objects/; those events carry no HEAD/ref/status meaning, so
        // drop them to avoid recomputing status on every loose object.
        let objects = [gitdir.join("objects"), common.join("objects")];
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = &res else {
                return;
            };
            if !event.paths.is_empty()
                && event
                    .paths
                    .iter()
                    .all(|p| objects.iter().any(|o| p.starts_with(o)))
            {
                return;
            }
            let status_side = event.paths.iter().any(|p| {
                workdir_probe
                    .as_deref()
                    .is_some_and(|w| p.starts_with(w) && !p.starts_with(&gitdir))
            });
            let _ = tx.send(StateMsg::Dirty { status_side });
        })
        .map_err(|e| watch_close_reason(&e))?;
        // Non-recursive on the gitdir roots (HEAD, index, MERGE_HEAD…),
        // recursive on refs/ and the sequencer dirs.
        let dirs: [PathBuf; 2] = [gitdir_of(&repo), common.clone()];
        for dir in dirs.iter().collect::<std::collections::HashSet<_>>() {
            let _ = watcher.watch(dir, notify::RecursiveMode::NonRecursive);
            for sub in [
                "refs",
                "rebase-merge",
                "rebase-apply",
                "sequencer",
                "logs/refs",
            ] {
                let path = dir.join(sub);
                if path.exists() {
                    let _ = watcher.watch(&path, notify::RecursiveMode::Recursive);
                }
            }
        }
        // The worktree watch is load-bearing for status: a failure here
        // (descriptor/watch exhaustion) means status silently never
        // updates, so surface it instead of swallowing it.
        if with_status
            && let Some(workdir) = &workdir
            && let Err(e) = watcher.watch(workdir, notify::RecursiveMode::Recursive)
        {
            return Err(watch_close_reason(&e));
        }
        Ok(Some(watcher))
    }

    /// Cut one complete snapshot of the mutable state.
    fn snapshot(&mut self) -> (u8, Vec<u8>) {
        let repo = self.repo.local();
        let mut records = Vec::new();
        let mut flags = 0u8;
        self.head_record(&repo, &mut records);
        let mut branches: Vec<String> = Vec::new();
        if !self.refs_records(&repo, &mut records, &mut branches) {
            flags |= GIT_STATE_REFS_TRUNCATED;
        }
        self.op_record(&repo, &mut records);
        if self.opts.tracking {
            self.upstream_records(&repo, &branches, &mut records);
        }
        self.stash_records(&repo, &mut records);
        if self.opts.status {
            self.status_records(&repo, &mut records, &mut flags);
        }
        (flags, records)
    }

    fn head_record(&self, repo: &gix::Repository, records: &mut Vec<u8>) {
        let Ok(head) = repo.head() else {
            return;
        };
        let (head_flags, oid, name) = match head.kind {
            gix::head::Kind::Symbolic(reference) => {
                let name = crate::escape_bstr(reference.name.as_bstr());
                let oid = repo
                    .head_id()
                    .map(|id| oid_bytes(id.as_ref()))
                    .unwrap_or(GIT_OID_NONE);
                (0, oid, name)
            }
            gix::head::Kind::Detached { target, .. } => {
                (GIT_HEAD_DETACHED, oid_bytes(target.as_ref()), String::new())
            }
            gix::head::Kind::Unborn(name) => (
                GIT_HEAD_UNBORN,
                GIT_OID_NONE,
                crate::escape_bstr(name.as_bstr()),
            ),
        };
        append_git_state_record(
            records,
            &GitStateRecord::Head {
                flags: head_flags,
                oid,
                name: &name,
            },
        );
    }

    /// All refs; returns false when the entry budget truncated the set.
    fn refs_records(
        &self,
        repo: &gix::Repository,
        records: &mut Vec<u8>,
        branches: &mut Vec<String>,
    ) -> bool {
        let Ok(platform) = repo.references() else {
            return true;
        };
        let Ok(iter) = platform.all() else {
            return true;
        };
        for (count, reference) in iter.flatten().enumerate() {
            if count >= self.repo.budgets.entries_max {
                return false;
            }
            let name = crate::escape_bstr(reference.name().as_bstr());
            let mut ref_flags = 0u8;
            let mut reference = reference;
            let oid = match reference.target() {
                gix::refs::TargetRef::Object(id) => oid_bytes(id),
                gix::refs::TargetRef::Symbolic(_) => {
                    ref_flags |= GIT_REF_SYMBOLIC;
                    reference
                        .peel_to_id_in_place()
                        .map(|id| oid_bytes(id.as_ref()))
                        .unwrap_or(GIT_OID_NONE)
                }
            };
            // Annotated tags peel to their target commit.
            let mut peeled = GIT_OID_NONE;
            if name.starts_with("refs/tags/")
                && let Ok(id) = reference.peel_to_id_in_place()
            {
                let peeled_bytes = oid_bytes(id.as_ref());
                if peeled_bytes != oid {
                    peeled = peeled_bytes;
                    ref_flags |= GIT_REF_PEELED_VALID;
                }
            }
            if let Some(branch) = name.strip_prefix("refs/heads/") {
                branches.push(branch.to_string());
            }
            append_git_state_record(
                records,
                &GitStateRecord::Ref {
                    flags: ref_flags,
                    oid,
                    peeled,
                    name: &name,
                },
            );
        }
        true
    }

    fn op_record(&self, repo: &gix::Repository, records: &mut Vec<u8>) {
        use gix::state::InProgress;
        let Some(state) = repo.state() else {
            return;
        };
        let (op, head_file) = match state {
            InProgress::Merge => (GIT_OP_MERGE, Some("MERGE_HEAD")),
            InProgress::Rebase | InProgress::RebaseInteractive => (GIT_OP_REBASE, None),
            InProgress::CherryPick | InProgress::CherryPickSequence => {
                (GIT_OP_CHERRY_PICK, Some("CHERRY_PICK_HEAD"))
            }
            InProgress::Revert | InProgress::RevertSequence => (GIT_OP_REVERT, Some("REVERT_HEAD")),
            InProgress::Bisect => (GIT_OP_BISECT, Some("BISECT_EXPECTED_REV")),
            _ => return,
        };
        let read_oid = |name: &str| -> Option<gix::ObjectId> {
            let text = std::fs::read_to_string(repo.git_dir().join(name)).ok()?;
            text.trim().parse().ok()
        };
        let oid = match (head_file, op) {
            (Some(file), _) => read_oid(file),
            // Rebase keeps its head under the rebase directory.
            (None, _) => ["rebase-merge/orig-head", "rebase-apply/orig-head"]
                .iter()
                .find_map(|f| read_oid(f)),
        };
        append_git_state_record(
            records,
            &GitStateRecord::Op {
                op,
                oid: oid.map(|id| oid_bytes(id.as_ref())).unwrap_or(GIT_OID_NONE),
                detail: "",
            },
        );
    }

    fn upstream_records(
        &mut self,
        repo: &gix::Repository,
        branches: &[String],
        records: &mut Vec<u8>,
    ) {
        for branch in branches {
            let Some((upstream_name, upstream_id)) = upstream_of(repo, branch) else {
                continue;
            };
            let name = format!("refs/heads/{branch}");
            let Some(upstream_id) = upstream_id else {
                append_git_state_record(
                    records,
                    &GitStateRecord::Upstream {
                        flags: GIT_UPSTREAM_GONE,
                        ahead: 0,
                        behind: 0,
                        name: &name,
                        upstream: &upstream_name,
                    },
                );
                continue;
            };
            let tip = repo
                .find_reference(&name)
                .ok()
                .and_then(|mut r| r.peel_to_id_in_place().ok().map(|id| id.detach()));
            let Some(tip) = tip else {
                continue;
            };
            let (flags, ahead, behind) = self.ahead_behind(repo, tip, upstream_id);
            append_git_state_record(
                records,
                &GitStateRecord::Upstream {
                    flags,
                    ahead,
                    behind,
                    name: &name,
                    upstream: &upstream_name,
                },
            );
        }
    }

    /// Count `upstream..tip` and `tip..upstream`, memoized by the immutable
    /// oid pair; `COUNTS_VALID` is withheld past the walk budget.
    fn ahead_behind(
        &mut self,
        repo: &gix::Repository,
        tip: gix::ObjectId,
        upstream: gix::ObjectId,
    ) -> (u8, u32, u32) {
        let count = |from: gix::ObjectId, hide: gix::ObjectId| -> Option<u32> {
            let walk = repo.rev_walk([from]).with_hidden([hide]);
            let iter = walk.all().ok()?;
            let mut n = 0u32;
            for item in iter {
                item.ok()?;
                n += 1;
                if n as usize > self.repo.budgets.walk_max {
                    return None;
                }
            }
            Some(n)
        };
        match (count(tip, upstream), count(upstream, tip)) {
            (Some(ahead), Some(behind)) => (GIT_UPSTREAM_COUNTS_VALID, ahead, behind),
            _ => (0, 0, 0),
        }
    }

    fn stash_records(&self, repo: &gix::Repository, records: &mut Vec<u8>) {
        let name: &gix::refs::FullNameRef = "refs/stash".try_into().expect("valid ref name");
        // The reverse reflog reader works through this window.
        let mut buf = vec![0u8; 64 * 1024];
        let Ok(Some(iter)) = repo.refs.reflog_iter_rev(name, &mut buf) else {
            return;
        };
        for (index, entry) in iter.flatten().enumerate() {
            if index >= self.repo.budgets.entries_max {
                break;
            }
            let (msg, _) = crate::utf8_lossy_flag(entry.message.as_ref());
            let time = entry.signature.time;
            append_git_state_record(
                records,
                &GitStateRecord::Stash {
                    index: index as u16,
                    oid: oid_bytes(entry.new_oid.as_ref()),
                    time: time.seconds,
                    tz: (time.offset / 60) as i16,
                    msg: &msg,
                },
            );
        }
    }

    fn status_records(&self, repo: &gix::Repository, records: &mut Vec<u8>, flags: &mut u8) {
        crate::diffs::append_status_records(repo, &self.opts, &self.repo.budgets, records, flags);
    }
}

fn gitdir_of(repo: &gix::Repository) -> PathBuf {
    repo.git_dir().to_path_buf()
}

/// The configured upstream of `branch`: `(escaped tracking ref name,
/// Some(tip) | None when the ref is gone)`. None when no upstream at all.
fn upstream_of(repo: &gix::Repository, branch: &str) -> Option<(String, Option<gix::ObjectId>)> {
    let full = format!("refs/heads/{branch}");
    let name: &gix::refs::FullNameRef = full.as_str().try_into().ok()?;
    let tracking = repo
        .branch_remote_tracking_ref_name(name, gix::remote::Direction::Fetch)?
        .ok()?;
    let escaped = crate::escape_bstr(tracking.as_bstr());
    match repo.find_reference(tracking.as_bstr()) {
        Ok(mut reference) => {
            let id = reference.peel_to_id_in_place().ok().map(|id| id.detach());
            Some((escaped, id))
        }
        Err(_) => Some((escaped, None)),
    }
}
