//! Repository-wide reads and the one remote operation: discovery of
//! repositories under a path, reflog traversal, and fetch
//! (docs/design/git.md).

use blit_remote::git::{
    GIT_BLAME_FOLLOW_COPIES, GIT_BLAME_FOLLOW_RENAMES, GIT_BLAME_TRUNCATED, GIT_DISCOVER_BARE,
    GIT_DISCOVER_NESTED, GIT_DISCOVER_TRUNCATED, GIT_FETCH_ANCHOR, GIT_FETCH_NO_TAGS,
    GIT_FETCH_PRUNE, GIT_FETCH_REF_FORCED, GIT_FETCH_REF_NEW, GIT_FETCH_REF_PRUNED,
    GIT_FETCH_REF_TAG_UPDATE, GIT_FOUND_BARE, GIT_FOUND_LINKED, GIT_OID_NONE,
    GIT_REFLOG_OLDEST_FIRST, GIT_REFLOG_TRUNCATED, GIT_STATUS_CANCELLED, GIT_STATUS_INVALID,
    GIT_STATUS_NOT_FOUND, GIT_STATUS_OK, GIT_STATUS_OTHER, GIT_STATUS_PERMISSION,
    GIT_STATUS_WRONG_TYPE, GitBlameRecord, GitBlameRequest, GitDiscoverRecord, GitDiscoverRequest,
    GitFetchRecord, GitFetchRequest, GitReflogRecord, GitReflogRequest, append_git_blame_record,
    append_git_discover_record, append_git_fetch_record, append_git_reflog_record,
    msg_git_blame_resp, msg_git_discover_resp, msg_git_fetch_resp, msg_git_reflog_resp,
};

use crate::{Cancel, RepoHandle, oid_bytes};

/// Bounds on a discovery walk, all operator-tunable.
pub struct DiscoverLimits {
    pub depth_max: usize,
    pub results_max: usize,
    pub scan_max: usize,
}

impl Default for DiscoverLimits {
    fn default() -> Self {
        DiscoverLimits {
            depth_max: crate::env_usize("BLIT_GIT_DISCOVER_DEPTH_MAX", 16),
            results_max: crate::env_usize("BLIT_GIT_DISCOVER_MAX", 256),
            scan_max: crate::env_usize("BLIT_GIT_DISCOVER_SCAN_MAX", 100_000),
        }
    }
}

/// `GIT_DISCOVER`: repositories under `path`, breadth-first to `depth`.
///
/// Allocates no repo ids — an enumeration, not an open — so it cannot
/// exhaust the per-connection repo budget, and a client stops probing a
/// ladder of candidate paths with an `FS_SYNC` per directory level just to
/// learn names.
pub fn discover(req: &GitDiscoverRequest<'_>, cancel: &Cancel) -> Vec<u8> {
    discover_within(req, cancel, DiscoverLimits::default())
}

/// `discover` with the budgets given rather than read from the environment,
/// so paging across a cap is reachable from a test without a process-wide
/// env var.
pub fn discover_within(
    req: &GitDiscoverRequest<'_>,
    cancel: &Cancel,
    limits: DiscoverLimits,
) -> Vec<u8> {
    let fail = |status: u8| msg_git_discover_resp(req.nonce, status, 0, &[]);
    const KNOWN: u8 = GIT_DISCOVER_NESTED | GIT_DISCOVER_BARE;
    if req.flags & !KNOWN != 0 {
        return fail(GIT_STATUS_INVALID);
    }
    let root = std::path::Path::new(req.path);
    let Ok(root) = root.canonicalize() else {
        return fail(GIT_STATUS_NOT_FOUND);
    };
    if !root.is_dir() {
        return fail(GIT_STATUS_NOT_FOUND);
    }
    let depth = if req.depth == 0 {
        4
    } else {
        (req.depth as usize).min(limits.depth_max)
    };
    let nested = req.flags & GIT_DISCOVER_NESTED != 0;
    let want_bare = req.flags & GIT_DISCOVER_BARE != 0;

    let mut records = Vec::new();
    let mut flags = 0u8;
    let mut scanned = 0usize;
    let mut emitted = 0usize;
    // Deduped by canonical gitdir: several paths routinely resolve to one
    // repository, and the gitdir is the identity GIT_REPO reports.
    let mut seen: std::collections::HashSet<std::path::PathBuf> = Default::default();
    let mut last = String::new();
    // A resume replays the walk to reach where the last page stopped. Until
    // it gets there nothing is emitted and nothing is charged: both budgets
    // bound the *new* work a call does, so a page always makes progress
    // where a budget counted from the start of the walk would stop at the
    // same repository forever.
    let mut skipping = !req.after.is_empty();
    // Breadth-first, so a shallow repository is never missed because a
    // deep subtree exhausted the scan budget first.
    let mut queue: std::collections::VecDeque<(std::path::PathBuf, usize)> =
        [(root.clone(), 0usize)].into_iter().collect();
    while let Some((dir, level)) = queue.pop_front() {
        if cancel.is_cancelled() {
            return fail(GIT_STATUS_CANCELLED);
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut is_repo = false;
        let mut children: Vec<std::path::PathBuf> = Vec::new();
        for entry in entries.flatten() {
            if !skipping {
                scanned += 1;
                if scanned > limits.scan_max {
                    flags |= GIT_DISCOVER_TRUNCATED;
                    break;
                }
            }
            let name = entry.file_name();
            // Never follow a symlink out of the tree (the discipline the
            // recursive watches use).
            let Ok(meta) = entry.metadata() else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if name == ".git" {
                is_repo = true;
                continue;
            }
            if meta.is_dir() {
                children.push(entry.path());
            }
        }
        // A bare repository is a directory that IS the gitdir.
        let bare =
            !is_repo && want_bare && dir.join("HEAD").is_file() && dir.join("objects").is_dir();
        if is_repo || bare {
            if let Some(record) = repo_record(&dir, &mut seen) {
                // `after` is a skip *during* the walk, not a filter over
                // the result: the cap has to measure newly-returned repos,
                // or a resume re-walks the same first page, stops at the
                // same repo, and hands back a cursor that has not moved.
                if skipping {
                    // The cursor names the last repository a page
                    // delivered, so seeing it means the next one is new.
                    if record.0 == req.after {
                        skipping = false;
                    }
                } else if emitted >= limits.results_max {
                    flags |= GIT_DISCOVER_TRUNCATED;
                    break;
                } else {
                    last = record.0.clone();
                    emitted += 1;
                    append_git_discover_record(
                        &mut records,
                        &GitDiscoverRecord::Repo {
                            flags: record.2,
                            workdir: &record.0,
                            gitdir: &record.1,
                        },
                    );
                }
            }
            if !nested {
                continue;
            }
        }
        if flags & GIT_DISCOVER_TRUNCATED != 0 {
            break;
        }
        if level < depth {
            // Sorted, because the resume above replays the walk and matches
            // the cursor by path: `read_dir` promises no order at all, so an
            // unsorted queue can reach the cursor's repository at a different
            // point on the next call and skip or repeat its neighbours. The
            // family's rule for a stateless continuation is a deterministic
            // total order — the same reason a tree listing pages on git's own
            // ordering — and for a filesystem walk that order is the path.
            children.sort();
            for child in children {
                queue.push_back((child, level + 1));
            }
        }
    }
    // Every truncation says where it stopped, whichever budget ran out —
    // except a walk that scanned itself out before finding anything, which
    // has no stopping point to name and is honestly unresumable.
    if flags & GIT_DISCOVER_TRUNCATED != 0 && !last.is_empty() {
        append_git_discover_record(
            &mut records,
            &GitDiscoverRecord::Cursor {
                after: &last,
                pos: 0,
            },
        );
    }
    msg_git_discover_resp(req.nonce, GIT_STATUS_OK, flags, &records)
}

/// `(workdir, gitdir, flags)` for a discovered directory, or None when it
/// does not open or is a duplicate of one already reported.
fn repo_record(
    dir: &std::path::Path,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
) -> Option<(String, String, u8)> {
    let (_handle, info) = crate::open(dir.to_str()?).ok()?;
    let gitdir = std::path::PathBuf::from(&info.gitdir);
    if !seen.insert(gitdir) {
        return None;
    }
    let mut flags = 0u8;
    if info.flags & blit_remote::git::GIT_REPO_BARE != 0 {
        flags |= GIT_FOUND_BARE;
    }
    if info.flags & blit_remote::git::GIT_REPO_LINKED != 0 {
        flags |= GIT_FOUND_LINKED;
    }
    Some((info.workdir, info.gitdir, flags))
}

impl RepoHandle {
    /// `GIT_REFLOG`: entries for any ref, generalizing the reader that
    /// already served the stash.
    ///
    /// This is the only way to name an oid no longer reachable from any
    /// ref — an amended-away commit `resolve` cannot see and `log` cannot
    /// reach — and the only local answer to "what did this session do to
    /// the repository".
    pub fn reflog(&self, req: &GitReflogRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let nonce = req.nonce;
        let fail = |status: u8| msg_git_reflog_resp(nonce, status, 0, &[]);
        if req.flags & !GIT_REFLOG_OLDEST_FIRST != 0 {
            return fail(GIT_STATUS_INVALID);
        }
        let repo = self.local();
        let name = if req.ref_name.is_empty() {
            "HEAD".to_string()
        } else {
            match crate::unescape_wire(req.ref_name) {
                Some(bytes) => match String::from_utf8(bytes) {
                    Ok(name) => name,
                    Err(_) => return fail(GIT_STATUS_INVALID),
                },
                None => return fail(GIT_STATUS_INVALID),
            }
        };
        let Ok(full): Result<&gix::refs::FullNameRef, _> = name.as_str().try_into() else {
            return fail(GIT_STATUS_INVALID);
        };
        let limit = if req.limit == 0 {
            self.budgets.entries_max
        } else {
            (req.limit as usize).min(self.budgets.entries_max)
        };
        let oldest_first = req.flags & GIT_REFLOG_OLDEST_FIRST != 0;
        // A missing ref and a ref with no reflog both make the reader
        // return `None`, and the doc promises they are told apart. Only the
        // ref lookup can distinguish them, so it happens first — a
        // reflog-less ref then answers OK with no entries.
        let ref_exists = match repo.refs.try_find(full) {
            Ok(found) => found.is_some(),
            Err(_) => return fail(GIT_STATUS_OTHER),
        };
        // HEAD in a repository with no commits yet resolves to nothing, but
        // it does exist and can have a reflog.
        let ref_exists = ref_exists || full.as_bstr() == "HEAD";

        // `after_pos` entries have already been delivered from whichever
        // end the flags select; skipping them is what makes a reflog longer
        // than one page reachable at all.
        let skip = usize::try_from(req.after_pos).unwrap_or(usize::MAX);
        let mut buf = vec![0u8; 64 * 1024];
        let mut lines: Vec<gix::refs::log::Line> = Vec::new();
        let mut seen = 0usize;
        let mut truncated = false;
        // The reverse reader is the one the stash already used; the forward
        // reader yields borrowed lines, so own them before the buffer is
        // reused.
        if oldest_first {
            let Ok(Some(iter)) = repo.refs.reflog_iter(full, &mut buf) else {
                return if ref_exists {
                    msg_git_reflog_resp(nonce, GIT_STATUS_OK, 0, &[])
                } else {
                    fail(GIT_STATUS_NOT_FOUND)
                };
            };
            for entry in iter.flatten() {
                if cancel.is_cancelled() {
                    return fail(GIT_STATUS_CANCELLED);
                }
                seen += 1;
                if seen <= skip {
                    continue;
                }
                if lines.len() >= limit {
                    truncated = true;
                    break;
                }
                lines.push(entry.into());
            }
        } else {
            let Ok(Some(iter)) = repo.refs.reflog_iter_rev(full, &mut buf) else {
                return if ref_exists {
                    msg_git_reflog_resp(nonce, GIT_STATUS_OK, 0, &[])
                } else {
                    fail(GIT_STATUS_NOT_FOUND)
                };
            };
            for entry in iter.flatten() {
                if cancel.is_cancelled() {
                    return fail(GIT_STATUS_CANCELLED);
                }
                seen += 1;
                if seen <= skip {
                    continue;
                }
                if lines.len() >= limit {
                    truncated = true;
                    break;
                }
                lines.push(entry);
            }
        }

        let mut records = Vec::new();
        for line in &lines {
            let (msg, _) = crate::utf8_lossy_flag(line.message.as_ref());
            let time = line.signature.time;
            append_git_reflog_record(
                &mut records,
                &GitReflogRecord::Entry {
                    flags: 0,
                    old: oid_bytes(line.previous_oid.as_ref()),
                    new: oid_bytes(line.new_oid.as_ref()),
                    time: time.seconds,
                    tz: (time.offset / 60) as i16,
                    msg: &msg,
                },
            );
        }
        let mut flags = 0u8;
        if truncated {
            flags |= GIT_REFLOG_TRUNCATED;
            // Where it stopped, counted from the same end the flags chose,
            // so the next page is `after_pos` = this.
            append_git_reflog_record(
                &mut records,
                &GitReflogRecord::Cursor {
                    after: "",
                    pos: (skip + lines.len()) as u64,
                },
            );
        }
        msg_git_reflog_resp(nonce, GIT_STATUS_OK, flags, &records)
    }
}

/// Whether a fetch can be attempted at all: the operator switch, and a
/// `git` binary to run. Answered per `GIT_REPO` as `FETCHABLE`, so a
/// client learns it at open rather than by trying.
pub fn fetch_available() -> bool {
    if std::env::var("BLIT_GIT_FETCH").is_ok_and(|v| v == "0") {
        return false;
    }
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        std::process::Command::new("git")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

impl RepoHandle {
    /// `GIT_FETCH`: bring objects in, and report per-ref what happened.
    ///
    /// Runs the box's own `git` as a plain subprocess — no PTY, no shell,
    /// argv only. The family's "never shell out" stance is about reads,
    /// where a spawn is real overhead against a 2 ms tree listing and
    /// porcelain is a format meant for humans. Neither transfers: a fetch
    /// takes seconds, and `--porcelain` is a machine format. Against that,
    /// linking a TLS stack into every binary would also mean
    /// reimplementing `insteadOf`, proxies, credential helpers and SSH
    /// config — divergence there is invisible until a user's fetch fails
    /// in a way their own `git` does not.
    pub fn fetch(&self, req: &GitFetchRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let nonce = req.nonce;
        let fail = |status: u8| msg_git_fetch_resp(nonce, status, 0, &[]);
        const KNOWN: u8 = GIT_FETCH_PRUNE | GIT_FETCH_NO_TAGS | GIT_FETCH_ANCHOR;
        if req.flags & !KNOWN != 0 {
            return fail(GIT_STATUS_INVALID);
        }
        if !fetch_available() {
            return fail(GIT_STATUS_PERMISSION);
        }
        // A remote or refspec that could be read as an option would let a
        // caller reach the rest of git's command line.
        if !fetch_fields_valid(req.remote, &req.refspecs) {
            return fail(GIT_STATUS_INVALID);
        }
        let repo = self.local();
        let Some(dir) = repo.workdir().or_else(|| Some(repo.git_dir())) else {
            return fail(GIT_STATUS_INVALID);
        };

        let mut args: Vec<String> = vec![
            "fetch".into(),
            "--porcelain".into(),
            "--atomic".into(),
            "--no-write-fetch-head".into(),
        ];
        if req.flags & GIT_FETCH_PRUNE != 0 {
            args.push("--prune".into());
        }
        if req.flags & GIT_FETCH_NO_TAGS != 0 {
            args.push("--no-tags".into());
        }
        let remote = if req.remote.is_empty() {
            "origin"
        } else {
            req.remote
        };
        args.push(remote.to_string());
        // ANCHOR writes each wanted tip under a namespace no other tool
        // uses, so a concurrent gc cannot prune it before the client
        // diffs it — the anchoring a consumer otherwise does by hand.
        for (n, spec) in req.refspecs.iter().enumerate() {
            if req.flags & GIT_FETCH_ANCHOR != 0 && !spec.contains(':') {
                args.push(format!("+{spec}:refs/blit/fetch/{remote}/{n}"));
            } else {
                args.push((*spec).to_string());
            }
        }

        let timeout = std::time::Duration::from_millis(if req.timeout_ms == 0 {
            crate::env_u64_pub("BLIT_GIT_FETCH_TIMEOUT_MS", 120_000)
        } else {
            u64::from(req.timeout_ms).min(600_000)
        });
        let output = match run_git(dir, &args, timeout, cancel) {
            Ok(output) => output,
            Err(status) => return fail(status),
        };

        let mut records = Vec::new();
        for line in output.stdout.lines() {
            if let Some(record) = parse_porcelain_line(line) {
                append_git_fetch_record(&mut records, &record);
            }
        }
        if !output.ok {
            // A remote can refuse one refspec of several and still exit
            // zero, so success is never inferred from the code alone —
            // but a non-zero code with nothing parsed still has to say
            // something, and git's last stderr line is what it has.
            let detail = output
                .stderr
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("fetch failed");
            append_git_fetch_record(
                &mut records,
                &GitFetchRecord::Ref {
                    flags: 0,
                    status: GIT_STATUS_OTHER,
                    old: GIT_OID_NONE,
                    new: GIT_OID_NONE,
                    name: "",
                    detail,
                },
            );
        }
        msg_git_fetch_resp(nonce, GIT_STATUS_OK, 0, &records)
    }
}

fn fetch_fields_valid(remote: &str, refspecs: &[&str]) -> bool {
    !remote.starts_with('-')
        && !remote.contains('\0')
        && refspecs
            .iter()
            .all(|s| !s.starts_with('-') && !s.contains('\0'))
}

struct GitOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run `git` with the environment pinned: no prompting, no askpass, no
/// inherited `-c` overrides, stdin closed. A missing credential then fails
/// reportably instead of hanging until the timeout.
///
/// Both pipes are drained on their own threads. Polling for exit while
/// leaving them unread deadlocks the moment git writes more than a pipe
/// buffer — which a fetch of any size does.
fn run_git(
    dir: &std::path::Path,
    args: &[String],
    timeout: std::time::Duration,
    cancel: &Cancel,
) -> Result<GitOutput, u8> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GIT_CONFIG_PARAMETERS", "")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| GIT_STATUS_OTHER)?;

    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out_thread = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );
    let err_thread = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    );

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => return Err(GIT_STATUS_OTHER),
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GIT_STATUS_CANCELLED);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(GIT_STATUS_OTHER);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    Ok(GitOutput {
        ok: status.success(),
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

/// One `git fetch --porcelain` line: `<flag> <old> <new> <ref>`, where
/// flag is ` ` fast-forward, `+` forced, `-` pruned, `*` new, `!` rejected,
/// `=` up to date.
fn parse_porcelain_line(line: &str) -> Option<GitFetchRecord<'_>> {
    // The flag is one character wide and column-aligned, and for the most
    // common outcome — an ordinary fast-forward — it is a space. Splitting
    // the whole line on ' ' therefore yields an empty first field and
    // drops exactly the records a client most wants; take the flag as the
    // first byte and split what follows.
    let (flag, rest) = line.split_at_checked(1)?;
    let flag = flag.as_bytes()[0];
    let mut parts = rest.strip_prefix(' ')?.splitn(3, ' ');
    let old = parts.next()?;
    let new = parts.next()?;
    let name = parts.next()?;
    let mut flags = 0u8;
    let mut status = GIT_STATUS_OK;
    // git's whole alphabet, and it has to stay whole: an unhandled flag is a
    // ref the reply does not mention, which is the one thing this response
    // exists to prevent.
    match flag {
        b'+' => flags |= GIT_FETCH_REF_FORCED,
        b'-' => flags |= GIT_FETCH_REF_PRUNED,
        b'*' => flags |= GIT_FETCH_REF_NEW,
        b't' => flags |= GIT_FETCH_REF_TAG_UPDATE,
        b'!' => status = GIT_STATUS_OTHER,
        b' ' | b'=' => {}
        _ => return None,
    }
    Some(GitFetchRecord::Ref {
        flags,
        status,
        old: hex_oid(old),
        new: hex_oid(new),
        name,
        detail: "",
    })
}

/// A hex oid from porcelain output, zero when absent or unparseable.
fn hex_oid(hex: &str) -> blit_remote::git::GitOid {
    let mut oid = GIT_OID_NONE;
    let bytes = hex.as_bytes();
    if bytes.len() < 40 || bytes.len() > 64 || !bytes.len().is_multiple_of(2) {
        return oid;
    }
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let Ok(text) = std::str::from_utf8(pair) else {
            return GIT_OID_NONE;
        };
        let Ok(byte) = u8::from_str_radix(text, 16) else {
            return GIT_OID_NONE;
        };
        oid[i] = byte;
    }
    oid
}

/// Lines in the blob at `path` in `commit`'s tree, counting a final line
/// with no newline — the same count `git blame` attributes over.
fn blob_line_count(repo: &gix::Repository, commit: gix::ObjectId, path: &[u8]) -> Result<u32, u8> {
    let object = repo.find_object(commit).map_err(|_| GIT_STATUS_NOT_FOUND)?;
    let tree = object.peel_to_tree().map_err(|_| GIT_STATUS_WRONG_TYPE)?;
    let entry = tree
        .lookup_entry_by_path(gix::path::from_byte_slice(path))
        .map_err(|_| GIT_STATUS_OTHER)?
        .ok_or(GIT_STATUS_NOT_FOUND)?;
    if !entry.mode().is_blob() {
        return Err(GIT_STATUS_WRONG_TYPE);
    }
    let blob = repo
        .find_object(entry.oid())
        .map_err(|_| GIT_STATUS_NOT_FOUND)?;
    let data: &[u8] = &blob.data;
    if data.is_empty() {
        return Ok(0);
    }
    let newlines = bytecount(data, b'\n');
    let trailing = usize::from(!data.ends_with(b"\n"));
    Ok(u32::try_from(newlines + trailing).unwrap_or(u32::MAX))
}

fn bytecount(data: &[u8], byte: u8) -> usize {
    data.iter().filter(|b| **b == byte).count()
}

impl RepoHandle {
    /// `GIT_BLAME`: line attribution over `gix-blame`.
    ///
    /// Author and message are deliberately absent — the response carries
    /// commit oids and the client resolves the distinct set with one
    /// `GIT_LOG`, or finds them already in its oid-keyed cache. That keeps
    /// a viewport blame to a few hundred bytes and keeps the family's
    /// "oid-addressed, cache forever" discipline intact.
    pub fn blame(&self, req: &GitBlameRequest<'_>, cancel: &Cancel) -> Vec<u8> {
        let nonce = req.nonce;
        let fail = |status: u8| msg_git_blame_resp(nonce, status, 0, &[]);
        const KNOWN: u8 = GIT_BLAME_FOLLOW_RENAMES | GIT_BLAME_FOLLOW_COPIES;
        if req.flags & !KNOWN != 0 {
            return fail(GIT_STATUS_INVALID);
        }
        let repo = self.local();
        let path = match crate::unescape_wire(req.path) {
            Some(bytes) if !bytes.is_empty() => bytes,
            _ => return fail(GIT_STATUS_INVALID),
        };
        // Zero means HEAD; the worktree is not blameable, so there is no
        // endpoint kind here at all.
        let suspect = if crate::is_zero_oid(&req.oid) {
            match repo.head_id() {
                Ok(id) => id.detach(),
                Err(_) => return fail(GIT_STATUS_NOT_FOUND),
            }
        } else {
            crate::oid_from_wire(&repo, &req.oid)
        };

        let lines_max = self.budgets.blame_lines_max.max(1);
        // gix *rejects* an inclusive range running past the end of the file
        // rather than clamping it, so the file's length is needed before a
        // range can be built at all — otherwise blaming the last page of a
        // file, or "from line N to the end", fails outright. It is also
        // what bounds a whole-file blame, which would otherwise walk
        // unbounded however large the file.
        let total = match blob_line_count(&repo, suspect, &path) {
            Ok(total) => total,
            Err(status) => return fail(status),
        };
        let start = req.start_line.max(1);
        // An empty file, or a viewport that begins past the end: nothing to
        // attribute, and nothing wrong with having asked.
        if total == 0 || start > total {
            return msg_git_blame_resp(nonce, GIT_STATUS_OK, 0, &[]);
        }
        let wanted_end = if req.line_count == 0 {
            total
        } else {
            start.saturating_add(req.line_count - 1).min(total)
        };
        let end = wanted_end.min(start.saturating_add(lines_max - 1));
        // Truncation is a property of the answer, not of the request: the
        // walk stopped short of the lines that were asked for.
        let clamped = end < wanted_end;

        // gix takes a 1-based inclusive range; a viewport blame is the
        // cheap case and the whole point of the field.
        let ranges = gix::blame::BlameRanges::from_range(start..=end);
        let Ok(mut resource_cache) = repo.diff_resource_cache_for_tree_diff() else {
            return fail(GIT_STATUS_OTHER);
        };
        let options = gix::blame::Options {
            diff_algorithm: gix::diff::blob::Algorithm::Histogram,
            range: ranges,
            since: None,
            rewrites: (req.flags & GIT_BLAME_FOLLOW_RENAMES != 0).then(|| {
                let mut rewrites = gix::diff::Rewrites::default();
                rewrites.copies = (req.flags & GIT_BLAME_FOLLOW_COPIES != 0)
                    .then(gix::diff::rewrites::Copies::default);
                rewrites
            }),
            debug_track_path: false,
        };
        if cancel.is_cancelled() {
            return fail(GIT_STATUS_CANCELLED);
        }
        let outcome = gix::blame::file(
            &repo.objects,
            suspect,
            repo.commit_graph_if_enabled().ok().flatten(),
            &mut resource_cache,
            path.as_slice().into(),
            options,
        );
        let Ok(outcome) = outcome else {
            return fail(GIT_STATUS_NOT_FOUND);
        };

        let mut records = Vec::new();
        for entry in outcome.entries {
            if cancel.is_cancelled() {
                return fail(GIT_STATUS_CANCELLED);
            }
            let orig_path = entry
                .source_file_name
                .as_ref()
                .map(|name| crate::escape_bstr(name.as_ref()))
                .unwrap_or_default();
            append_git_blame_record(
                &mut records,
                &GitBlameRecord::Range {
                    flags: 0,
                    // Lines are 1-based on the wire, as everywhere else in
                    // the family; gix counts from zero.
                    commit: oid_bytes(entry.commit_id.as_ref()),
                    start_line: entry.start_in_blamed_file + 1,
                    line_count: entry.len.get(),
                    orig_start: entry.start_in_source_file + 1,
                    orig_path: &orig_path,
                },
            );
        }
        let mut flags = 0u8;
        if clamped {
            flags |= GIT_BLAME_TRUNCATED;
            // The last line attributed. A client blaming a 200 000-line
            // file continues with `start_line` one past it, so the cap
            // bounds one response rather than the answer.
            append_git_blame_record(
                &mut records,
                &GitBlameRecord::Cursor {
                    after: "",
                    pos: u64::from(end),
                },
            );
        }
        msg_git_blame_resp(nonce, GIT_STATUS_OK, flags, &records)
    }
}

/// Open a submodule by `(parent, path)` rather than by a guessed
/// filesystem location.
///
/// `.gitmodules` says where a submodule's worktree lives and blit does not
/// expose config, so a client asked to "open them as separate
/// repositories" had to guess. Discovery from the joined path resolves the
/// `.git` file or `.git/modules/<name>` itself, and comparing gitdirs
/// proves it really was a submodule rather than the parent answering again.
pub fn open_submodule(
    parent: &RepoHandle,
    path: &str,
) -> Result<(RepoHandle, crate::RepoInfo), (u8, String)> {
    let repo = parent.local();
    let Some(workdir) = repo.workdir() else {
        return Err((GIT_STATUS_INVALID, "parent repository is bare".to_string()));
    };
    let rel = std::path::Path::new(path);
    if rel.is_absolute() || rel.components().any(|c| c.as_os_str() == "..") {
        return Err((
            GIT_STATUS_INVALID,
            "submodule path must be relative to the parent worktree".to_string(),
        ));
    }
    let joined = workdir.join(rel);
    if !joined.is_dir() {
        return Err((
            GIT_STATUS_NOT_FOUND,
            "submodule not initialized".to_string(),
        ));
    }
    // Lexical confinement is not confinement: `..` and an absolute path are
    // refused above, but a symlink inside the worktree pointing anywhere on
    // the box is neither. Resolve the path and prove it is still under the
    // parent's own resolved worktree before opening what it names.
    let Ok(joined) = joined.canonicalize() else {
        return Err((
            GIT_STATUS_NOT_FOUND,
            "submodule not initialized".to_string(),
        ));
    };
    let top = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_owned());
    if !joined.starts_with(&top) {
        return Err((
            GIT_STATUS_INVALID,
            "submodule path leaves the parent worktree".to_string(),
        ));
    }
    // A submodule that was initialized but never updated is an empty
    // directory: there is nothing there to open, and discovery from it would
    // walk *up* and find the parent, which reads as "not a submodule" when
    // the honest answer is that the submodule has no checkout yet.
    if !joined.join(GIT_DIR_NAME).exists() {
        return Err((
            GIT_STATUS_NOT_FOUND,
            "submodule has no checkout (git submodule update)".to_string(),
        ));
    }
    let Some(joined) = joined.to_str() else {
        return Err((GIT_STATUS_OTHER, "non-UTF-8 submodule path".to_string()));
    };
    let (handle, info) = crate::open(joined)?;
    if handle.gitdir.as_ref() == parent.gitdir.as_ref() {
        return Err((
            GIT_STATUS_WRONG_TYPE,
            "path is not a submodule of that repository".to_string(),
        ));
    }
    Ok((handle, info))
}

/// The name of a repository's git directory or gitfile inside a worktree.
const GIT_DIR_NAME: &str = ".git";

#[cfg(test)]
mod tests {
    use super::fetch_fields_valid;

    #[test]
    fn fetch_fields_reject_options_and_nuls() {
        assert!(fetch_fields_valid("origin", &["refs/heads/main"]));
        assert!(!fetch_fields_valid("--upload-pack=bad", &[]));
        assert!(!fetch_fields_valid("bad\0remote", &[]));
        assert!(!fetch_fields_valid("origin", &["bad\0refspec"]));
    }
}
