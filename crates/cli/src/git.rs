//! `blit git` — repository introspection client (docs/git.md).
//!
//! Thin by design: open a repo, apply state snapshots or read objects,
//! print. No git logic lives here.

use crate::fs::handshake;
use crate::transport::{Transport, read_message, write_frame};
use blit_remote::S2C_QUIT;
use blit_remote::git::{
    FEATURE_GIT, GIT_BLAME_FOLLOW_RENAMES, GIT_BLAME_TRUNCATED, GIT_BLOB_WHOLE,
    GIT_CLOSED_CLIENT_REQUEST, GIT_COMMIT_LOSSY_ENCODING, GIT_COMMITS_MORE, GIT_DIFF_RENAMES,
    GIT_DIFF_UNTRACKED, GIT_DISCOVER_BARE, GIT_DISCOVER_NESTED, GIT_DISCOVER_TRUNCATED,
    GIT_ENDPOINT_COMMIT, GIT_ENDPOINT_EMPTY, GIT_ENDPOINT_INDEX, GIT_ENDPOINT_MERGE_BASE,
    GIT_ENDPOINT_WORKTREE, GIT_FETCH_ANCHOR, GIT_FETCH_PRUNE, GIT_FOUND_BARE, GIT_FOUND_LINKED,
    GIT_LOG_FIRST_PARENT, GIT_LOG_FOLLOW, GIT_LOG_FULL_MESSAGE, GIT_LOG_TOPO, GIT_OID_NONE,
    GIT_OPEN_STATUS, GIT_OPEN_TRACKING, GIT_OPEN_UNTRACKED, GIT_OPEN_WATCH, GIT_OTYPE_BLOB,
    GIT_OTYPE_COMMIT, GIT_OTYPE_TREE, GIT_PATCH_BINARY, GIT_PATCH_TEXT, GIT_REFLOG_OLDEST_FIRST,
    GIT_REFLOG_TRUNCATED, GIT_REPO_BARE, GIT_STATUS_OK, GIT_UPSTREAM_COUNTS_VALID, GitBlameRecord,
    GitBlameRequest, GitBlobRequest, GitCommitRecord, GitDiffRecord, GitDiffRequest,
    GitDiscoverRecord, GitDiscoverRequest, GitEndpoint, GitFetchRecord, GitFetchRequest,
    GitIndexRecord, GitIndexRequest, GitOid, GitOpenRequest, GitPatchRequest, GitReflogRecord,
    GitReflogRequest, GitStateApply, GitStateMirror, GitTreeRecord, GitTreeRequest, S2C_GIT_BASE,
    S2C_GIT_BLAME, S2C_GIT_BLOB, S2C_GIT_CLOSED, S2C_GIT_COMMITS, S2C_GIT_DIFF, S2C_GIT_DISCOVER,
    S2C_GIT_FETCH, S2C_GIT_INDEX, S2C_GIT_LOG_PAGE, S2C_GIT_PATCH, S2C_GIT_REFLOG, S2C_GIT_REPO,
    S2C_GIT_RESOLVE, S2C_GIT_STATE, S2C_GIT_TREE, git_blame_records, git_commit_records,
    git_diff_records, git_discover_records, git_fetch_records, git_index_records,
    git_reflog_records, git_status_text, git_tree_records, msg_git_ack, msg_git_base,
    msg_git_blame, msg_git_blob, msg_git_diff, msg_git_discover, msg_git_fetch, msg_git_index,
    msg_git_log, msg_git_log_ack, msg_git_log_watch, msg_git_open, msg_git_patch, msg_git_reflog,
    msg_git_resolve, msg_git_tree, parse_git_base_resp, parse_git_blame_resp, parse_git_blob_resp,
    parse_git_closed, parse_git_commits, parse_git_diff_resp, parse_git_discover_resp,
    parse_git_fetch_resp, parse_git_index_resp, parse_git_log_page, parse_git_patch_resp,
    parse_git_reflog_resp, parse_git_repo, parse_git_resolve_resp, parse_git_state,
    parse_git_tree_resp,
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
    flags: u16,
) -> Result<(Session<R, W>, String), String> {
    let mut fragment_buf: Vec<u8> = Vec::new();
    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_GIT == 0 {
        return Err(
            "server does not support git introspection (upgrade blit on the remote)".into(),
        );
    }
    if !write_frame(
        &mut writer,
        &msg_git_open(&GitOpenRequest::new(OPEN_NONCE, flags, path)),
    )
    .await
    {
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
        let GitStateApply::Complete(state_id) = mirror.apply_state(&data) else {
            // A PARTIAL chunk is buffered, not acknowledged; anything
            // else was malformed and there is nothing to render.
            continue;
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
    /// With `patch`, emit binary content as git's `GIT binary patch` block.
    pub binary: bool,
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
        binary,
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
            &msg_git_patch(&GitPatchRequest {
                nonce: REQ_NONCE + 2,
                repo_id: session.repo_id,
                flags: u16::from(flags)
                    | GIT_PATCH_TEXT
                    | if binary { GIT_PATCH_BINARY } else { 0 },
                context: 3,
                rename: 0,
                old,
                new,
                path: &filter,
                max_len: 0,
                after: "",
                after_pos: 0,
            }),
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
        &msg_git_diff(&GitDiffRequest {
            nonce: REQ_NONCE + 1,
            repo_id: session.repo_id,
            flags,
            rename: 0,
            old,
            new,
            path: &filter,
            after: "",
        }),
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

// ── show / ls-tree / merge-base ────────────────────────────────────────────

/// Await one response with our request nonce, returning the whole frame.
async fn await_resp<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session<R, W>,
    opcode: u8,
) -> Result<Vec<u8>, String> {
    loop {
        let data = session.recv().await?;
        if data.first() == Some(&opcode) && data.len() >= 3 {
            let nonce = u16::from_le_bytes([data[1], data[2]]);
            if nonce == REQ_NONCE {
                return Ok(data);
            }
        }
    }
}

/// `blit git show REV:PATH` — an object's bytes, straight to stdout.
pub async fn cmd_show(
    transport: Transport,
    repo: String,
    spec: String,
    max_len: u32,
) -> Result<(), String> {
    use std::io::Write as _;
    // git's own `REV:PATH` shape. A bare rev with no colon means the
    // commit object itself, which the server renders as its raw text.
    let (rev, path) = match spec.split_once(':') {
        Some((r, p)) => (if r.is_empty() { "HEAD" } else { r }, p),
        None => (spec.as_str(), ""),
    };
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    let oid = resolve_commit(&mut session, rev).await?;
    if !write_frame(
        &mut session.writer,
        &msg_git_blob(&GitBlobRequest {
            nonce: REQ_NONCE,
            repo_id: session.repo_id,
            flags: GIT_BLOB_WHOLE,
            oid,
            path,
            offset: 0,
            max_len,
        }),
    )
    .await
    {
        return Err("connection closed".into());
    }
    let data = await_resp(&mut session, S2C_GIT_BLOB).await?;
    let Some((_, status, size, bytes)) = parse_git_blob_resp(&data) else {
        return Err("malformed blob response from server".into());
    };
    if status != GIT_STATUS_OK {
        return Err(format!("{spec}: {}", git_status_text(status)));
    }
    // Bytes through unaltered — this is `cat-file`, not a text filter.
    std::io::stdout()
        .write_all(&bytes)
        .map_err(|e| format!("writing stdout: {e}"))?;
    if (bytes.len() as u64) < size {
        eprintln!(
            "blit: truncated at {} of {size} bytes (raise --max-len)",
            bytes.len()
        );
    }
    Ok(())
}

/// `blit git ls-tree REV[:PATH]` — one tree level, as TSV.
pub async fn cmd_ls_tree(
    transport: Transport,
    repo: String,
    spec: String,
    json: bool,
) -> Result<(), String> {
    let (rev, path) = match spec.split_once(':') {
        Some((r, p)) => (if r.is_empty() { "HEAD" } else { r }, p),
        None => (spec.as_str(), ""),
    };
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    let oid = resolve_commit(&mut session, rev).await?;
    if !write_frame(
        &mut session.writer,
        &msg_git_tree(&GitTreeRequest {
            nonce: REQ_NONCE,
            repo_id: session.repo_id,
            flags: 0,
            oid,
            path,
            after: "",
        }),
    )
    .await
    {
        return Err("connection closed".into());
    }
    let data = await_resp(&mut session, S2C_GIT_TREE).await?;
    let Some((_, status, _, records)) = parse_git_tree_resp(&data) else {
        return Err("malformed tree response from server".into());
    };
    if status != GIT_STATUS_OK {
        return Err(format!("{spec}: {}", git_status_text(status)));
    }
    for rec in git_tree_records(&records) {
        // A CURSOR record can end a truncated listing; the CLI prints one
        // page, so entries are all it renders.
        let GitTreeRecord::Entry {
            otype,
            mode,
            oid,
            name,
        } = rec
        else {
            continue;
        };
        // git ls-tree's column order: MODE TYPE OID<TAB>NAME.
        let kind = match otype {
            GIT_OTYPE_TREE => "tree",
            GIT_OTYPE_BLOB => "blob",
            GIT_OTYPE_COMMIT => "commit",
            _ => "?",
        };
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "mode": format!("{mode:06o}"),
                    "type": kind,
                    "oid": hex(&oid, 40),
                    "name": name,
                })
            );
        } else {
            println!("{mode:06o} {kind} {}\t{name}", hex(&oid, 40));
        }
    }
    Ok(())
}

/// `blit git merge-base REV REV…` — the best common ancestors.
pub async fn cmd_merge_base(
    transport: Transport,
    repo: String,
    revs: Vec<String>,
    json: bool,
) -> Result<i32, String> {
    if revs.len() < 2 {
        return Err("merge-base needs at least two revisions".into());
    }
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    let mut oids = Vec::with_capacity(revs.len());
    for rev in &revs {
        oids.push(resolve_commit(&mut session, rev).await?);
    }
    if !write_frame(
        &mut session.writer,
        &msg_git_base(REQ_NONCE, session.repo_id, &oids),
    )
    .await
    {
        return Err("connection closed".into());
    }
    let data = await_resp(&mut session, S2C_GIT_BASE).await?;
    let Some((_, status, bases)) = parse_git_base_resp(&data) else {
        return Err("malformed merge-base response from server".into());
    };
    if status != GIT_STATUS_OK {
        return Err(format!("merge-base: {}", git_status_text(status)));
    }
    for b in &bases {
        if json {
            println!("{}", serde_json::json!({"oid": hex(b, 40)}));
        } else {
            println!("{}", hex(b, 40));
        }
    }
    // git merge-base exits 1 when the histories are unrelated.
    Ok(if bases.is_empty() { 1 } else { 0 })
}

/// `blit git ls-files` — the index, as TSV.
pub async fn cmd_ls_files(
    transport: Transport,
    repo: String,
    path: String,
    json: bool,
) -> Result<(), String> {
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    if !write_frame(
        &mut session.writer,
        &msg_git_index(&GitIndexRequest {
            nonce: REQ_NONCE,
            repo_id: session.repo_id,
            flags: 0,
            path: &path,
            after: "",
        }),
    )
    .await
    {
        return Err("connection closed".into());
    }
    let data = await_resp(&mut session, S2C_GIT_INDEX).await?;
    let Some((_, status, _, records)) = parse_git_index_resp(&data) else {
        return Err("malformed index response from server".into());
    };
    if status != GIT_STATUS_OK {
        return Err(format!("ls-files: {}", git_status_text(status)));
    }
    for rec in git_index_records(&records) {
        let GitIndexRecord::Entry {
            stage,
            mode,
            size,
            oid,
            path,
            ..
        } = rec
        else {
            continue;
        };
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "mode": format!("{mode:06o}"),
                    "stage": stage,
                    "oid": hex(&oid, 40),
                    "size": size,
                    "path": path,
                })
            );
        } else {
            // git ls-files --stage order: MODE OID STAGE<TAB>PATH, with
            // mode first so the columns line up with ls-tree's.
            println!("{mode:06o} {stage} {}\t{path}", hex(&oid, 40));
        }
    }
    Ok(())
}

/// `blit git blame` — one row per contiguous attributed range, which is
/// what the server computes. Authors are deliberately not here: resolve
/// the oids with `blit git log` when you want them.
#[allow(clippy::too_many_arguments)] // mirrors the subcommand's flags
pub async fn cmd_blame(
    transport: Transport,
    repo: String,
    path: String,
    rev: Option<String>,
    start: Option<u32>,
    lines: Option<u32>,
    follow: bool,
    json: bool,
) -> Result<(), String> {
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    // A range spec here would silently blame one endpoint, so it is
    // refused: resolve_commit exists for exactly that.
    let oid = match rev {
        Some(spec) => resolve_commit(&mut session, &spec).await?,
        None => GIT_OID_NONE,
    };
    let flags = if follow { GIT_BLAME_FOLLOW_RENAMES } else { 0 };
    let escaped = escape_filter(&path);
    let wanted = lines.unwrap_or(0);
    let mut next_line = start.unwrap_or(0);
    let mut delivered = 0u32;
    // The server caps one response at BLIT_GIT_BLAME_LINES_MAX lines and
    // says where it stopped; a whole-file blame of something large is
    // several of those, not a silently short answer.
    loop {
        let line_count = if wanted == 0 {
            0
        } else {
            wanted.saturating_sub(delivered)
        };
        if wanted != 0 && line_count == 0 {
            return Ok(());
        }
        if !write_frame(
            &mut session.writer,
            &msg_git_blame(&GitBlameRequest {
                nonce: REQ_NONCE,
                repo_id: session.repo_id,
                flags,
                oid,
                start_line: next_line,
                line_count,
                path: &escaped,
            }),
        )
        .await
        {
            return Err("connection closed".into());
        }
        let data = await_resp(&mut session, S2C_GIT_BLAME).await?;
        let Some((_, status, resp_flags, records)) = parse_git_blame_resp(&data) else {
            return Err("malformed blame response from server".into());
        };
        if status != GIT_STATUS_OK {
            return Err(format!("blame: {}", git_status_text(status)));
        }
        let mut cursor = None;
        for record in git_blame_records(&records) {
            match record {
                GitBlameRecord::Range {
                    commit,
                    start_line,
                    line_count,
                    orig_start,
                    orig_path,
                    ..
                } => {
                    delivered = delivered.saturating_add(line_count);
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "commit": hex(&commit, 40),
                                "start": start_line,
                                "lines": line_count,
                                "origStart": orig_start,
                                "origPath": orig_path,
                            })
                        );
                    } else {
                        let end = start_line + line_count - 1;
                        let from = if orig_path.is_empty() {
                            String::new()
                        } else {
                            format!("\t{orig_path}")
                        };
                        println!("{} {start_line}-{end}{from}", hex(&commit, 12));
                    }
                }
                GitBlameRecord::Cursor { pos, .. } => cursor = Some(pos),
            }
        }
        if resp_flags & GIT_BLAME_TRUNCATED == 0 {
            return Ok(());
        }
        let Some(pos) = cursor.and_then(|pos| u32::try_from(pos).ok()) else {
            if !json {
                eprintln!("… (truncated, and the server named no resume point)");
            }
            return Ok(());
        };
        next_line = pos.saturating_add(1);
    }
}

/// `blit git reflog` — including entries no ref can reach any more.
pub async fn cmd_reflog(
    transport: Transport,
    repo: String,
    ref_name: String,
    limit: u16,
    reverse: bool,
    json: bool,
) -> Result<(), String> {
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    let flags = if reverse { GIT_REFLOG_OLDEST_FIRST } else { 0 };
    let label = if ref_name.is_empty() {
        "HEAD"
    } else {
        &ref_name
    };
    // `-n` is what the caller asked for; the server's own entry budget can
    // cut a page shorter than that, so pages are followed until the
    // caller's count is met or the reflog runs out. The index keeps
    // counting across pages — it is the `@{n}` a caller pastes into git.
    let mut delivered = 0u64;
    loop {
        let page_limit = if limit == 0 {
            0
        } else {
            limit.saturating_sub(u16::try_from(delivered).unwrap_or(u16::MAX))
        };
        if limit != 0 && page_limit == 0 {
            if !json {
                eprintln!("… (more; raise -n)");
            }
            return Ok(());
        }
        let before = delivered;
        if !write_frame(
            &mut session.writer,
            &msg_git_reflog(&GitReflogRequest {
                nonce: REQ_NONCE,
                repo_id: session.repo_id,
                flags,
                limit: page_limit,
                ref_name: &ref_name,
                after_pos: delivered,
            }),
        )
        .await
        {
            return Err("connection closed".into());
        }
        let data = await_resp(&mut session, S2C_GIT_REFLOG).await?;
        let Some((_, status, resp_flags, records)) = parse_git_reflog_resp(&data) else {
            return Err("malformed reflog response from server".into());
        };
        if status != GIT_STATUS_OK {
            return Err(format!("reflog: {}", git_status_text(status)));
        }
        let mut cursor = None;
        for record in git_reflog_records(&records) {
            match record {
                GitReflogRecord::Entry {
                    old,
                    new,
                    msg,
                    time,
                    ..
                } => {
                    let n = delivered;
                    delivered += 1;
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "index": n,
                                "old": hex(&old, 40),
                                "new": hex(&new, 40),
                                "time": time,
                                "message": msg,
                            })
                        );
                    } else {
                        println!("{} {label}@{{{n}}}: {msg}", hex(&new, 12));
                    }
                }
                GitReflogRecord::Cursor { pos, .. } => cursor = Some(pos),
            }
        }
        if resp_flags & GIT_REFLOG_TRUNCATED == 0 {
            return Ok(());
        }
        // The cursor is where the server says it stopped; our own count is
        // the fallback. Either way a page that advanced nothing ends the
        // loop rather than spinning on it.
        let resumed = cursor.unwrap_or(delivered).max(delivered);
        if resumed == before {
            return Ok(());
        }
        delivered = resumed;
        if limit != 0 && delivered >= u64::from(limit) {
            if !json {
                eprintln!("… (more; raise -n)");
            }
            return Ok(());
        }
    }
}

/// `blit git discover` — repositories under a path, deduped by gitdir.
pub async fn cmd_discover(
    transport: Transport,
    path: String,
    depth: u8,
    nested: bool,
    bare: bool,
    json: bool,
) -> Result<(), String> {
    // No repo to open: discovery names repositories rather than using one.
    let (mut reader, mut writer) = transport.split();
    let mut fragment_buf: Vec<u8> = Vec::new();
    let features = handshake(&mut reader, &mut fragment_buf).await?;
    if features & FEATURE_GIT == 0 {
        return Err(
            "server does not support git introspection (upgrade blit on the remote)".into(),
        );
    }
    let mut flags = 0u8;
    if nested {
        flags |= GIT_DISCOVER_NESTED;
    }
    if bare {
        flags |= GIT_DISCOVER_BARE;
    }
    // The walk is capped at BLIT_GIT_DISCOVER_MAX repositories per
    // response and says where it stopped, so a tree with more than that
    // takes several requests — "what is under here" is a question with one
    // answer, not one page of one.
    let mut after = String::new();
    loop {
        if !write_frame(
            &mut writer,
            &msg_git_discover(&GitDiscoverRequest {
                nonce: REQ_NONCE,
                flags,
                depth,
                path: &path,
                after: &after,
            }),
        )
        .await
        {
            return Err("connection closed".into());
        }
        let (resp_flags, records) = loop {
            let Some(data) = read_message(&mut reader, &mut fragment_buf).await else {
                return Err("connection closed".into());
            };
            if data.first() != Some(&S2C_GIT_DISCOVER) {
                continue;
            }
            let Some((_, status, resp_flags, records)) = parse_git_discover_resp(&data) else {
                return Err("malformed discover response from server".into());
            };
            if status != GIT_STATUS_OK {
                return Err(format!("discover: {}", git_status_text(status)));
            }
            break (resp_flags, records);
        };
        let mut cursor = None;
        for record in git_discover_records(&records) {
            match record {
                GitDiscoverRecord::Repo {
                    flags,
                    workdir,
                    gitdir,
                } => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "workdir": workdir,
                                "gitdir": gitdir,
                                "bare": flags & GIT_FOUND_BARE != 0,
                                "linked": flags & GIT_FOUND_LINKED != 0,
                            })
                        );
                    } else {
                        println!("{workdir}\t{gitdir}");
                    }
                }
                GitDiscoverRecord::Cursor { after, .. } => cursor = Some(after.to_string()),
            }
        }
        if resp_flags & GIT_DISCOVER_TRUNCATED == 0 {
            return Ok(());
        }
        // Truncated with no cursor, or one that has not moved, is as far as
        // this walk goes; say so rather than stopping silently.
        match cursor {
            Some(cursor) if cursor != after => after = cursor,
            _ => {
                if !json {
                    eprintln!("… (truncated; raise BLIT_GIT_DISCOVER_SCAN_MAX on the server)");
                }
                return Ok(());
            }
        }
    }
}

/// `blit git fetch` — per-ref outcomes, and a non-zero exit when any ref
/// was refused. `git fetch` can exit 0 having refused one refspec of
/// several, which is the trap this exists to avoid.
#[allow(clippy::too_many_arguments)] // mirrors the subcommand's flags
pub async fn cmd_fetch(
    transport: Transport,
    repo: String,
    remote: String,
    refspecs: Vec<String>,
    prune: bool,
    anchor: bool,
    timeout: u32,
    json: bool,
) -> Result<i32, String> {
    let (reader, writer) = transport.split();
    let (mut session, _) = open_repo(reader, writer, &repo, 0).await?;
    let mut flags = 0u8;
    if prune {
        flags |= GIT_FETCH_PRUNE;
    }
    if anchor {
        flags |= GIT_FETCH_ANCHOR;
    }
    let specs: Vec<&str> = refspecs.iter().map(String::as_str).collect();
    if !write_frame(
        &mut session.writer,
        &msg_git_fetch(&GitFetchRequest {
            nonce: REQ_NONCE,
            repo_id: session.repo_id,
            flags,
            timeout_ms: timeout.saturating_mul(1000),
            remote: &remote,
            refspecs: specs,
        }),
    )
    .await
    {
        return Err("connection closed".into());
    }
    let data = await_resp(&mut session, S2C_GIT_FETCH).await?;
    let Some((_, status, _, records)) = parse_git_fetch_resp(&data) else {
        return Err("malformed fetch response from server".into());
    };
    if status != GIT_STATUS_OK {
        return Err(format!("fetch: {}", git_status_text(status)));
    }
    let mut refused = false;
    for record in git_fetch_records(&records) {
        let GitFetchRecord::Ref {
            status,
            old,
            new,
            name,
            detail,
            ..
        } = record;
        if status != GIT_STATUS_OK {
            refused = true;
        }
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "ref": name,
                    "old": hex(&old, 40),
                    "new": hex(&new, 40),
                    "ok": status == GIT_STATUS_OK,
                    "detail": detail,
                })
            );
        } else if status == GIT_STATUS_OK {
            println!("{} {} {name}", hex(&old, 12), hex(&new, 12));
        } else if name.is_empty() {
            // A whole-fetch failure has no ref to name, only git's word.
            eprintln!("! {detail}");
        } else {
            eprintln!("! {name}: {detail}");
        }
    }
    Ok(if refused { 1 } else { 0 })
}
