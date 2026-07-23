//! `blit git` — repository introspection client (docs/git.md).
//!
//! Thin by design: open a repo, apply state snapshots or read objects,
//! print. No git logic lives here.

use crate::fs::handshake;
use crate::transport::{Transport, read_message, write_frame};
use blit_remote::S2C_QUIT;
use blit_remote::git::{
    FEATURE_GIT, GIT_CLOSED_CLIENT_REQUEST, GIT_COMMIT_LOSSY_ENCODING, GIT_COMMITS_MORE,
    GIT_DIFF_RENAMES, GIT_DIFF_UNTRACKED, GIT_ENDPOINT_COMMIT, GIT_ENDPOINT_EMPTY,
    GIT_ENDPOINT_INDEX, GIT_ENDPOINT_MERGE_BASE, GIT_ENDPOINT_WORKTREE, GIT_LOG_FIRST_PARENT,
    GIT_LOG_FOLLOW, GIT_LOG_FULL_MESSAGE, GIT_LOG_TOPO, GIT_OID_NONE, GIT_OPEN_STATUS,
    GIT_OPEN_TRACKING, GIT_OPEN_UNTRACKED, GIT_OPEN_WATCH, GIT_PATCH_TEXT, GIT_REPO_BARE,
    GIT_STATUS_OK, GIT_UPSTREAM_COUNTS_VALID, GitCommitRecord, GitDiffRecord, GitEndpoint, GitOid,
    GitStateMirror, S2C_GIT_CLOSED, S2C_GIT_COMMITS, S2C_GIT_DIFF, S2C_GIT_LOG_PAGE, S2C_GIT_PATCH,
    S2C_GIT_REPO, S2C_GIT_RESOLVE, S2C_GIT_STATE, git_commit_records, git_diff_records,
    git_status_text, msg_git_ack, msg_git_diff, msg_git_log, msg_git_log_ack, msg_git_log_watch,
    msg_git_open, msg_git_patch, msg_git_resolve, parse_git_closed, parse_git_commits,
    parse_git_diff_resp, parse_git_log_page, parse_git_patch_resp, parse_git_repo,
    parse_git_resolve_resp, parse_git_state,
};
use tokio::io::{AsyncRead, AsyncWrite};

const OPEN_NONCE: u16 = 1;
const REQ_NONCE: u16 = 2;
const RESOLVE_NONCE: u16 = 3;

fn hex(oid: &GitOid, len: usize) -> String {
    oid.iter()
        .take(len.div_ceil(2))
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..len]
        .to_string()
}

/// Normalize a client path filter to the fs-family wire form the server
/// decodes (docs/git.md: the GIT_LOG/GIT_DIFF/GIT_PATCH path filter is
/// escaped "exactly like FS_FETCH"): drop a leading `./` and escape a
/// literal `%` to `%25`. Matches `escape_wire` in the fs client.
fn escape_filter(path: &str) -> String {
    path.trim_start_matches("./").replace('%', "%25")
}

struct Session<R, W> {
    reader: R,
    writer: W,
    fragment_buf: Vec<u8>,
    repo_id: u16,
}

/// Handshake, open, and wait for `GIT_REPO`; fails on any open error.
async fn open_repo<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    path: &str,
    flags: u8,
) -> Result<(Session<R, W>, String), String> {
    let mut fragment_buf: Vec<u8> = Vec::new();
    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_GIT == 0 {
        return Err(
            "server does not support git introspection (upgrade blit on the remote)".into(),
        );
    }
    if !write_frame(&mut writer, &msg_git_open(OPEN_NONCE, flags, 0, 0, path)).await {
        return Err("connection closed".into());
    }
    loop {
        let Some(data) = read_message(&mut reader, &mut fragment_buf).await else {
            return Err("connection closed".into());
        };
        if data.first() != Some(&S2C_GIT_REPO) {
            if data.first() == Some(&S2C_QUIT) {
                return Err("server is shutting down".into());
            }
            continue;
        }
        let info = parse_git_repo(&data).ok_or("malformed GIT_REPO")?;
        if info.nonce != OPEN_NONCE {
            continue;
        }
        if info.status != GIT_STATUS_OK {
            return Err(format!("open failed: {}", info.workdir));
        }
        let workdir = if info.flags & GIT_REPO_BARE != 0 {
            format!("{} (bare)", info.gitdir)
        } else {
            info.workdir.to_string()
        };
        return Ok((
            Session {
                reader,
                writer,
                fragment_buf,
                repo_id: info.repo_id,
            },
            workdir,
        ));
    }
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> Session<R, W> {
    async fn recv(&mut self) -> Result<Vec<u8>, String> {
        loop {
            let Some(data) = read_message(&mut self.reader, &mut self.fragment_buf).await else {
                return Err("connection closed".into());
            };
            if data.is_empty() {
                continue;
            }
            if data[0] == S2C_QUIT {
                return Err("server is shutting down".into());
            }
            if data[0] == S2C_GIT_CLOSED
                && let Some((repo_id, reason)) = parse_git_closed(&data)
                && repo_id == self.repo_id
                && reason != GIT_CLOSED_CLIENT_REQUEST
            {
                return Err(format!("repository closed by server (reason {reason})"));
            }
            return Ok(data);
        }
    }
}

pub async fn cmd_status(
    transport: Transport,
    repo: String,
    watch: bool,
    json: bool,
) -> Result<(), String> {
    let (reader, writer) = transport.split();
    let flags = GIT_OPEN_STATUS | GIT_OPEN_UNTRACKED | GIT_OPEN_TRACKING;
    let (mut session, workdir) = open_repo(reader, writer, &repo, flags).await?;
    if !json {
        eprintln!("repository {workdir}");
    }
    let mut mirror = GitStateMirror::new();
    // What was last shown. The server re-snapshots on any settled repo
    // change, which can leave the view untouched (an unrelated ref moved,
    // an index refresh) — reprint only when the rendered output differs.
    let mut last: Option<String> = None;
    loop {
        let data = session.recv().await?;
        if data[0] != S2C_GIT_STATE {
            continue;
        }
        let Some((repo_id, _, _, _)) = parse_git_state(&data) else {
            continue;
        };
        if repo_id != session.repo_id {
            continue;
        }
        let Some(state_id) = mirror.apply_state(&data) else {
            return Err("malformed state from server".into());
        };
        if !write_frame(&mut session.writer, &msg_git_ack(session.repo_id, state_id)).await {
            return Err("connection closed".into());
        }
        let rendered = if json {
            state_json(&mirror)
        } else {
            render_status(&mirror)
        };
        if last.as_deref() != Some(rendered.as_str()) {
            if json {
                println!("{rendered}");
            } else {
                if last.is_some() {
                    println!();
                }
                print!("{rendered}");
                if last.is_none() && watch {
                    eprintln!("watching (ctrl-c to stop)…");
                }
            }
            last = Some(rendered);
        }
        if !watch {
            return Ok(());
        }
    }
}

fn state_json(mirror: &GitStateMirror) -> String {
    let head = mirror.head.as_ref();
    let upstream = head.and_then(|h| mirror.upstreams.get(&h.name));
    // Counts are only meaningful when COUNTS_VALID is set — an UPSTREAM
    // record clears it when the ref is GONE (counts forced to zero) or the
    // ahead/behind walk hit its cap. Emit null then so a consumer cannot
    // mistake "unknown"/"gone" for "in sync", mirroring render_status.
    let counts = upstream.filter(|u| u.flags & GIT_UPSTREAM_COUNTS_VALID != 0);
    serde_json::json!({
        "type": "state",
        "head": head.map(|h| h.name.clone()),
        "oid": head.map(|h| hex(&h.oid, 40)),
        "ahead": counts.map(|u| u.ahead),
        "behind": counts.map(|u| u.behind),
        "stashes": mirror.stashes.len(),
        "status": mirror
            .status
            .iter()
            .map(|s| {
                serde_json::json!({
                    "staged": (s.staged as char).to_string(),
                    "unstaged": (s.unstaged as char).to_string(),
                    "path": s.path,
                    "old_path": if s.old_path.is_empty() { None } else { Some(s.old_path.clone()) },
                })
            })
            .collect::<Vec<_>>(),
    })
    .to_string()
}

/// Render the status view; watch mode reprints only when this changes.
fn render_status(mirror: &GitStateMirror) -> String {
    let mut out = String::new();
    if let Some(head) = &mirror.head {
        let branch = head.name.strip_prefix("refs/heads/").unwrap_or(&head.name);
        let mut line = if branch.is_empty() {
            format!("HEAD detached at {}", hex(&head.oid, 8))
        } else {
            format!("on {branch}")
        };
        if let Some(upstream) = mirror.upstreams.get(&head.name)
            && upstream.flags & GIT_UPSTREAM_COUNTS_VALID != 0
        {
            if upstream.ahead > 0 {
                line.push_str(&format!(" ↑{}", upstream.ahead));
            }
            if upstream.behind > 0 {
                line.push_str(&format!(" ↓{}", upstream.behind));
            }
        }
        if !mirror.stashes.is_empty() {
            line.push_str(&format!(" [{} stashed]", mirror.stashes.len()));
        }
        out.push_str(&line);
        out.push('\n');
    }
    if mirror.status.is_empty() {
        out.push_str("clean\n");
        return out;
    }
    for entry in &mirror.status {
        let staged = entry.staged as char;
        let unstaged = entry.unstaged as char;
        if entry.old_path.is_empty() {
            out.push_str(&format!("{staged}{unstaged} {}\n", entry.path));
        } else {
            out.push_str(&format!(
                "{staged}{unstaged} {} -> {}\n",
                entry.old_path, entry.path
            ));
        }
    }
    out
}

/// Options for `blit git log`, assembled from the CLI args.
pub struct LogOpts {
    pub rev: Option<String>,
    pub path: Option<String>,
    pub limit: u16,
    pub watch: bool,
    pub follow: bool,
    pub first_parent: bool,
    pub full_message: bool,
    pub topo: bool,
    pub json: bool,
}

impl LogOpts {
    fn flags(&self) -> u8 {
        let mut f = 0u8;
        if self.follow {
            f |= GIT_LOG_FOLLOW;
        }
        if self.first_parent {
            f |= GIT_LOG_FIRST_PARENT;
        }
        if self.full_message {
            f |= GIT_LOG_FULL_MESSAGE;
        }
        if self.topo {
            f |= GIT_LOG_TOPO;
        }
        f
    }
}

/// Options for `blit git diff`, assembled from the CLI args.
pub struct DiffOpts {
    /// 0, 1, or 2 revisions, or a single `A..B` / `A...B` range.
    pub revs: Vec<String>,
    pub staged: bool,
    pub patch: bool,
    pub path: Option<String>,
    pub json: bool,
}

/// One commit record as pretty text or a rich JSON line. In text mode
/// `full_message` prints the body after the subject; JSON always carries
/// the whole message.
fn print_commit(record: &GitCommitRecord, json: bool, full_message: bool) {
    let GitCommitRecord::Commit {
        oid,
        tree,
        parents,
        author_time,
        author_tz,
        committer_time,
        committer_tz,
        author_name,
        author_email,
        committer_name,
        committer_email,
        message,
        flags,
        ..
    } = record
    else {
        return;
    };
    if json {
        println!(
            "{}",
            serde_json::json!({
                "type": "commit",
                "oid": hex(oid, 40),
                "tree": hex(tree, 40),
                "parents": parents.iter().map(|p| hex(p, 40)).collect::<Vec<_>>(),
                "author": { "name": author_name, "email": author_email,
                            "time": author_time, "tz": author_tz },
                "committer": { "name": committer_name, "email": committer_email,
                               "time": committer_time, "tz": committer_tz },
                "message": message,
                "lossy": flags & GIT_COMMIT_LOSSY_ENCODING != 0,
            })
        );
    } else {
        let subject = message.lines().next().unwrap_or("");
        println!(
            "{} {} <{}> {}",
            hex(oid, 8),
            author_name,
            author_email,
            subject
        );
        if full_message {
            for line in message.lines().skip(1) {
                println!("    {line}");
            }
        }
    }
}

pub async fn cmd_log(transport: Transport, repo: String, opts: LogOpts) -> Result<(), String> {
    let (reader, writer) = transport.split();
    // A watched log needs the ref-watch engine (WATCH), so it can re-emit
    // when the endpoints move.
    let open_flags = if opts.watch { GIT_OPEN_WATCH } else { 0 };
    let (mut session, _workdir) = open_repo(reader, writer, &repo, open_flags).await?;

    if opts.watch {
        if opts.path.is_some() {
            return Err("--watch does not support a path filter yet".into());
        }
        // Server-pushed live log: subscribe, print each page, ack.
        let spec = opts.rev.clone().unwrap_or_else(|| "HEAD".to_string());
        return watch_log(&mut session, &opts, spec).await;
    }

    // Resolve the rev/range to tips/hides (empty = HEAD).
    let (tips, hides) = match &opts.rev {
        None => (Vec::new(), Vec::new()),
        Some(spec) => resolve_spec(&mut session, spec).await?,
    };
    let path = escape_filter(&opts.path.clone().unwrap_or_default());

    if !write_frame(
        &mut session.writer,
        &msg_git_log(
            REQ_NONCE,
            session.repo_id,
            opts.flags(),
            opts.limit,
            &path,
            &tips,
            &hides,
        ),
    )
    .await
    {
        return Err("connection closed".into());
    }
    loop {
        let data = session.recv().await?;
        if data[0] != S2C_GIT_COMMITS {
            continue;
        }
        let page = parse_git_commits(&data).ok_or("malformed commits from server")?;
        if page.nonce != REQ_NONCE {
            continue;
        }
        if page.status != GIT_STATUS_OK {
            return Err(format!("log failed: {}", git_status_text(page.status)));
        }
        for record in git_commit_records(&page.records) {
            print_commit(&record, opts.json, opts.full_message);
        }
        if page.flags & GIT_COMMITS_MORE != 0 && !opts.json {
            eprintln!("… (more; raise -n)");
        }
        return Ok(());
    }
}

/// Resolve a revision spec server-side into (tips, hides) oids.
async fn resolve_spec<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session<R, W>,
    spec: &str,
) -> Result<(Vec<GitOid>, Vec<GitOid>), String> {
    if !write_frame(
        &mut session.writer,
        &msg_git_resolve(RESOLVE_NONCE, session.repo_id, spec),
    )
    .await
    {
        return Err("connection closed".into());
    }
    loop {
        let data = session.recv().await?;
        if data[0] != S2C_GIT_RESOLVE {
            continue;
        }
        let Some((nonce, status, tips, hides)) = parse_git_resolve_resp(&data) else {
            return Err("malformed resolve from server".into());
        };
        if nonce != RESOLVE_NONCE {
            continue;
        }
        if status != GIT_STATUS_OK {
            return Err(format!(
                "could not resolve '{spec}': {}",
                git_status_text(status)
            ));
        }
        return Ok((tips, hides));
    }
}

/// Resolve a revision to exactly one commit oid (docs/git.md GIT_RESOLVE).
/// Rejects anything that resolves to a range or a multi-commit set — a
/// range operator passed where a single endpoint is expected (e.g. as one
/// of two positional revisions) must fail loudly, not silently keep a tip.
async fn resolve_commit<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session<R, W>,
    rev: &str,
) -> Result<GitOid, String> {
    match resolve_spec(session, rev).await? {
        (tips, hides) if hides.is_empty() && tips.len() == 1 => Ok(tips[0]),
        _ => Err(format!("'{rev}' does not name a single commit")),
    }
}

/// Split a range spec on its operator, defaulting an omitted side to HEAD —
/// git's own `A..`, `..B`, `A...`, `...B` shorthands.
fn split_range<'a>(spec: &'a str, op: &str) -> (&'a str, &'a str) {
    let (a, b) = spec.split_once(op).unwrap_or((spec, ""));
    (
        if a.is_empty() { "HEAD" } else { a },
        if b.is_empty() { "HEAD" } else { b },
    )
}

/// The old side for a `--staged` diff with no explicit revision: the HEAD
/// commit, or EMPTY on an unborn branch (staged files are then additions
/// against nothing — a null oid would be rejected as NOT_FOUND).
async fn staged_head_endpoint<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session<R, W>,
) -> Result<GitEndpoint, String> {
    if !write_frame(
        &mut session.writer,
        &msg_git_log(REQ_NONCE, session.repo_id, 0, 1, "", &[], &[]),
    )
    .await
    {
        return Err("connection closed".into());
    }
    let head = loop {
        let data = session.recv().await?;
        if data[0] != S2C_GIT_COMMITS {
            continue;
        }
        let page = parse_git_commits(&data).ok_or("malformed commits")?;
        if page.nonce != REQ_NONCE {
            continue;
        }
        break git_commit_records(&page.records).find_map(|r| match r {
            GitCommitRecord::Commit { oid, .. } => Some(oid),
            _ => None,
        });
    };
    Ok(match head {
        Some(oid) => GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid,
        },
        None => GitEndpoint {
            kind: GIT_ENDPOINT_EMPTY,
            oid: GIT_OID_NONE,
        },
    })
}

/// Server-pushed live log: subscribe and reprint on every pushed page.
async fn watch_log<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session<R, W>,
    opts: &LogOpts,
    spec: String,
) -> Result<(), String> {
    const LOG_ID: u16 = 1;
    if !write_frame(
        &mut session.writer,
        &msg_git_log_watch(LOG_ID, session.repo_id, opts.flags(), opts.limit, &spec),
    )
    .await
    {
        return Err("connection closed".into());
    }
    loop {
        let data = session.recv().await?;
        if data[0] != S2C_GIT_LOG_PAGE {
            continue;
        }
        let page = parse_git_log_page(&data).ok_or("malformed log page from server")?;
        if page.log_id != LOG_ID {
            continue;
        }
        // Acknowledge so the server sends later updates.
        let _ = write_frame(
            &mut session.writer,
            &msg_git_log_ack(LOG_ID, session.repo_id, page.update_id),
        )
        .await;
        if page.status != GIT_STATUS_OK {
            // A ref may not exist yet; report and keep waiting.
            if !opts.json {
                eprintln!("(log unavailable: {})", git_status_text(page.status));
            }
            continue;
        }
        if opts.json {
            println!("{}", serde_json::json!({ "type": "page" }));
        } else {
            // Repaint: clear and redraw the current head page.
            print!("\x1b[2J\x1b[H");
        }
        for record in git_commit_records(&page.records) {
            print_commit(&record, opts.json, opts.full_message);
        }
        if page.flags & GIT_COMMITS_MORE != 0 && !opts.json {
            eprintln!("… (more; raise -n)");
        }
    }
}

pub async fn cmd_diff(transport: Transport, repo: String, opts: DiffOpts) -> Result<(), String> {
    let DiffOpts {
        revs,
        staged,
        patch,
        path,
        json,
    } = opts;
    let (reader, writer) = transport.split();
    let (mut session, _workdir) = open_repo(reader, writer, &repo, 0).await?;
    let filter = escape_filter(&path.unwrap_or_default());

    let commit = |oid| GitEndpoint {
        kind: GIT_ENDPOINT_COMMIT,
        oid,
    };
    let index = GitEndpoint {
        kind: GIT_ENDPOINT_INDEX,
        oid: GIT_OID_NONE,
    };
    let worktree = GitEndpoint {
        kind: GIT_ENDPOINT_WORKTREE,
        oid: GIT_OID_NONE,
    };
    let range_conflict = "--staged cannot be combined with a range or two revisions";

    // The two diff endpoints, git-diff style, from the positional revisions.
    // A range operator is split locally (each half is a server-resolved
    // revision); `A...B` becomes MERGE_BASE(A) vs B, which the server folds
    // to merge-base(A,B) vs B (docs/git.md endpoints).
    let (old, new) = match revs.as_slice() {
        [] if staged => (staged_head_endpoint(&mut session).await?, index),
        [] => (index, worktree),
        [one] if one.contains("...") => {
            if staged {
                return Err(range_conflict.into());
            }
            let (a, b) = split_range(one, "...");
            let a = resolve_commit(&mut session, a).await?;
            let b = resolve_commit(&mut session, b).await?;
            (
                GitEndpoint {
                    kind: GIT_ENDPOINT_MERGE_BASE,
                    oid: a,
                },
                commit(b),
            )
        }
        [one] if one.contains("..") => {
            if staged {
                return Err(range_conflict.into());
            }
            let (a, b) = split_range(one, "..");
            let a = resolve_commit(&mut session, a).await?;
            let b = resolve_commit(&mut session, b).await?;
            (commit(a), commit(b))
        }
        [one] => {
            let c = resolve_commit(&mut session, one).await?;
            (commit(c), if staged { index } else { worktree })
        }
        [a, b] => {
            if staged {
                return Err(range_conflict.into());
            }
            let a = resolve_commit(&mut session, a).await?;
            let b = resolve_commit(&mut session, b).await?;
            (commit(a), commit(b))
        }
        _ => return Err("git diff takes at most two revisions".into()),
    };
    // UNTRACKED only means something when the new side is the worktree.
    let flags = GIT_DIFF_RENAMES
        | if new.kind == GIT_ENDPOINT_WORKTREE {
            GIT_DIFF_UNTRACKED
        } else {
            0
        };

    // -p: request the unified patch (per-file hunks) instead of the list.
    if patch {
        if !write_frame(
            &mut session.writer,
            &msg_git_patch(
                REQ_NONCE + 2,
                session.repo_id,
                flags | GIT_PATCH_TEXT,
                3,
                old,
                new,
                &filter,
                0,
            ),
        )
        .await
        {
            return Err("connection closed".into());
        }
        loop {
            let data = session.recv().await?;
            if data[0] != S2C_GIT_PATCH {
                continue;
            }
            let Some((nonce, status, _flags, bytes)) = parse_git_patch_resp(&data) else {
                return Err("malformed patch from server".into());
            };
            if nonce != REQ_NONCE + 2 {
                continue;
            }
            if status != GIT_STATUS_OK {
                return Err(format!("patch failed: {}", git_status_text(status)));
            }
            let text = String::from_utf8_lossy(&bytes);
            if json {
                println!("{}", serde_json::json!({ "type": "patch", "text": text }));
            } else {
                print!("{text}");
            }
            return Ok(());
        }
    }

    // Default: the changed-file list.
    if !write_frame(
        &mut session.writer,
        &msg_git_diff(REQ_NONCE + 1, session.repo_id, flags, old, new, &filter),
    )
    .await
    {
        return Err("connection closed".into());
    }
    loop {
        let data = session.recv().await?;
        if data[0] != S2C_GIT_DIFF {
            continue;
        }
        let Some((nonce, status, _flags, records)) = parse_git_diff_resp(&data) else {
            return Err("malformed diff from server".into());
        };
        if nonce != REQ_NONCE + 1 {
            continue;
        }
        if status != GIT_STATUS_OK {
            return Err(format!("diff failed: {}", git_status_text(status)));
        }
        for record in git_diff_records(&records) {
            if let GitDiffRecord::Entry {
                st,
                old_path,
                new_path,
                ..
            } = record
            {
                if json {
                    println!(
                        "{}",
                        serde_json::json!({
                            "type": "entry",
                            "status": (st as char).to_string(),
                            "path": new_path,
                            "old_path": if old_path.is_empty() { None } else { Some(old_path) },
                        })
                    );
                } else if old_path.is_empty() || old_path == new_path {
                    println!("{} {new_path}", st as char);
                } else {
                    println!("{} {old_path} -> {new_path}", st as char);
                }
            }
        }
        return Ok(());
    }
}
