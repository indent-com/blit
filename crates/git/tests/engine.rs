//! End-to-end engine tests against fixture repositories built with the
//! real git CLI (docs/git.md).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blit_git::{Cancel, RepoHandle, StateOptions, open};
use blit_remote::git::*;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "blit-git-test-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A `git` invocation that sees no configuration but the fixture repo's
/// own: the developer's `~/.gitconfig` is not part of the fixture, and it
/// changes what these tests observe. `tag.gpgSign` turns `git tag v1` into
/// an annotated tag that then fails for want of a message; `diff.algorithm`
/// and `diff.mnemonicPrefix` rewrite the very bytes the oracle comparisons
/// against the real CLI diff against (`a/`…`b/` becomes `c/`…`w/`).
/// Identity is set for the opposite reason — a box with no `user.email`
/// cannot commit at all.
fn git_cmd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        // The same door held open by `-c`, which reaches a child process
        // through the environment rather than through argv.
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args);
    cmd
}

fn git(dir: &Path, args: &[&str]) {
    let out = git_cmd(dir, args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repo with two commits, a branch, a tag, a stash, staged + unstaged +
/// untracked changes.
fn fixture() -> PathBuf {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::write(dir.join("b.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    std::fs::write(dir.join("a.txt"), "alpha\nBETA\ngamma\n").unwrap();
    git(&dir, &["commit", "-am", "second\n\nbody here"]);
    git(&dir, &["tag", "-a", "v1", "-m", "tag v1"]);
    // Staged change, unstaged change, untracked file.
    std::fs::write(dir.join("b.txt"), "one\ntwo\n").unwrap();
    git(&dir, &["add", "b.txt"]);
    std::fs::write(dir.join("a.txt"), "alpha\nBETA\ngamma\ndelta\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();
    dir
}

/// Run git and return its stdout (trimmed of a trailing newline), asserting
/// success — for oracle comparisons against the real CLI.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir, args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

/// Drive a state engine to its first snapshot and return that message.
fn wait_first_state(handle: &RepoHandle, opts: StateOptions) -> Vec<u8> {
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let msg = loop {
        if let Some(m) = sent.lock().unwrap().first().cloned() {
            break m;
        }
        assert!(Instant::now() < deadline, "no snapshot arrived");
        std::thread::sleep(Duration::from_millis(10));
    };
    state.stop();
    msg
}

fn rev(dir: &Path, spec: &str) -> GitOid {
    let out = git_cmd(dir, &["rev-parse", spec]).output().unwrap();
    let hex = String::from_utf8(out.stdout).unwrap();
    let mut oid = GIT_OID_NONE;
    for (i, chunk) in hex.trim().as_bytes().chunks(2).enumerate() {
        oid[i] = u8::from_str_radix(std::str::from_utf8(chunk).unwrap(), 16).unwrap();
    }
    oid
}

#[test]
fn open_reports_repo() {
    let dir = fixture();
    let (_handle, info) = open(dir.to_str().unwrap()).expect("open");
    assert_eq!(info.oid_format, GIT_OID_FORMAT_SHA1);
    assert_eq!(info.flags & GIT_REPO_BARE, 0);
    assert!(
        info.workdir
            .ends_with(dir.file_name().unwrap().to_str().unwrap())
    );
    // Opening a subpath discovers upward.
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let (_h2, info2) = open(sub.to_str().unwrap()).expect("open subdir");
    assert_eq!(info2.workdir, info.workdir);
    // A non-repo directory is WRONG_TYPE; a missing path NOT_FOUND; an
    // empty/NUL path INVALID (docs/protocol.md common status registry).
    let plain = temp_dir();
    assert_eq!(
        open(plain.to_str().unwrap()).err().unwrap().0,
        GIT_STATUS_WRONG_TYPE
    );
    let missing = plain.join("does-not-exist");
    assert_eq!(
        open(missing.to_str().unwrap()).err().unwrap().0,
        GIT_STATUS_NOT_FOUND
    );
    assert_eq!(open("").err().unwrap().0, GIT_STATUS_INVALID);
    assert_eq!(open("bad\0path").err().unwrap().0, GIT_STATUS_INVALID);
}

#[test]
fn state_snapshot_records() {
    let dir = fixture();
    git(&dir, &["stash", "push", "-m", "wip"]);
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        status: true,
        untracked: true,
        tracking: true,
        ..Default::default()
    };
    let state = handle.start_state(
        7,
        opts,
        Box::new(move |msg| {
            sent2.lock().unwrap().push(msg);
            true
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let msg = loop {
        if let Some(msg) = sent.lock().unwrap().first().cloned() {
            break msg;
        }
        assert!(Instant::now() < deadline, "no snapshot arrived");
        std::thread::sleep(Duration::from_millis(10));
    };
    let (repo_id, state_id, _flags, records) = parse_git_state(&msg).expect("valid state");
    assert_eq!(repo_id, 7);
    let mut mirror = GitStateMirror::new();
    assert_eq!(mirror.apply_state(&msg).complete(), Some(state_id));
    let head = mirror.head.as_ref().expect("head record");
    assert_eq!(head.name, "refs/heads/main");
    assert!(mirror.refs.contains_key("refs/heads/main"));
    let tag = mirror.refs.get("refs/tags/v1").expect("tag ref");
    assert_ne!(tag.flags & GIT_REF_PEELED_VALID, 0, "annotated tag peels");
    assert_eq!(mirror.stashes.len(), 1);
    assert!(mirror.stashes[0].message.contains("wip"));
    // The stash reverted staged and unstaged changes to HEAD; only the
    // untracked file remains, as '??'.
    assert_eq!(mirror.status.len(), 1, "status: {:?}", mirror.status);
    let untracked = &mirror.status[0];
    assert_eq!(untracked.path, "untracked.txt");
    assert_eq!((untracked.staged, untracked.unstaged), (b'?', b'?'));
    let _ = records;
    state.stop();
}

/// In-progress operations surface as an OP record plus their pseudo-refs
/// (docs/design/git.md): a conflicted merge streams MERGE_HEAD/ORIG_HEAD,
/// a conflicted rebase streams REBASE_HEAD and step/total in `detail`.
#[test]
fn op_state_and_special_refs() {
    // Run git expecting failure to be fine (conflicts exit nonzero).
    let git_any = |dir: &Path, args: &[&str]| {
        let _ = git_cmd(dir, args).output().expect("run git");
    };
    let state_mirror = |dir: &Path| {
        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let msg = wait_first_state(&handle, StateOptions::default());
        let mut mirror = GitStateMirror::new();
        mirror.apply_state(&msg).complete().expect("valid state");
        mirror
    };

    // Conflicted merge.
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "base\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "base"]);
    git(&dir, &["checkout", "-b", "side"]);
    std::fs::write(dir.join("f.txt"), "side\n").unwrap();
    git(&dir, &["commit", "-am", "side"]);
    git(&dir, &["checkout", "main"]);
    std::fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-am", "main"]);
    git_any(&dir, &["merge", "side"]);
    let mirror = state_mirror(&dir);
    let op = mirror.op.as_ref().expect("merge op record");
    assert_eq!(op.op, GIT_OP_MERGE);
    assert_eq!(op.oid, rev(&dir, "side"));
    let merge_head = mirror.refs.get("MERGE_HEAD").expect("MERGE_HEAD ref");
    assert_eq!(merge_head.oid, rev(&dir, "side"));
    assert!(mirror.refs.contains_key("ORIG_HEAD"));

    // Conflicted rebase of two commits: stops on the first, so 1/2.
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "base\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "base"]);
    git(&dir, &["checkout", "-b", "topic"]);
    std::fs::write(dir.join("f.txt"), "topic-1\n").unwrap();
    git(&dir, &["commit", "-am", "topic 1"]);
    std::fs::write(dir.join("f.txt"), "topic-2\n").unwrap();
    git(&dir, &["commit", "-am", "topic 2"]);
    git(&dir, &["checkout", "main"]);
    std::fs::write(dir.join("f.txt"), "main\n").unwrap();
    git(&dir, &["commit", "-am", "main"]);
    git(&dir, &["checkout", "topic"]);
    git_any(&dir, &["rebase", "main"]);
    let mirror = state_mirror(&dir);
    let op = mirror.op.as_ref().expect("rebase op record");
    assert_eq!(op.op, GIT_OP_REBASE);
    assert_eq!(op.detail, "1/2");
    // REBASE_HEAD is the commit being applied — the first of the two.
    let rebase_head = mirror.refs.get("REBASE_HEAD").expect("REBASE_HEAD ref");
    assert_eq!(rebase_head.oid, rev(&dir, "topic~1"));
    assert!(mirror.refs.contains_key("ORIG_HEAD"));
}

/// Tracking state against a real clone, plus watch-driven live updates
/// under the coalescing ack discipline.
#[test]
fn tracking_and_live_updates() {
    let upstream = fixture();
    git(&upstream, &["stash", "push", "-u", "-m", "clean"]);
    let clone = temp_dir();
    git(
        &clone,
        &[
            "clone",
            upstream.to_str().unwrap(),
            clone.join("c").to_str().unwrap(),
        ],
    );
    let workdir = clone.join("c");
    // One local commit (ahead 1); one upstream commit fetched (behind 1).
    std::fs::write(workdir.join("local.txt"), "local\n").unwrap();
    git(&workdir, &["add", "."]);
    git(&workdir, &["commit", "-m", "local work"]);
    std::fs::write(upstream.join("up.txt"), "up\n").unwrap();
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-m", "upstream work"]);
    git(&workdir, &["fetch", "origin"]);

    let (handle, _info) = open(workdir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        tracking: true,
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        3,
        opts,
        Box::new(move |msg| {
            sent2.lock().unwrap().push(msg);
            true
        }),
    );
    let wait_msg = |count: usize| -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Some(msg) = sent.lock().unwrap().get(count - 1).cloned() {
                return msg;
            }
            assert!(Instant::now() < deadline, "snapshot {count} never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let first = wait_msg(1);
    let mut mirror = GitStateMirror::new();
    let id = mirror.apply_state(&first).complete().expect("valid state");
    let up = mirror
        .upstreams
        .get("refs/heads/main")
        .expect("upstream record");
    assert_ne!(up.flags & GIT_UPSTREAM_COUNTS_VALID, 0);
    assert_eq!((up.ahead, up.behind), (1, 1));
    assert!(up.upstream.contains("origin/main"));

    // Coalescing: the next snapshot needs the ack first.
    state.ack(id);
    let old_head = mirror.head.as_ref().unwrap().oid;
    std::fs::write(workdir.join("more.txt"), "more\n").unwrap();
    git(&workdir, &["add", "."]);
    git(&workdir, &["commit", "-m", "another"]);
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        let count = sent.lock().unwrap().len();
        if count >= 2 {
            let msg = wait_msg(count);
            let id = mirror.apply_state(&msg).complete().expect("valid update");
            if mirror.head.as_ref().unwrap().oid != old_head {
                break;
            }
            state.ack(id);
        }
        assert!(
            Instant::now() < deadline,
            "watch never delivered the new HEAD"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        mirror.upstreams.get("refs/heads/main").map(|u| u.ahead),
        Some(2)
    );
    state.stop();
}

/// Ahead/behind counts stay correct across snapshots whose `(tip,
/// upstream)` pair did not move — the memo-hit path — including after an
/// unrelated ref change forces a fresh snapshot.
#[test]
fn ahead_behind_memo_survives_unrelated_ref_change() {
    let upstream = fixture();
    git(&upstream, &["stash", "push", "-u", "-m", "clean"]);
    let clone = temp_dir();
    git(
        &clone,
        &[
            "clone",
            upstream.to_str().unwrap(),
            clone.join("c").to_str().unwrap(),
        ],
    );
    let workdir = clone.join("c");
    std::fs::write(workdir.join("local.txt"), "local\n").unwrap();
    git(&workdir, &["add", "."]);
    git(&workdir, &["commit", "-m", "local work"]);
    std::fs::write(upstream.join("up.txt"), "up\n").unwrap();
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-m", "upstream work"]);
    git(&workdir, &["fetch", "origin"]);

    let (handle, _info) = open(workdir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        tracking: true,
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |msg| {
            sent2.lock().unwrap().push(msg);
            true
        }),
    );
    let mut mirror = GitStateMirror::new();
    let mut applied = 0usize;
    let mut wait_until =
        |mirror: &mut GitStateMirror, label: &str, pred: &dyn Fn(&GitStateMirror) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                let next = sent.lock().unwrap().get(applied).cloned();
                if let Some(msg) = next {
                    applied += 1;
                    let id = mirror.apply_state(&msg).complete().expect("valid state");
                    state.ack(id);
                    if pred(mirror) {
                        return;
                    }
                } else {
                    assert!(Instant::now() < deadline, "{label}: never satisfied");
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        };
    let counts = |m: &GitStateMirror| {
        m.upstreams
            .get("refs/heads/main")
            .map(|u| (u.flags, u.ahead, u.behind))
    };
    wait_until(&mut mirror, "initial-counts", &|m| counts(m).is_some());
    assert_eq!(counts(&mirror), Some((GIT_UPSTREAM_COUNTS_VALID, 1, 1)));

    // An unrelated branch forces a fresh snapshot; main's (tip, upstream)
    // pair is unchanged, so its counts come from the memo — and must be
    // the same numbers.
    git(&workdir, &["branch", "unrelated"]);
    wait_until(&mut mirror, "unrelated-ref", &|m| {
        m.refs.contains_key("refs/heads/unrelated")
    });
    assert_eq!(counts(&mirror), Some((GIT_UPSTREAM_COUNTS_VALID, 1, 1)));

    // Moving the tip invalidates the pair: ahead becomes 2.
    std::fs::write(workdir.join("more.txt"), "more\n").unwrap();
    git(&workdir, &["add", "."]);
    git(&workdir, &["commit", "-m", "another"]);
    wait_until(&mut mirror, "tip-moved", &|m| {
        counts(m) == Some((GIT_UPSTREAM_COUNTS_VALID, 2, 1))
    });
    state.stop();
}

#[test]
fn log_pages_and_follows() {
    let dir = fixture();
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    // Page of 1 with MORE + frontier, then continuation.
    let req = GitLogRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        limit: 1,
        path: "",
        tips: vec![],
        hides: vec![],
    };
    let resp = handle.log(&req, &cancel);
    let page = parse_git_commits(&resp).expect("commits");
    assert_eq!(page.status, GIT_STATUS_OK);
    assert_ne!(page.flags & GIT_COMMITS_MORE, 0);
    assert_eq!(page.frontier.len(), 1);
    let commits: Vec<_> = git_commit_records(&page.records).collect();
    assert_eq!(commits.len(), 1);
    match &commits[0] {
        GitCommitRecord::Commit { message, .. } => assert_eq!(*message, "second"),
        other => panic!("unexpected {other:?}"),
    }
    // Continuation reaches the root commit.
    let req2 = GitLogRequest {
        nonce: 2,
        tips: page.frontier.clone(),
        ..req
    };
    let resp2 = handle.log(&req2, &cancel);
    let page2 = parse_git_commits(&resp2).unwrap();
    let commits2: Vec<_> = git_commit_records(&page2.records).collect();
    assert!(matches!(
        &commits2[0],
        GitCommitRecord::Commit { message, .. } if *message == "first"
    ));
    // FULL_MESSAGE includes the body.
    let req3 = GitLogRequest {
        nonce: 3,
        flags: GIT_LOG_FULL_MESSAGE,
        limit: 1,
        tips: vec![],
        hides: vec![],
        path: "",
        repo_id: 0,
    };
    let page3 = parse_git_commits(&handle.log(&req3, &cancel)).unwrap();
    assert!(matches!(
        git_commit_records(&page3.records).next().unwrap(),
        GitCommitRecord::Commit { message, .. } if message.contains("body here")
    ));
}

#[test]
fn tree_blob_and_base() {
    let dir = fixture();
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let head = rev(&dir, "HEAD");
    // Tree listing at HEAD (peeled from commit).
    let resp = handle.tree(
        &GitTreeRequest {
            nonce: 1,
            repo_id: 0,
            flags: 0,
            oid: head,
            path: "",
            after: "",
        },
        &cancel,
    );
    let (nonce, status, _flags, records) = parse_git_tree_resp(&resp).unwrap();
    assert_eq!((nonce, status), (1, GIT_STATUS_OK));
    let names: Vec<String> = git_tree_records(&records)
        .filter_map(|record| match record {
            GitTreeRecord::Entry { name, .. } => Some(name.to_string()),
            GitTreeRecord::Cursor { .. } => None,
        })
        .collect();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
    // Blob by commit + path.
    let resp = handle.blob(&GitBlobRequest {
        nonce: 2,
        repo_id: 0,
        flags: GIT_BLOB_WHOLE,
        oid: head,
        path: "a.txt",
        offset: 0,
        max_len: 0,
    });
    let (_, status, size, data) = parse_git_blob_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(size as usize, data.len());
    assert_eq!(data, b"alpha\nBETA\ngamma\n");
    // TOO_LARGE carries the true size.
    let resp = handle.blob(&GitBlobRequest {
        nonce: 3,
        repo_id: 0,
        flags: GIT_BLOB_WHOLE,
        oid: head,
        path: "a.txt",
        offset: 0,
        max_len: 4,
    });
    let (_, status, size, data) = parse_git_blob_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_TOO_LARGE);
    assert_eq!(size, 17);
    assert!(data.is_empty());
    // Merge base of HEAD and HEAD~1 is HEAD~1.
    let parent = rev(&dir, "HEAD~1");
    let resp = handle.base(4, &[head, parent], &cancel);
    let (_, status, bases) = parse_git_base_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(bases, vec![parent]);
    let resp = handle.base(5, &[head], &cancel);
    let (_, status, _) = parse_git_base_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_INVALID);
}

#[test]
fn diff_endpoints_and_patch_rows() {
    let dir = fixture();
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let commit = |oid| GitEndpoint {
        kind: GIT_ENDPOINT_COMMIT,
        oid,
    };
    let plain = |kind| GitEndpoint {
        kind,
        oid: GIT_OID_NONE,
    };
    // Between commits: a.txt modified.
    let req = GitDiffRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: commit(rev(&dir, "HEAD~1")),
        new: commit(rev(&dir, "HEAD")),
        path: "",
        after: "",
    };
    let (_, status, _flags, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let entries: Vec<_> = git_diff_records(&records).collect();
    assert!(matches!(
        &entries[0],
        GitDiffRecord::Entry { st: b'M', new_path, .. } if *new_path == "a.txt"
    ));
    // Staged: HEAD×INDEX shows b.txt.
    let req = GitDiffRequest {
        nonce: 2,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: commit(rev(&dir, "HEAD")),
        new: plain(GIT_ENDPOINT_INDEX),
        path: "",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    let staged: Vec<String> = git_diff_records(&records)
        .filter_map(|r| match r {
            GitDiffRecord::Entry {
                st: b'M', new_path, ..
            } => Some(new_path.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(staged, vec!["b.txt"]);
    // Unstaged incl untracked: INDEX×WORKTREE.
    let req = GitDiffRequest {
        nonce: 3,
        repo_id: 0,
        flags: GIT_DIFF_UNTRACKED,
        rename: 0,
        old: plain(GIT_ENDPOINT_INDEX),
        new: plain(GIT_ENDPOINT_WORKTREE),
        path: "",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    let mut unstaged: Vec<(u8, String)> = git_diff_records(&records)
        .filter_map(|r| match r {
            GitDiffRecord::Entry { st, new_path, .. } => Some((st, new_path.to_string())),
            _ => None,
        })
        .collect();
    unstaged.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        unstaged,
        vec![(b'M', "a.txt".into()), (b'A', "untracked.txt".into())]
    );
    // MERGE_BASE endpoint reveals the base and diffs base..topic.
    let req = GitDiffRequest {
        nonce: 4,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: rev(&dir, "HEAD~1"),
        },
        new: commit(rev(&dir, "HEAD")),
        path: "",
        after: "",
    };
    let (_, status, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert!(matches!(
        git_diff_records(&records).next().unwrap(),
        GitDiffRecord::Base { oid } if oid == rev(&dir, "HEAD~1")
    ));
    // Structured patch rows with word spans: BETA changed on one row.
    let req = GitPatchRequest {
        nonce: 5,
        repo_id: 0,
        flags: 0,
        context: 1,
        rename: 0,
        old: commit(rev(&dir, "HEAD~1")),
        new: commit(rev(&dir, "HEAD")),
        path: "",
        max_len: 0,
        after: "",
        after_pos: 0,
    };
    let (_, status, pflags, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_ne!(pflags & GIT_PATCH_STRUCTURED, 0);
    let rows: Vec<_> = git_patch_records(&data).collect();
    assert!(matches!(
        &rows[0],
        GitPatchRecord::File { new_path, .. } if *new_path == "a.txt"
    ));
    let changed_row = rows
        .iter()
        .find_map(|r| match r {
            GitPatchRecord::Row {
                old_text,
                new_text,
                old_spans,
                new_spans,
                ..
            } if !old_spans.is_empty() || !new_spans.is_empty() => {
                Some((old_text.to_vec(), new_text.to_vec()))
            }
            _ => None,
        })
        .expect("a changed row with spans");
    assert_eq!(changed_row, (b"beta".to_vec(), b"BETA".to_vec()));
    // TEXT mode emits a unified diff.
    let req = GitPatchRequest {
        nonce: 6,
        flags: GIT_PATCH_TEXT,
        ..req
    };
    let (_, status, pflags, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(pflags & GIT_PATCH_STRUCTURED, 0);
    let text = String::from_utf8(data).unwrap();
    assert!(text.contains("--- a/a.txt"), "unified headers: {text}");
    assert!(text.contains("-beta") && text.contains("+BETA"));
    // Whitespace-only change is dropped under IGNORE_ALL_SPACE.
    std::fs::write(dir.join("a.txt"), "alpha\nBETA\ngamma\ndelta \n").unwrap();
    git(&dir, &["add", "a.txt"]);
    std::fs::write(dir.join("a.txt"), "alpha\nBETA\ngamma\n delta  \n").unwrap();
    let req = GitDiffRequest {
        nonce: 7,
        repo_id: 0,
        flags: GIT_DIFF_IGNORE_ALL_SPACE,
        rename: 0,
        old: plain(GIT_ENDPOINT_INDEX),
        new: plain(GIT_ENDPOINT_WORKTREE),
        path: "a.txt",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert_eq!(git_diff_records(&records).count(), 0, "ws-only drop");
}

/// A same-size rewrite in the same wall-clock second as the index stat —
/// the racy-git case — must still show as modified (nanosecond mtimes).
#[test]
fn same_second_same_size_rewrite_is_detected() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("r.txt"), "aaaa\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::write(dir.join("r.txt"), "bbbb\n").unwrap();
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitDiffRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert!(
        git_diff_records(&records).any(|r| matches!(r, GitDiffRecord::Entry { st: b'M', .. })),
        "racy same-second rewrite missed"
    );
}

/// The same case one step worse: a same-size rewrite whose mtime is
/// byte-identical to the one the index recorded, which is what a write
/// landing in the same coarse-clock tick as the `git add` produces. Stat
/// equality proves nothing inside the index's own mtime, so the content
/// must be read — git's racy-index rule.
#[test]
fn rewrite_with_the_indexed_mtime_is_detected() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    let file = dir.join("r.txt");
    std::fs::write(&file, "aaaa\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    // What `git add` recorded for r.txt, straight from the file it stat'd.
    let indexed = std::fs::metadata(&file).unwrap().modified().unwrap();
    std::fs::write(&file, "bbbb\n").unwrap();
    // Replay the tick collision. One tick means one timestamp for all of
    // it: the rewrite carries the mtime the index recorded, and the index
    // itself was written in that same tick.
    let stamp = std::fs::FileTimes::new().set_modified(indexed);
    for path in [&file, &dir.join(".git/index")] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(stamp)
            .unwrap();
    }

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitDiffRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert!(
        git_diff_records(&records).any(|r| matches!(r, GitDiffRecord::Entry { st: b'M', .. })),
        "rewrite inside the index's own mtime missed"
    );
}

#[test]
fn index_entries_and_rename() {
    let dir = fixture();
    // Settle the staged content first so the rename is exact-oid.
    git(&dir, &["commit", "-am", "third"]);
    git(&dir, &["mv", "b.txt", "c.txt"]);
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let resp = handle.index(
        &GitIndexRequest {
            nonce: 1,
            repo_id: 0,
            flags: 0,
            path: "",
            after: "",
        },
        &cancel,
    );
    let (_, status, _flags, records) = parse_git_index_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let paths: Vec<String> = git_index_records(&records)
        .filter_map(|r| match r {
            GitIndexRecord::Entry { path, stage, .. } => {
                assert_eq!(stage, 0);
                Some(path.to_string())
            }
            GitIndexRecord::Cursor { .. } => None,
        })
        .collect();
    assert_eq!(paths, vec!["a.txt", "c.txt"]);
    // Staged rename detected by exact oid.
    let req = GitDiffRequest {
        nonce: 2,
        repo_id: 0,
        flags: GIT_DIFF_RENAMES,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: rev(&dir, "HEAD"),
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        path: "",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    let rename = git_diff_records(&records)
        .find_map(|r| match r {
            GitDiffRecord::Entry {
                st: b'R',
                similarity,
                old_path,
                new_path,
                ..
            } => Some((similarity, old_path.to_string(), new_path.to_string())),
            _ => None,
        })
        .expect("rename entry");
    assert_eq!(rename, (100, "b.txt".into(), "c.txt".into()));
}

/// `rename` 100 is the strictest percentage, not a second way to say 0: the
/// scorer runs and only a pair that scores identical joins. A mode change on
/// a moved file is exactly that pair — the blobs differ in no bytes, but the
/// tree entries differ, so the exact-oid join can see it while a 100%
/// content score still matches.
#[test]
fn rename_threshold_100_still_scores() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    // Two files, so the candidate sets are non-trivial: one moves with a
    // small edit (well under 100% similar), one moves untouched.
    std::fs::write(dir.join("same.txt"), "l1\nl2\nl3\nl4\n").unwrap();
    std::fs::write(dir.join("edited.txt"), "a1\na2\na3\na4\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    // `git mv` then rewrite the blob byte-for-byte identically under a new
    // path: the oid is unchanged, so this is the exact-join case.
    std::fs::rename(dir.join("same.txt"), dir.join("moved.txt")).unwrap();
    std::fs::rename(dir.join("edited.txt"), dir.join("changed.txt")).unwrap();
    std::fs::write(dir.join("changed.txt"), "a1\nA2\na3\na4\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-m", "second"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let renames = |threshold: u8| {
        let req = GitDiffRequest {
            nonce: 1,
            repo_id: 0,
            flags: GIT_DIFF_RENAMES,
            rename: threshold,
            old: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&dir, "HEAD~1"),
            },
            new: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&dir, "HEAD"),
            },
            path: "",
            after: "",
        };
        let (_, status, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut pairs: Vec<(u8, String)> = git_diff_records(&records)
            .filter_map(|r| match r {
                GitDiffRecord::Entry {
                    st: b'R',
                    similarity,
                    new_path,
                    ..
                } => Some((similarity, new_path.to_string())),
                _ => None,
            })
            .collect();
        pairs.sort();
        pairs
    };

    // 50%: both moves are renames — the edited one scores in the high 70s.
    let scored = renames(50);
    assert_eq!(scored.len(), 2, "at 50% both moves join: {scored:?}");
    // 100%: the edited file is no longer similar enough, the untouched one is.
    assert_eq!(
        renames(100),
        vec![(100u8, "moved.txt".to_string())],
        "100 is the strictest threshold, not an opt-out"
    );
    // 0: the exact-oid join only, which still finds the untouched move.
    assert_eq!(renames(0), vec![(100u8, "moved.txt".to_string())]);
}

/// Ignored files carry the `!` porcelain letter in STATUS; untracked `?`.
#[test]
fn status_marks_ignored_and_untracked() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&dir, &["add", ".gitignore"]);
    git(&dir, &["commit", "-m", "init"]);
    std::fs::write(dir.join("ignored.txt"), "x\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "y\n").unwrap();

    let letters = |ignored: bool| -> std::collections::HashMap<String, (u8, u8)> {
        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
        let sent2 = sent.clone();
        let opts = StateOptions {
            status: true,
            untracked: true,
            ignored,
            ..Default::default()
        };
        let state = handle.start_state(
            1,
            opts,
            Box::new(move |m| {
                sent2.lock().unwrap().push(m);
                true
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(45);
        let msg = loop {
            if let Some(m) = sent.lock().unwrap().first().cloned() {
                break m;
            }
            assert!(Instant::now() < deadline, "no snapshot");
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut mirror = GitStateMirror::new();
        mirror.apply_state(&msg).complete().unwrap();
        state.stop();
        mirror
            .status
            .into_iter()
            .map(|s| (s.path, (s.staged, s.unstaged)))
            .collect()
    };

    let with_ignored = letters(true);
    assert_eq!(with_ignored.get("ignored.txt"), Some(&(b'!', b'!')));
    assert_eq!(with_ignored.get("untracked.txt"), Some(&(b'?', b'?')));
    // Without the ignored flag, the ignored file is absent entirely.
    let without = letters(false);
    assert!(!without.contains_key("ignored.txt"));
    assert_eq!(without.get("untracked.txt"), Some(&(b'?', b'?')));
}

/// Deleting a tracked file then recreating it leaves a staged deletion ('D')
/// beside a new untracked file at the same path. The untracked marking must
/// not clobber the staged 'D' — git reports both, and so must the status.
#[test]
fn status_delete_then_recreate_keeps_staged_deletion() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "README.md"]);
    git(&dir, &["commit", "-m", "init"]);
    // Remove from the index only; the worktree file remains → untracked.
    git(&dir, &["rm", "--cached", "README.md"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        status: true,
        untracked: true,
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let msg = loop {
        if let Some(m) = sent.lock().unwrap().first().cloned() {
            break m;
        }
        assert!(Instant::now() < deadline, "no snapshot");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().unwrap();
    state.stop();

    let entry = mirror
        .status
        .iter()
        .find(|s| s.path == "README.md")
        .expect("README.md present in status");
    // Staged deletion preserved; worktree side marked untracked.
    assert_eq!(entry.staged, b'D', "staged deletion kept");
    assert_eq!(entry.unstaged, b'?', "worktree marked untracked");
}

/// GIT_DIFF sets the BINARY dflag for files containing NUL.
#[test]
fn diff_marks_binary() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("bin"), [0u8, 1, 2, 0, 3]).unwrap();
    std::fs::write(dir.join("text.txt"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::write(dir.join("bin"), [0u8, 9, 9, 0, 9]).unwrap();
    std::fs::write(dir.join("text.txt"), "world\n").unwrap();
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitDiffRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
        after: "",
    };
    let (_, _, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    let flags: std::collections::HashMap<String, u8> = git_diff_records(&records)
        .filter_map(|r| match r {
            GitDiffRecord::Entry {
                new_path, dflags, ..
            } => Some((new_path.to_string(), dflags)),
            _ => None,
        })
        .collect();
    assert_ne!(
        flags["bin"] & GIT_DIFF_ENTRY_BINARY,
        0,
        "binary file flagged"
    );
    assert_eq!(
        flags["text.txt"] & GIT_DIFF_ENTRY_BINARY,
        0,
        "text not flagged"
    );
}

/// A watched log emits an initial page and re-emits when its endpoint
/// ref moves (a new commit on HEAD).
#[test]
fn log_watch_updates_on_ref_move() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f"), "1\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c1"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    // Log-only engine (no GIT_STATE snapshots).
    let opts = StateOptions {
        wants_state: false,
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    state.watch_log(9, 0, 20, "HEAD".to_string());

    let wait_page = |after: usize| -> blit_remote::git::GitLogPage {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let msgs = sent.lock().unwrap().clone();
            if let Some(m) = msgs[after..].iter().find(|m| m[0] == S2C_GIT_LOG_PAGE) {
                return parse_git_log_page(m).expect("valid page");
            }
            assert!(Instant::now() < deadline, "log page never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    let first = wait_page(0);
    assert_eq!(first.status, GIT_STATUS_OK);
    let n1 = git_commit_records(&first.records)
        .filter(|r| matches!(r, GitCommitRecord::Commit { .. }))
        .count();
    assert_eq!(n1, 1);
    state.log_ack(9, first.update_id);
    let seen = sent.lock().unwrap().len();

    // Move HEAD; the watch must re-emit with the new commit.
    std::fs::write(dir.join("f"), "2\n").unwrap();
    git(&dir, &["commit", "-am", "c2"]);
    let second = wait_page(seen);
    let n2 = git_commit_records(&second.records)
        .filter(|r| matches!(r, GitCommitRecord::Commit { .. }))
        .count();
    assert_eq!(n2, 2, "watched log did not pick up the new commit");
    state.stop();
}

/// The per-repo log-subscription cap (docs/git.md limits table) refuses a
/// subscription past the limit with a BUDGET page rather than growing the
/// map unbounded on client-chosen ids.
#[test]
fn log_watch_subscription_cap() {
    // The default Budgets.max_log_subs (BLIT_GIT_MAX_LOG_SUBS), unset here.
    const CAP: u16 = 64;

    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f"), "1\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c1"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        wants_state: false,
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    // Fill to capacity, then one past it (log ids 1..=CAP+1).
    for log_id in 1..=CAP + 1 {
        state.watch_log(log_id, 0, 20, "HEAD".to_string());
    }

    // Find the first log page for a given id.
    let wait_for = |log_id: u16| -> blit_remote::git::GitLogPage {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let msgs = sent.lock().unwrap().clone();
            for m in &msgs {
                if m[0] == S2C_GIT_LOG_PAGE {
                    let page = parse_git_log_page(m).expect("valid page");
                    if page.log_id == log_id {
                        return page;
                    }
                }
            }
            assert!(Instant::now() < deadline, "page for {log_id} never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    };

    // The last in-cap subscription resolves normally.
    assert_eq!(wait_for(CAP).status, GIT_STATUS_OK);
    // The one past the cap is refused with BUDGET and an empty page.
    let over = wait_for(CAP + 1);
    assert_eq!(over.status, GIT_STATUS_BUDGET);
    assert!(over.records.is_empty());
    assert!(over.frontier.is_empty());
    state.stop();
}

/// GIT_RESOLVE turns ref names, short shas, HEAD~N, and ranges into
/// tips/hides commit oids.
#[test]
fn resolve_revspecs() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f"), "1\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c1"]);
    std::fs::write(dir.join("f"), "2\n").unwrap();
    git(&dir, &["commit", "-am", "c2"]);
    git(&dir, &["branch", "dev"]);
    std::fs::write(dir.join("f"), "3\n").unwrap();
    git(&dir, &["commit", "-am", "c3"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let c1 = rev(&dir, "HEAD~2");
    let head = rev(&dir, "HEAD");
    let dev = rev(&dir, "dev");

    let resolve = |spec: &str| {
        let (_, status, tips, hides) =
            parse_git_resolve_resp(&handle.resolve(1, spec, &cancel)).unwrap();
        (status, tips, hides)
    };
    // Single ref.
    assert_eq!(resolve("main"), (GIT_STATUS_OK, vec![head], vec![]));
    // Short sha (7 chars).
    let short: String = hex(&head, 40)[..7].to_string();
    assert_eq!(resolve(&short), (GIT_STATUS_OK, vec![head], vec![]));
    // Relative.
    assert_eq!(resolve("HEAD~2"), (GIT_STATUS_OK, vec![c1], vec![]));
    // Range: dev..HEAD → tips=[HEAD], hides=[dev].
    assert_eq!(resolve("dev..HEAD"), (GIT_STATUS_OK, vec![head], vec![dev]));
    // `^A` exclusion → tips=[], hides=[A].
    assert_eq!(resolve("^dev"), (GIT_STATUS_OK, vec![], vec![dev]));
    // `a^!` is the commit itself with its parents hidden: reachable set {a}.
    let c2 = rev(&dir, "HEAD~1");
    assert_eq!(resolve("HEAD^!"), (GIT_STATUS_OK, vec![head], vec![c2]));
    // A non-committish spec (a tree) is WRONG_TYPE.
    assert_eq!(resolve("HEAD^{tree}").0, GIT_STATUS_WRONG_TYPE);
    // Unknown ref.
    assert_eq!(resolve("nope").0, GIT_STATUS_NOT_FOUND);
    // Whitespace-separated tokens merge tips and hides like `git
    // rev-list` args — a base to multiple heads in one spec.
    assert_eq!(
        resolve("HEAD~2..main dev"),
        (GIT_STATUS_OK, vec![head, dev], vec![c1])
    );
    assert_eq!(
        resolve("main dev ^HEAD~2"),
        (GIT_STATUS_OK, vec![head, dev], vec![c1])
    );
    // One bad token fails the whole spec.
    assert_eq!(resolve("main nope").0, GIT_STATUS_NOT_FOUND);
}

fn hex(oid: &GitOid, len: usize) -> String {
    oid.iter().map(|b| format!("{b:02x}")).collect::<String>()[..len].to_string()
}

/// FOLLOW keeps history across an exact rename: commits from before the
/// file was renamed must still appear.
#[test]
fn log_follow_across_rename() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("old.txt"), "line\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "create old"]);
    git(&dir, &["mv", "old.txt", "new.txt"]);
    git(&dir, &["commit", "-m", "rename to new"]);
    std::fs::write(dir.join("new.txt"), "line\nmore\n").unwrap();
    git(&dir, &["commit", "-am", "edit new"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    // Without FOLLOW, only commits touching new.txt: edit + rename.
    let plain = GitLogRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        limit: 0,
        path: "new.txt",
        tips: vec![],
        hides: vec![],
    };
    let page = parse_git_commits(&handle.log(&plain, &cancel)).unwrap();
    let plain_msgs: Vec<String> = git_commit_records(&page.records)
        .filter_map(|r| match r {
            GitCommitRecord::Commit { message, .. } => Some(message.to_string()),
            _ => None,
        })
        .collect();
    assert!(!plain_msgs.iter().any(|m| m == "create old"));

    // With FOLLOW, the pre-rename creation appears too.
    let follow = GitLogRequest {
        nonce: 2,
        flags: GIT_LOG_FOLLOW,
        ..plain
    };
    let page = parse_git_commits(&handle.log(&follow, &cancel)).unwrap();
    assert_eq!(page.status, GIT_STATUS_OK);
    let msgs: Vec<String> = git_commit_records(&page.records)
        .filter_map(|r| match r {
            GitCommitRecord::Commit { message, .. } => Some(message.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        msgs.iter().any(|m| m == "create old"),
        "FOLLOW lost pre-rename history: {msgs:?}"
    );
}

/// Two changes within one context window must not duplicate rows
/// (structured) nor emit overlapping @@ hunks (TEXT — git apply rejects).
#[test]
fn patch_adjacent_hunks() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    // Lines 1..=8; change line 2 and line 5 — 2 unchanged lines apart, so
    // with context 3 the two hunks coalesce.
    std::fs::write(dir.join("f.txt"), "1\n2\n3\n4\n5\n6\n7\n8\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::write(dir.join("f.txt"), "1\nX\n3\n4\nY\n6\n7\n8\n").unwrap();
    git(&dir, &["add", "."]);
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let head = rev(&dir, "HEAD");
    let base = GitPatchRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        context: 3,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: head,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        path: "",
        max_len: 0,
        after: "",
        after_pos: 0,
    };
    // Structured: every old_line appears at most once across rows.
    let (_, _, _, data) = parse_git_patch_resp(&handle.patch(&base, &cancel)).unwrap();
    let mut old_lines_seen = Vec::new();
    for r in git_patch_records(&data) {
        if let GitPatchRecord::Row { old_line, .. } = r
            && old_line != 0
        {
            old_lines_seen.push(old_line);
        }
    }
    let mut sorted = old_lines_seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        old_lines_seen.len(),
        "duplicate old rows: {old_lines_seen:?}"
    );
    // Changed lines are present with spans.
    let changed: Vec<Vec<u8>> = git_patch_records(&data)
        .filter_map(|r| match r {
            GitPatchRecord::Row {
                new_text,
                new_spans,
                ..
            } if !new_spans.is_empty() => Some(new_text.to_vec()),
            _ => None,
        })
        .collect();
    assert!(changed.contains(&b"X".to_vec()) && changed.contains(&b"Y".to_vec()));

    // TEXT mode: hunks must be strictly non-overlapping and monotonic.
    let text_req = GitPatchRequest {
        nonce: 2,
        flags: GIT_PATCH_TEXT,
        ..base
    };
    let (_, _, _, data) = parse_git_patch_resp(&handle.patch(&text_req, &cancel)).unwrap();
    let text = String::from_utf8(data).unwrap();
    let mut last_end = 0usize;
    for line in text.lines().filter(|l| l.starts_with("@@")) {
        // @@ -old_start,old_count +new_start,new_count @@
        let old = line.split(' ').nth(1).unwrap().trim_start_matches('-');
        let mut it = old.split(',');
        let start: usize = it.next().unwrap().parse().unwrap();
        let count: usize = it.next().unwrap_or("1").parse().unwrap();
        assert!(start > last_end, "overlapping hunks: {text}");
        last_end = start + count;
    }
    assert!(
        text.contains("-2") && text.contains("+X") && text.contains("-5") && text.contains("+Y")
    );
}

/// A path-filtered GIT_LOG page holds up to `limit` MATCHING commits —
/// filtering runs during collection, not after — and its frontier
/// continues without loss or duplication.
#[test]
fn filtered_log_fills_page_with_matches() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a0\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b0\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    // 10 commits touching a.txt interleaved with 10 touching b.txt:
    // 11 commits match `a.txt` in total (10 + seed).
    for i in 1..=10 {
        std::fs::write(dir.join("b.txt"), format!("b{i}\n")).unwrap();
        git(&dir, &["commit", "-am", &format!("b{i}")]);
        std::fs::write(dir.join("a.txt"), format!("a{i}\n")).unwrap();
        git(&dir, &["commit", "-am", &format!("a{i}")]);
    }

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitLogRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        limit: 5,
        path: "a.txt",
        tips: vec![],
        hides: vec![],
    };
    let page = parse_git_commits(&handle.log(&req, &cancel)).unwrap();
    assert_eq!(page.status, GIT_STATUS_OK);
    let first: Vec<GitOid> = git_commit_records(&page.records)
        .filter_map(|r| match r {
            GitCommitRecord::Commit { oid, .. } => Some(oid),
            _ => None,
        })
        .collect();
    assert_eq!(first.len(), 5, "page must fill with MATCHING commits");
    assert_ne!(page.flags & GIT_COMMITS_MORE, 0);
    assert!(!page.frontier.is_empty());

    // Continuation from the frontier delivers the remaining 6 matches,
    // no duplicates, no loss.
    let req2 = GitLogRequest {
        nonce: 2,
        limit: 0,
        tips: page.frontier.clone(),
        ..req
    };
    let page2 = parse_git_commits(&handle.log(&req2, &cancel)).unwrap();
    assert_eq!(page2.status, GIT_STATUS_OK);
    let second: Vec<GitOid> = git_commit_records(&page2.records)
        .filter_map(|r| match r {
            GitCommitRecord::Commit { oid, .. } => Some(oid),
            _ => None,
        })
        .collect();
    let mut all: Vec<GitOid> = first.iter().chain(second.iter()).copied().collect();
    let total = all.len();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), total, "duplicate commits across pages");
    assert_eq!(total, 11, "every a.txt commit exactly once");
    assert_eq!(page2.flags & GIT_COMMITS_MORE, 0);
}

/// GIT_LOG FOLLOW on a directory is WRONG_TYPE; unknown flag bits INVALID.
#[test]
fn log_follow_directory_and_unknown_flags() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::create_dir(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/f.txt"), "x\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let follow_dir = GitLogRequest {
        nonce: 1,
        repo_id: 0,
        flags: GIT_LOG_FOLLOW,
        limit: 0,
        path: "sub",
        tips: vec![],
        hides: vec![],
    };
    let page = parse_git_commits(&handle.log(&follow_dir, &cancel)).unwrap();
    assert_eq!(page.status, GIT_STATUS_WRONG_TYPE);
    // An undefined flag bit is rejected.
    let bad_flags = GitLogRequest {
        nonce: 2,
        flags: 0x80,
        path: "",
        ..follow_dir
    };
    let page = parse_git_commits(&handle.log(&bad_flags, &cancel)).unwrap();
    assert_eq!(page.status, GIT_STATUS_INVALID);
    let follow_without_path = GitLogRequest {
        nonce: 3,
        repo_id: 0,
        flags: GIT_LOG_FOLLOW,
        limit: 0,
        path: "",
        tips: vec![],
        hides: vec![],
    };
    let page = parse_git_commits(&handle.log(&follow_without_path, &cancel)).unwrap();
    assert_eq!(page.status, GIT_STATUS_INVALID);
}

/// Parse "@@ -o,oc +n,nc @@" into (old_count, new_count); a missing count
/// defaults to 1 as in git's unified-diff format.
fn hunk_counts(header: &str) -> (usize, usize) {
    let mut parts = header.split_whitespace();
    parts.next(); // "@@"
    let old = parts.next().unwrap().trim_start_matches('-');
    let new = parts.next().unwrap().trim_start_matches('+');
    let oc = old.split(',').nth(1).unwrap_or("1").parse().unwrap();
    let nc = new.split(',').nth(1).unwrap_or("1").parse().unwrap();
    (oc, nc)
}

/// A real fork (two branches diverging past a shared base) exercises the
/// newest-common-ancestor selection that linear ancestry never does:
/// GIT_BASE, the MERGE_BASE diff endpoint, and `A...B` all report the true
/// base, which is neither input, and match the real CLI's merge-base.
#[test]
fn merge_base_and_symmetric_diff_on_fork() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("shared.txt"), "base\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "base"]);
    git(&dir, &["branch", "feature"]);
    // main advances two commits.
    std::fs::write(dir.join("shared.txt"), "main-1\n").unwrap();
    git(&dir, &["commit", "-am", "m1"]);
    std::fs::write(dir.join("shared.txt"), "main-2\n").unwrap();
    git(&dir, &["commit", "-am", "m2"]);
    // feature advances two commits on a different file.
    git(&dir, &["checkout", "feature"]);
    std::fs::write(dir.join("feat.txt"), "f1\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "f1"]);
    std::fs::write(dir.join("feat.txt"), "f2\n").unwrap();
    git(&dir, &["commit", "-am", "f2"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let main_tip = rev(&dir, "main");
    let feature_tip = rev(&dir, "feature");
    let base = rev(&dir, "feature~2"); // == main~2, the fork point
    assert_ne!(base, main_tip);
    assert_ne!(base, feature_tip);
    // Cross-check the fork point against the real CLI.
    let cli_base = rev(&dir, &git_out(&dir, &["merge-base", "main", "feature"]));
    assert_eq!(cli_base, base);

    // GIT_BASE returns the true fork point.
    let (_, status, bases) =
        parse_git_base_resp(&handle.base(1, &[main_tip, feature_tip], &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(bases, vec![base]);

    // MERGE_BASE diff endpoint reveals the same base.
    let req = GitDiffRequest {
        nonce: 2,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: main_tip,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: feature_tip,
        },
        path: "",
        after: "",
    };
    let (_, status, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert!(matches!(
        git_diff_records(&records).next().unwrap(),
        GitDiffRecord::Base { oid } if oid == base
    ));

    // `A...B` resolves to tips={A,B}, hides=[base].
    let (_, status, mut tips, hides) =
        parse_git_resolve_resp(&handle.resolve(3, "feature...main", &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    tips.sort();
    let mut want = vec![main_tip, feature_tip];
    want.sort();
    assert_eq!(tips, want);
    assert_eq!(hides, vec![base]);

    // MERGE_BASE is only valid on the old side; on the new side it is INVALID.
    let bad = GitDiffRequest {
        nonce: 4,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: main_tip,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: feature_tip,
        },
        path: "",
        after: "",
    };
    let (_, status, _, _) = parse_git_diff_resp(&handle.diff(&bad, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_INVALID);

    // Both operands peel, so an annotated tag names its commit; a blob is
    // WRONG_TYPE and an absent oid NOT_FOUND, not "backend error".
    git(&dir, &["tag", "-a", "v1", "-m", "v1", "main"]);
    let cases = [
        (rev(&dir, "v1"), GIT_STATUS_OK),
        (rev(&dir, "main:shared.txt"), GIT_STATUS_WRONG_TYPE),
        (GIT_OID_NONE, GIT_STATUS_NOT_FOUND),
    ];
    assert_ne!(cases[0].0, main_tip, "annotated tag has its own oid");
    for (nonce, (oid, want)) in cases.iter().enumerate() {
        let req = GitDiffRequest {
            nonce: 5 + nonce as u16,
            repo_id: 0,
            flags: 0,
            rename: 0,
            old: GitEndpoint {
                kind: GIT_ENDPOINT_MERGE_BASE,
                oid: *oid,
            },
            new: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: feature_tip,
            },
            path: "",
            after: "",
        };
        let (_, status, _, _) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
        assert_eq!(status, *want, "MERGE_BASE over {}", hex(oid, 8));
    }
}

/// The review view of a branch still being worked on: MERGE_BASE against
/// the index or the worktree, which carry no oid of their own, takes the
/// base against HEAD — so one request answers "everything since the fork,
/// committed or not". Oracle is git's own `--merge-base`.
#[test]
fn merge_base_pairs_with_index_and_worktree() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("shared.txt"), "base\n").unwrap();
    std::fs::write(dir.join("staged.txt"), "staged\n").unwrap();
    std::fs::write(dir.join("dirty.txt"), "dirty\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "base"]);
    git(&dir, &["branch", "feature"]);
    // main advances past the fork, so the base is neither tip.
    std::fs::write(dir.join("shared.txt"), "main-1\n").unwrap();
    git(&dir, &["commit", "-am", "m1"]);
    // feature commits, stages, edits and leaves a file untracked.
    git(&dir, &["checkout", "feature"]);
    std::fs::write(dir.join("committed.txt"), "c\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "f1"]);
    std::fs::write(dir.join("staged.txt"), "staged2\n").unwrap();
    git(&dir, &["add", "staged.txt"]);
    std::fs::write(dir.join("dirty.txt"), "dirty2\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "u\n").unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let main_tip = rev(&dir, "main");
    let base = rev(&dir, &git_out(&dir, &["merge-base", "main", "HEAD"]));
    assert_ne!(base, main_tip);
    assert_ne!(base, rev(&dir, "HEAD"));

    let merge_base = GitEndpoint {
        kind: GIT_ENDPOINT_MERGE_BASE,
        oid: main_tip,
    };
    let plain = |kind| GitEndpoint {
        kind,
        oid: GIT_OID_NONE,
    };
    // git's name-status, as `(st, path)` pairs, for an oracle comparison.
    let oracle = |args: &[&str]| -> Vec<(u8, String)> {
        git_out(&dir, args)
            .lines()
            .map(|line| {
                let (st, path) = line.split_once('\t').unwrap();
                (st.as_bytes()[0], path.to_string())
            })
            .collect()
    };
    let entries = |nonce: u16,
                   new: GitEndpoint,
                   flags: u8|
     -> (u8, Option<GitOid>, Vec<(u8, String)>) {
        let req = GitDiffRequest {
            nonce,
            repo_id: 0,
            flags,
            rename: 0,
            old: merge_base,
            new,
            path: "",
            after: "",
        };
        let (_, status, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
        let mut base_oid = None;
        let mut out = Vec::new();
        for record in git_diff_records(&records) {
            match record {
                GitDiffRecord::Base { oid } => base_oid = Some(oid),
                GitDiffRecord::Entry { st, new_path, .. } => out.push((st, new_path.to_string())),
                _ => {}
            }
        }
        out.sort();
        (status, base_oid, out)
    };

    // Worktree: the fork's whole diff, committed and uncommitted alike.
    let (status, base_oid, mut got) = entries(1, plain(GIT_ENDPOINT_WORKTREE), GIT_DIFF_UNTRACKED);
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(base_oid, Some(base), "BASE record still names the base");
    let untracked = (b'A', "untracked.txt".to_string());
    assert!(got.contains(&untracked), "untracked file missing: {got:?}");
    got.retain(|entry| *entry != untracked);
    let mut want = oracle(&["diff", "--name-status", "--merge-base", "main"]);
    want.sort();
    assert_eq!(got, want, "worktree side must match git --merge-base");
    assert!(
        want.contains(&(b'M', "dirty.txt".into()))
            && want.contains(&(b'A', "committed.txt".into())),
        "fixture must mix committed and uncommitted work: {want:?}"
    );

    // Index: the same base, staged content only — dirty.txt is unstaged, so
    // it must not appear.
    let (status, base_oid, got) = entries(2, plain(GIT_ENDPOINT_INDEX), 0);
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(base_oid, Some(base));
    let mut want = oracle(&["diff", "--name-status", "--cached", "--merge-base", "main"]);
    want.sort();
    assert_eq!(got, want, "index side must match git --merge-base --cached");
    assert!(!got.iter().any(|(_, path)| path == "dirty.txt"));

    // The kinds with no ancestry to fork from stay INVALID.
    for kind in [
        GIT_ENDPOINT_EMPTY,
        GIT_ENDPOINT_TREE,
        GIT_ENDPOINT_MERGE_BASE,
    ] {
        let (status, _, _) = entries(3, plain(kind), 0);
        assert_eq!(status, GIT_STATUS_INVALID, "kind {kind} must be INVALID");
    }

    // GIT_PATCH shares the resolver, and its text is git's.
    let req = GitPatchRequest {
        nonce: 4,
        repo_id: 0,
        flags: GIT_PATCH_TEXT,
        context: 3,
        rename: 0,
        old: merge_base,
        new: plain(GIT_ENDPOINT_WORKTREE),
        path: "dirty.txt",
        max_len: 0,
        after: "",
        after_pos: 0,
    };
    let (_, status, _, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let text = String::from_utf8(data).unwrap();
    assert!(
        text.contains("+dirty2") && text.contains("-dirty"),
        "patch must read the worktree side: {text}"
    );

    // An unborn HEAD names no commit to take the base against: NOT_FOUND,
    // not INVALID, so a client can degrade instead of blaming its request.
    git(&dir, &["checkout", "--orphan", "fresh"]);
    git(&dir, &["reset"]);
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let req = GitDiffRequest {
        nonce: 5,
        repo_id: 0,
        flags: 0,
        rename: 0,
        old: merge_base,
        new: plain(GIT_ENDPOINT_WORKTREE),
        path: "",
        after: "",
    };
    let (_, status, _, _) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_NOT_FOUND);
}

/// Criss-cross merges give two maximal merge bases: `A...B` must hide
/// both (matching `git merge-base --all`), and `GIT_BASE` returns one of
/// them as the best base.
#[test]
fn criss_cross_merge_bases() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "base\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    git(&dir, &["branch", "q"]);
    git(&dir, &["checkout", "-b", "p"]);
    std::fs::write(dir.join("p.txt"), "p\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "p1"]);
    let p1 = rev(&dir, "p");
    git(&dir, &["checkout", "q"]);
    std::fs::write(dir.join("q.txt"), "q\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "q1"]);
    let q1 = rev(&dir, "q");
    // p merges q1; q merges the ORIGINAL p1 — the criss-cross.
    git(&dir, &["checkout", "p"]);
    git(&dir, &["merge", &hex(&q1, 40), "-m", "pm"]);
    git(&dir, &["checkout", "q"]);
    git(&dir, &["merge", &hex(&p1, 40), "-m", "qm"]);
    let p_tip = rev(&dir, "p");
    let q_tip = rev(&dir, "q");

    // Oracle: git's own full base set.
    let mut cli_bases: Vec<GitOid> = git_out(&dir, &["merge-base", "--all", "p", "q"])
        .lines()
        .map(|line| rev(&dir, line))
        .collect();
    cli_bases.sort();
    let mut want = vec![p1, q1];
    want.sort();
    assert_eq!(cli_bases, want, "fixture is not criss-cross");

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    // `p...q` hides ALL bases.
    let (_, status, mut tips, mut hides) =
        parse_git_resolve_resp(&handle.resolve(1, "p...q", &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    tips.sort();
    hides.sort();
    let mut want_tips = vec![p_tip, q_tip];
    want_tips.sort();
    assert_eq!(tips, want_tips);
    assert_eq!(hides, want, "A...B must hide every merge base");
    // GIT_BASE returns one best base from the set.
    let (_, status, bases) =
        parse_git_base_resp(&handle.base(2, &[p_tip, q_tip], &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(bases.len(), 1);
    assert!(want.contains(&bases[0]), "best base not a merge base");
    // Memoized second resolution answers identically.
    let (_, status, _, mut hides2) =
        parse_git_resolve_resp(&handle.resolve(3, "p...q", &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    hides2.sort();
    assert_eq!(hides2, want);
}

/// Disjoint histories (orphan branch): `GIT_BASE` answers OK with zero
/// bases, `A...B` resolves with no hides, and a MERGE_BASE diff endpoint
/// reports `NO_MERGE_BASE` — a fact about the repository, not `INVALID`,
/// which would tell a correct client it built the request wrong.
#[test]
fn disjoint_histories_have_no_base() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    git(&dir, &["checkout", "--orphan", "other"]);
    std::fs::write(dir.join("o.txt"), "two\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "orphan"]);
    let main_tip = rev(&dir, "main");
    let other_tip = rev(&dir, "other");

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let (_, status, bases) =
        parse_git_base_resp(&handle.base(1, &[main_tip, other_tip], &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert!(bases.is_empty(), "disjoint histories have no base");
    let (_, status, mut tips, hides) =
        parse_git_resolve_resp(&handle.resolve(2, "main...other", &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    tips.sort();
    let mut want = vec![main_tip, other_tip];
    want.sort();
    assert_eq!(tips, want);
    assert!(hides.is_empty(), "nothing to hide without a base");

    // The same fact through a diff, on both an oid-bearing new side and the
    // worktree, and on GIT_PATCH — every one of them a distinct status a
    // client can render, not a bare "invalid request".
    for (nonce, new) in [
        (
            3,
            GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: other_tip,
            },
        ),
        (
            4,
            GitEndpoint {
                kind: GIT_ENDPOINT_WORKTREE,
                oid: GIT_OID_NONE,
            },
        ),
    ] {
        let req = GitDiffRequest {
            nonce,
            repo_id: 0,
            flags: 0,
            rename: 0,
            old: GitEndpoint {
                kind: GIT_ENDPOINT_MERGE_BASE,
                oid: main_tip,
            },
            new,
            path: "",
            after: "",
        };
        let (_, status, _, _) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
        assert_eq!(status, GIT_STATUS_NO_MERGE_BASE, "new kind {}", new.kind);
    }
    let req = GitPatchRequest {
        nonce: 5,
        repo_id: 0,
        flags: 0,
        context: 3,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: main_tip,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
        max_len: 0,
        after: "",
        after_pos: 0,
    };
    let (_, status, _, _) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_NO_MERGE_BASE);
    assert_eq!(git_status_text(GIT_STATUS_NO_MERGE_BASE), "no merge base");
}

/// The file-level diff status set matches `git diff --name-status` across a
/// mixed matrix (modify, delete, add, exact rename) rather than only
/// hand-enumerated literals.
#[test]
fn diff_status_matches_git() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a1\n").unwrap();
    std::fs::write(dir.join("b.txt"), "bbb\n").unwrap();
    std::fs::write(dir.join("c.txt"), "ccc\n").unwrap();
    std::fs::write(dir.join("d.txt"), "ddd\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c1"]);
    let a = rev(&dir, "HEAD");
    // Modify a, delete b, rename c->cc (exact), add e, leave d untouched.
    std::fs::write(dir.join("a.txt"), "a2\n").unwrap();
    git(&dir, &["rm", "b.txt"]);
    git(&dir, &["mv", "c.txt", "cc.txt"]);
    std::fs::write(dir.join("e.txt"), "eee\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c2"]);
    let b = rev(&dir, "HEAD");

    // Oracle: git's own name-status with rename detection.
    let (a_hex, b_hex) = (hex(&a, 40), hex(&b, 40));
    let raw = git_out(
        &dir,
        &["diff", "--name-status", "--find-renames", &a_hex, &b_hex],
    );
    let mut expected: Vec<String> = raw
        .lines()
        .map(|line| {
            let mut f = line.split('\t');
            let st = f.next().unwrap();
            if st.starts_with('R') {
                format!("R {} {}", f.next().unwrap(), f.next().unwrap())
            } else {
                format!("{} {}", &st[..1], f.next().unwrap())
            }
        })
        .collect();
    expected.sort();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitDiffRequest {
        nonce: 1,
        repo_id: 0,
        flags: GIT_DIFF_RENAMES,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: a,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: b,
        },
        path: "",
        after: "",
    };
    let (_, status, _, records) = parse_git_diff_resp(&handle.diff(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let mut got: Vec<String> = git_diff_records(&records)
        .filter_map(|r| match r {
            GitDiffRecord::Entry {
                st,
                old_path,
                new_path,
                ..
            } => Some(if st == b'R' {
                format!("R {old_path} {new_path}")
            } else {
                format!("{} {new_path}", st as char)
            }),
            _ => None,
        })
        .collect();
    got.sort();
    assert_eq!(got, expected);
}

/// TEXT-mode unified patches round-trip through `git apply`: a non-zero net
/// delta (pure insert/delete) applies cleanly and its new-side hunk header
/// reflects the delta, and a file without a trailing newline carries git's
/// "\ No newline at end of file" marker so it applies too.
#[test]
fn patch_text_round_trips_through_git_apply() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    let old_n = "1\n2\n3\n4\n5\n";
    let old_m = "a\nb"; // no trailing newline
    std::fs::write(dir.join("n.txt"), old_n).unwrap();
    std::fs::write(dir.join("m.txt"), old_m).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    // Delete a line and insert three (net +2); rewrite the no-newline file's
    // last line, keeping it newline-free.
    std::fs::write(dir.join("n.txt"), "1\n2\nX\nY\nZ\n4\n5\n").unwrap();
    std::fs::write(dir.join("m.txt"), "a\nc").unwrap();
    git(&dir, &["add", "."]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitPatchRequest {
        nonce: 1,
        repo_id: 0,
        flags: GIT_PATCH_TEXT,
        context: 3,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: rev(&dir, "HEAD"),
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        path: "",
        max_len: 0,
        after: "",
        after_pos: 0,
    };
    let (_, status, pflags, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(pflags & GIT_PATCH_STRUCTURED, 0);
    let text = String::from_utf8(data.clone()).unwrap();
    // The no-newline file carries git's marker on both sides of its change.
    assert!(
        text.matches("\\ No newline at end of file").count() >= 2,
        "missing no-newline markers: {text}"
    );
    // n.txt's hunk header reflects the +2 net delta (new_count != old_count).
    let lines: Vec<&str> = text.lines().collect();
    let n_hunk = lines
        .iter()
        .position(|l| *l == "+++ b/n.txt")
        .and_then(|i| lines[i + 1..].iter().find(|l| l.starts_with("@@")))
        .expect("n.txt hunk header");
    assert_eq!(hunk_counts(n_hunk), (5, 7), "header: {n_hunk}");

    // git apply --check accepts the patch against the pre-change content.
    let apply_dir = temp_dir();
    git(&apply_dir, &["init"]);
    std::fs::write(apply_dir.join("n.txt"), old_n).unwrap();
    std::fs::write(apply_dir.join("m.txt"), old_m).unwrap();
    std::fs::write(apply_dir.join("patch.diff"), &data).unwrap();
    git(&apply_dir, &["apply", "--check", "patch.diff"]);
}

/// HEAD state records for the non-symbolic cases: a detached HEAD carries
/// the flag, the checked-out oid, and an empty name; an unborn branch
/// carries the flag, a zero oid, a symbolic name, and an empty-OK log.
#[test]
fn detached_and_unborn_head() {
    // Detached HEAD.
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f"), "1\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c1"]);
    std::fs::write(dir.join("f"), "2\n").unwrap();
    git(&dir, &["commit", "-am", "c2"]);
    let c1 = rev(&dir, "HEAD~1");
    git(&dir, &["checkout", &hex(&c1, 40)]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let msg = wait_first_state(
        &handle,
        StateOptions {
            status: true,
            ..Default::default()
        },
    );
    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().expect("valid state");
    let head = mirror.head.as_ref().expect("head record");
    assert_ne!(head.flags & GIT_HEAD_DETACHED, 0, "detached flag set");
    assert_eq!(
        head.oid, c1,
        "detached HEAD points at the checked-out commit"
    );
    assert!(head.name.is_empty(), "detached HEAD has no symbolic name");

    // Unborn branch: a fresh repo with no commits.
    let empty = temp_dir();
    git(&empty, &["init", "-b", "main"]);
    let (handle, _info) = open(empty.to_str().unwrap()).unwrap();
    let msg = wait_first_state(
        &handle,
        StateOptions {
            status: true,
            ..Default::default()
        },
    );
    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().expect("valid state");
    let head = mirror.head.as_ref().expect("head record");
    assert_ne!(head.flags & GIT_HEAD_UNBORN, 0, "unborn flag set");
    assert_eq!(head.oid, GIT_OID_NONE, "unborn HEAD has a zero oid");
    assert_eq!(
        head.name, "refs/heads/main",
        "unborn HEAD keeps its symbolic name"
    );
    // The log on an unborn branch is an empty OK page, not an error.
    let cancel = Cancel::default();
    let req = GitLogRequest {
        nonce: 1,
        repo_id: 0,
        flags: 0,
        limit: 0,
        path: "",
        tips: vec![],
        hides: vec![],
    };
    let page = parse_git_commits(&handle.log(&req, &cancel)).unwrap();
    assert_eq!(page.status, GIT_STATUS_OK);
    assert_eq!(
        git_commit_records(&page.records)
            .filter(|r| matches!(r, GitCommitRecord::Commit { .. }))
            .count(),
        0
    );
}

/// Editing a tracked file and then rewriting the *committed* content back
/// (a fresh mtime, identical bytes) must return the file to clean — the
/// worktree re-hash proves content equality despite the stat mismatch. This
/// pins the reported "save-back-to-HEAD still shows M" behaviour end-to-end,
/// through the watch-driven live snapshots.
#[test]
fn status_clears_when_file_reverted_to_committed() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    let committed = "alpha\nbeta\ngamma\n";
    std::fs::write(dir.join("a.txt"), committed).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        status: true,
        status_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |msg| {
            sent2.lock().unwrap().push(msg);
            true
        }),
    );

    let mut mirror = GitStateMirror::new();
    let mut applied = 0usize;
    // Apply snapshots (acking each) until `pred` holds, or time out.
    let mut wait_until =
        |mirror: &mut GitStateMirror, label: &str, pred: &dyn Fn(&GitStateMirror) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                let next = sent.lock().unwrap().get(applied).cloned();
                if let Some(msg) = next {
                    applied += 1;
                    let id = mirror.apply_state(&msg).complete().expect("valid state");
                    // Ack every snapshot (including the matching one) so the
                    // engine's coalescing keeps delivering the next.
                    state.ack(id);
                    if pred(mirror) {
                        return;
                    }
                } else {
                    assert!(
                        Instant::now() < deadline,
                        "{label}: never satisfied; status={:?}",
                        mirror.status
                    );
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        };

    // Starts clean.
    wait_until(&mut mirror, "initial-clean", &|m| {
        m.status.iter().all(|s| s.path != "a.txt")
    });

    // Edit -> the file is modified in the worktree.
    std::fs::write(dir.join("a.txt"), "alpha\nBETA\ngamma\n").unwrap();
    wait_until(&mut mirror, "modified", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "a.txt" && s.unstaged == b'M')
    });

    // Rewrite the exact committed bytes back (fresh mtime, same content).
    std::fs::write(dir.join("a.txt"), committed).unwrap();
    wait_until(&mut mirror, "reverted-clean", &|m| {
        m.status.iter().all(|s| s.path != "a.txt")
    });

    state.stop();
}

/// A pure ref settle (branch created) reuses the previous status records
/// instead of re-running the pipeline — the snapshot must still carry the
/// correct status — while an index change arriving on the ref side (a
/// `git add` touches only `.git/index`) is fingerprinted and recomputes.
#[test]
fn ref_settle_reuses_status_and_index_change_recomputes() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        status: true,
        untracked: true,
        refs_latency: Duration::from_millis(20),
        status_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    let mut mirror = GitStateMirror::new();
    let mut applied = 0usize;
    let mut wait_until =
        |mirror: &mut GitStateMirror, label: &str, pred: &dyn Fn(&GitStateMirror) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                let next = sent.lock().unwrap().get(applied).cloned();
                if let Some(msg) = next {
                    applied += 1;
                    let id = mirror.apply_state(&msg).complete().expect("valid state");
                    state.ack(id);
                    if pred(mirror) {
                        return;
                    }
                } else {
                    assert!(
                        Instant::now() < deadline,
                        "{label}: never satisfied; status={:?} refs={:?}",
                        mirror.status,
                        mirror.refs.keys().collect::<Vec<_>>()
                    );
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        };

    wait_until(&mut mirror, "initial-clean", &|m| {
        m.head.is_some() && m.status.iter().all(|s| s.path != "f.txt")
    });

    // Worktree edit → unstaged M.
    std::fs::write(dir.join("f.txt"), "two\n").unwrap();
    wait_until(&mut mirror, "unstaged-m", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "f.txt" && s.unstaged == b'M' && s.staged == b' ')
    });

    // Pure ref change: the snapshot carries the new ref AND the reused
    // status records, still showing the dirty file.
    git(&dir, &["branch", "side"]);
    wait_until(&mut mirror, "ref-with-reused-status", &|m| {
        m.refs.contains_key("refs/heads/side")
    });
    assert!(
        mirror
            .status
            .iter()
            .any(|s| s.path == "f.txt" && s.unstaged == b'M'),
        "status lost across a pure ref settle: {:?}",
        mirror.status
    );

    // `git add` only rewrites .git/index (a ref-side event); the index
    // fingerprint must force a status recompute → staged M.
    git(&dir, &["add", "f.txt"]);
    wait_until(&mut mirror, "staged-m", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "f.txt" && s.staged == b'M' && s.unstaged == b' ')
    });

    state.stop();
}

/// A settled change that produces a byte-identical snapshot (gitdir noise
/// with no state meaning) is suppressed: nothing is sent and no state_id
/// is burned — the next real change uses the next consecutive id.
#[test]
fn identical_snapshot_suppressed() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let first = loop {
        if let Some(m) = sent.lock().unwrap().first().cloned() {
            break m;
        }
        assert!(Instant::now() < deadline, "no first snapshot");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut mirror = GitStateMirror::new();
    let first_id = mirror.apply_state(&first).complete().expect("valid state");
    state.ack(first_id);

    // Gitdir noise: a file the state records never read. The settle fires,
    // the snapshot recomputes byte-identical, and nothing is sent.
    std::fs::write(dir.join(".git").join("BLIT_NOISE"), "x").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "identical snapshot was re-sent"
    );

    // A real ref change delivers, with the very next state_id — the
    // suppressed snapshot burned none.
    git(&dir, &["branch", "side"]);
    let deadline = Instant::now() + Duration::from_secs(45);
    let second = loop {
        if let Some(m) = sent.lock().unwrap().get(1).cloned() {
            break m;
        }
        assert!(Instant::now() < deadline, "no snapshot after ref change");
        std::thread::sleep(Duration::from_millis(10));
    };
    let second_id = mirror.apply_state(&second).complete().expect("valid state");
    assert!(mirror.refs.contains_key("refs/heads/side"));
    assert_eq!(second_id, first_id.wrapping_add(1), "state_id was burned");
    state.stop();
}

/// Messages a test sink captured, in send order.
type Sent = Arc<Mutex<Vec<Vec<u8>>>>;
/// The engine-facing half of a test sink (a `blit_git::Outbox`).
type SinkOutbox = Box<dyn FnMut(Vec<u8>) -> bool + Send>;

/// A `(sent messages, outbox)` pair for driving a state engine.
fn sink() -> (Sent, SinkOutbox) {
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    (
        sent,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    )
}

/// Block until `sent` holds a message at `index`, then return it.
fn wait_msg(sent: &Arc<Mutex<Vec<Vec<u8>>>>, index: usize, label: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(m) = sent.lock().unwrap().get(index).cloned() {
            return m;
        }
        assert!(Instant::now() < deadline, "{label}: message never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Two opens of one repository attach to ONE shared engine — the registry
/// refcount reads 2, not two engines — with per-open state: each open's
/// snapshots carry its own repo_id. Teardown is refcounted: dropping one
/// handle keeps the engine (and the other subscriber) live; dropping the
/// last removes the registry entry and stops the engine with its watchers
/// (docs/design/git.md).
#[test]
fn opens_share_one_engine_with_refcounted_teardown() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    let gitdir = dir.join(".git");
    assert_eq!(blit_git::debug_engine_refs(&gitdir), None, "no engine yet");

    // Two separate opens (exercising the repo registry too).
    let (h1, _info) = open(dir.to_str().unwrap()).unwrap();
    let (h2, _info) = open(dir.to_str().unwrap()).unwrap();
    let (sent_a, out_a) = sink();
    let (sent_b, out_b) = sink();
    let opts = StateOptions {
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state_a = h1.start_state(1, opts.clone(), out_a);
    let state_b = h2.start_state(2, opts, out_b);
    assert_eq!(
        blit_git::debug_engine_refs(&gitdir),
        Some(2),
        "both opens share one engine"
    );

    // Each subscriber's first snapshot carries its own repo_id.
    let first_a = wait_msg(&sent_a, 0, "first snapshot A");
    let (repo_id_a, ..) = parse_git_state(&first_a).expect("valid state");
    assert_eq!(repo_id_a, 1);
    let first_b = wait_msg(&sent_b, 0, "first snapshot B");
    let (repo_id_b, ..) = parse_git_state(&first_b).expect("valid state");
    assert_eq!(repo_id_b, 2);

    // Dropping one handle keeps the engine serving the other.
    drop(state_a);
    assert_eq!(
        blit_git::debug_engine_refs(&gitdir),
        Some(1),
        "engine survives the first detach"
    );
    let mut mirror = GitStateMirror::new();
    let id = mirror
        .apply_state(&first_b)
        .complete()
        .expect("valid state");
    state_b.ack(id);
    git(&dir, &["branch", "side"]);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut applied = 1usize;
    loop {
        let next = sent_b.lock().unwrap().get(applied).cloned();
        if let Some(msg) = next {
            applied += 1;
            let id = mirror.apply_state(&msg).complete().expect("valid state");
            state_b.ack(id);
            if mirror.refs.contains_key("refs/heads/side") {
                break;
            }
        } else {
            assert!(
                Instant::now() < deadline,
                "remaining subscriber stopped receiving updates"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    // Last handle out: registry slot gone, engine stopped.
    drop(state_b);
    assert_eq!(
        blit_git::debug_engine_refs(&gitdir),
        None,
        "last detach tears the engine down"
    );
}

/// Per-subscriber flag filtering over one shared computation
/// (docs/design/git.md): the engine cuts status once at the superset
/// demand, and each open sees only the letters its flags admit — '?'
/// needs UNTRACKED, '!' needs IGNORED, and a no-STATUS open sees no
/// status records at all.
#[test]
fn subscribers_filter_by_flags_and_share_computation() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
    std::fs::write(dir.join("t.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::write(dir.join("t.txt"), "two\n").unwrap();
    std::fs::write(dir.join("ignored.txt"), "x\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "y\n").unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let gitdir = dir.join(".git");
    let recomputes_before = blit_git::debug_status_recomputes(&gitdir);
    let sub = |status, untracked, ignored| StateOptions {
        status,
        untracked,
        ignored,
        ..Default::default()
    };
    let (sent_a, out_a) = sink();
    let (sent_b, out_b) = sink();
    let (sent_c, out_c) = sink();
    // A carries the superset; B and C filter down from A's computation.
    let state_a = handle.start_state(1, sub(true, true, true), out_a);
    let state_b = handle.start_state(2, sub(true, false, false), out_b);
    let state_c = handle.start_state(3, sub(false, false, false), out_c);

    let letters = |msg: &[u8]| -> std::collections::HashMap<String, (u8, u8)> {
        let mut mirror = GitStateMirror::new();
        mirror.apply_state(msg).complete().expect("valid state");
        mirror
            .status
            .into_iter()
            .map(|s| (s.path, (s.staged, s.unstaged)))
            .collect()
    };
    let a = letters(&wait_msg(&sent_a, 0, "first snapshot A"));
    assert_eq!(a.get("ignored.txt"), Some(&(b'!', b'!')));
    assert_eq!(a.get("untracked.txt"), Some(&(b'?', b'?')));
    assert_eq!(a.get("t.txt"), Some(&(b' ', b'M')));
    let b = letters(&wait_msg(&sent_b, 0, "first snapshot B"));
    assert!(
        !b.contains_key("ignored.txt"),
        "'!' filtered without IGNORED"
    );
    assert!(
        !b.contains_key("untracked.txt"),
        "'?' filtered without UNTRACKED"
    );
    assert_eq!(b.get("t.txt"), Some(&(b' ', b'M')), "tracked change kept");
    let c = letters(&wait_msg(&sent_c, 0, "first snapshot C"));
    assert!(c.is_empty(), "no STATUS records without the STATUS flag");

    // One shared status computation served all three subscribers.
    assert_eq!(
        blit_git::debug_status_recomputes(&gitdir),
        recomputes_before + 1,
        "status pipeline must run once, not per subscriber"
    );
    drop((state_a, state_b, state_c));
}

/// The shared engine runs at the MINIMUM settle window across
/// subscribers (docs/design/git.md): a slow-window open still receives a
/// ref change promptly once a fast-window open shares its engine.
#[test]
fn engine_runs_at_minimum_settle_window() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let (sent_slow, out_slow) = sink();
    let (sent_fast, out_fast) = sink();
    // Far beyond the test deadline: if the engine settled at the slow
    // window, the update below could never arrive in time.
    let slow = handle.start_state(
        1,
        StateOptions {
            refs_latency: Duration::from_secs(30),
            ..Default::default()
        },
        out_slow,
    );
    let fast = handle.start_state(
        2,
        StateOptions {
            refs_latency: Duration::from_millis(20),
            ..Default::default()
        },
        out_fast,
    );
    let mut mirror = GitStateMirror::new();
    let id = mirror
        .apply_state(&wait_msg(&sent_slow, 0, "first snapshot slow"))
        .complete()
        .expect("valid state");
    slow.ack(id);
    let _ = wait_msg(&sent_fast, 0, "first snapshot fast");

    git(&dir, &["branch", "side"]);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut applied = 1usize;
    loop {
        let next = sent_slow.lock().unwrap().get(applied).cloned();
        if let Some(msg) = next {
            applied += 1;
            let id = mirror.apply_state(&msg).complete().expect("valid state");
            slow.ack(id);
            if mirror.refs.contains_key("refs/heads/side") {
                break;
            }
        } else {
            assert!(
                Instant::now() < deadline,
                "engine did not run at the minimum settle window"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    drop((slow, fast));
}

/// Build-artifact churn under an ignored directory must not dirty the
/// status side: worktree watch events filter through the repo's ignore
/// rules on the engine thread (docs/design/git.md). Editing `.gitignore`
/// itself both rebuilds the exclude stack and recomputes status,
/// re-enabling events under the previously ignored directory; a
/// tracked-file event always dirties.
#[test]
fn ignored_churn_skips_status_recompute() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let gitdir = dir.join(".git");
    let (sent, out) = sink();
    let opts = StateOptions {
        status: true,
        untracked: true,
        refs_latency: Duration::from_millis(20),
        status_latency: Duration::from_millis(30),
        ..Default::default()
    };
    let state = handle.start_state(1, opts, out);
    let mut mirror = GitStateMirror::new();
    let mut applied = 0usize;
    let mut wait_until =
        |mirror: &mut GitStateMirror, label: &str, pred: &dyn Fn(&GitStateMirror) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                let next = sent.lock().unwrap().get(applied).cloned();
                if let Some(msg) = next {
                    applied += 1;
                    let id = mirror.apply_state(&msg).complete().expect("valid state");
                    state.ack(id);
                    if pred(mirror) {
                        return;
                    }
                } else {
                    assert!(
                        Instant::now() < deadline,
                        "{label}: never satisfied; status={:?}",
                        mirror.status
                    );
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        };
    wait_until(&mut mirror, "initial", &|m| m.head.is_some());
    let base = blit_git::debug_status_recomputes(&gitdir);
    assert!(base >= 1, "first snapshot ran the pipeline");

    // Churn under the ignored directory: filtered before it ever arms the
    // status settle — no recompute, not merely a suppressed send.
    std::fs::create_dir(dir.join("target")).unwrap();
    std::fs::write(dir.join("target").join("a.o"), "obj\n").unwrap();
    std::fs::write(dir.join("target").join("a.o"), "obj2\n").unwrap();
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(
        blit_git::debug_status_recomputes(&gitdir),
        base,
        "ignored churn must not recompute status"
    );

    // Dropping the rule is itself a status change — the `.gitignore`
    // event invalidates the stack AND recomputes, surfacing target/a.o.
    std::fs::write(dir.join(".gitignore"), "").unwrap();
    wait_until(&mut mirror, "gitignore-edit", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "target/a.o" && s.unstaged == b'?')
    });
    assert!(
        blit_git::debug_status_recomputes(&gitdir) > base,
        ".gitignore edit must recompute"
    );

    // Events under the formerly ignored directory now dirty status.
    std::fs::write(dir.join("target").join("b.o"), "x\n").unwrap();
    wait_until(&mut mirror, "unignored-churn", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "target/b.o" && s.unstaged == b'?')
    });

    // A tracked-file event always dirties.
    std::fs::write(dir.join("f.txt"), "two\n").unwrap();
    wait_until(&mut mirror, "tracked-edit", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "f.txt" && s.unstaged == b'M')
    });
    state.stop();
}

/// An open with IGNORED surfaces ignored files in its status, so the
/// event filter must not swallow churn under ignored directories for
/// that engine (docs/design/git.md).
#[test]
fn ignored_flag_bypasses_event_filter() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::create_dir(dir.join("target")).unwrap();
    std::fs::write(dir.join("target").join("c0.o"), "obj\n").unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let (sent, out) = sink();
    let opts = StateOptions {
        status: true,
        untracked: true,
        ignored: true,
        refs_latency: Duration::from_millis(20),
        status_latency: Duration::from_millis(30),
        ..Default::default()
    };
    let state = handle.start_state(1, opts, out);
    let mut mirror = GitStateMirror::new();
    let mut applied = 0usize;
    let mut wait_until =
        |mirror: &mut GitStateMirror, label: &str, pred: &dyn Fn(&GitStateMirror) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                let next = sent.lock().unwrap().get(applied).cloned();
                if let Some(msg) = next {
                    applied += 1;
                    let id = mirror.apply_state(&msg).complete().expect("valid state");
                    state.ack(id);
                    if pred(mirror) {
                        return;
                    }
                } else {
                    assert!(
                        Instant::now() < deadline,
                        "{label}: never satisfied; status={:?}",
                        mirror.status
                    );
                    std::thread::sleep(Duration::from_millis(15));
                }
            }
        };
    // The pre-existing ignored file arrives as '!'.
    wait_until(&mut mirror, "initial-ignored", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "target/c0.o" && s.unstaged == b'!')
    });
    // New churn under the ignored dir is a REAL update for this open: the
    // filter is bypassed and the snapshot surfaces the new file.
    std::fs::write(dir.join("target").join("c1.o"), "obj\n").unwrap();
    wait_until(&mut mirror, "ignored-churn-surfaces", &|m| {
        m.status
            .iter()
            .any(|s| s.path == "target/c1.o" && s.unstaged == b'!')
    });
    state.stop();
}

/// The worktree watch is armed per directory, and a directory the exclude
/// stack marks ignored gets none: nothing beneath it can affect `git
/// status`, because git's own rule is that no negation re-includes a path
/// under an excluded directory. So `!blitprune-target/keep` does NOT save
/// the subtree — but `!blitprune-build/`, which un-ignores the directory
/// itself, does keep it watched. The gitdir subtree is never pruned: ref
/// moves arrive through the worktree watch.
///
/// The directory names carry a `blitprune` prefix so a rule in the test
/// host's own global ignore file cannot decide the outcome.
#[test]
fn worktree_watch_prunes_ignored_dirs() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(
        dir.join(".gitignore"),
        "blitprune-target/\n!blitprune-target/keep\nblitprune-build*\n!blitprune-build/\n",
    )
    .unwrap();
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::create_dir_all(dir.join("blitprune-target/keep")).unwrap();
    std::fs::create_dir_all(dir.join("blitprune-build")).unwrap();
    std::fs::create_dir_all(dir.join("src/nested")).unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let gitdir = dir.join(".git");
    let (_sent, out) = sink();
    let state = handle.start_state(
        1,
        StateOptions {
            status: true,
            ..Default::default()
        },
        out,
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let armed = loop {
        if let Some(dirs) = blit_git::debug_worktree_watches(&gitdir) {
            break dirs;
        }
        assert!(Instant::now() < deadline, "watch set never published");
        std::thread::sleep(Duration::from_millis(10));
    };
    let has = |suffix: &str| armed.iter().any(|d| *d == dir.join(suffix));
    assert!(has(""), "the root is armed");
    assert!(has("src") && has("src/nested"));
    assert!(has(".git"), "the gitdir subtree is never pruned");
    assert!(
        !has("blitprune-target") && !has("blitprune-target/keep"),
        "an ignored subtree gets no watch; its negation cannot re-include: {armed:?}"
    );
    assert!(
        has("blitprune-build"),
        "a negation matching the directory itself keeps it watched: {armed:?}"
    );
    state.stop();
}

/// A `.gitignore` edit that newly ignores a directory retires its watch;
/// emptying the file re-arms it — the watch set reconciles against the
/// new rules on the same settle that recomputes status.
#[test]
fn worktree_watch_follows_ignore_edits() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitignore"), "\n").unwrap();
    std::fs::write(dir.join("f.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::create_dir_all(dir.join("blitprune-gen")).unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let gitdir = dir.join(".git");
    let (_sent, out) = sink();
    let state = handle.start_state(
        1,
        StateOptions {
            status: true,
            ..Default::default()
        },
        out,
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let wait_set = |label: &str, pred: &dyn Fn(&[PathBuf]) -> bool| {
        loop {
            if let Some(dirs) = blit_git::debug_worktree_watches(&gitdir)
                && pred(&dirs)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{label}: watch set never converged"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let has_gen = |dirs: &[PathBuf]| dirs.iter().any(|d| *d == dir.join("blitprune-gen"));
    wait_set("initial", &|dirs| has_gen(dirs));
    std::fs::write(dir.join(".gitignore"), "blitprune-gen/\n").unwrap();
    wait_set("newly-ignored", &|dirs| !has_gen(dirs));
    std::fs::write(dir.join(".gitignore"), "\n").unwrap();
    wait_set("unignored", &|dirs| has_gen(dirs));
    state.stop();
}

/// The status view has to notice a rule change wherever the rule lives.
///
/// An in-tree `.gitignore` rides the worktree watch, but git reads two
/// ignore sources that are not in the tree, and neither raised an event
/// (#256): the user's global ignore file, and `$GIT_DIR/info/exclude` for
/// a repo whose gitdir is elsewhere (`--separate-git-dir`, a submodule) —
/// there the worktree watch covers no `.git`, and the targeted gitdir
/// watches did not include `info/`. Both left the Changes view showing
/// files git had just started ignoring, with nothing to correct it until
/// an unrelated worktree write happened by.
///
/// The probe extension is deliberately outlandish so a rule in the *test
/// host's* own global ignore file cannot decide the outcome.
#[test]
fn out_of_tree_ignore_sources_reach_the_status_view() {
    for source in ["global", "info-exclude"] {
        let base = temp_dir();
        let dir = base.join("work");
        let gitdir = base.join("gitdir");
        std::fs::create_dir_all(&dir).unwrap();
        let global = base.join("globalignore");
        match source {
            "global" => {
                git(&dir, &["init", "-b", "main"]);
                std::fs::write(&global, "# nothing yet\n").unwrap();
                git(
                    &dir,
                    &["config", "core.excludesFile", global.to_str().unwrap()],
                );
            }
            _ => git(
                &dir,
                &[
                    "init",
                    "-b",
                    "main",
                    "--separate-git-dir",
                    gitdir.to_str().unwrap(),
                ],
            ),
        }
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "seed"]);
        std::fs::write(dir.join("noise.blitprobe"), "x\n").unwrap();

        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let (sent, out) = sink();
        let opts = StateOptions {
            status: true,
            untracked: true,
            refs_latency: Duration::from_millis(20),
            status_latency: Duration::from_millis(30),
            ..Default::default()
        };
        let state = handle.start_state(1, opts, out);
        let mut mirror = GitStateMirror::new();
        let mut applied = 0usize;
        let mut wait_until =
            |mirror: &mut GitStateMirror, label: &str, pred: &dyn Fn(&GitStateMirror) -> bool| {
                let deadline = Instant::now() + Duration::from_secs(45);
                loop {
                    let next = sent.lock().unwrap().get(applied).cloned();
                    if let Some(msg) = next {
                        applied += 1;
                        let id = mirror.apply_state(&msg).complete().expect("valid state");
                        state.ack(id);
                        if pred(mirror) {
                            return;
                        }
                    } else {
                        assert!(
                            Instant::now() < deadline,
                            "{source}/{label}: never satisfied; status={:?}",
                            mirror.status
                        );
                        std::thread::sleep(Duration::from_millis(15));
                    }
                }
            };
        wait_until(&mut mirror, "untracked-appears", &|m| {
            m.status
                .iter()
                .any(|s| s.path == "noise.blitprobe" && s.unstaged == b'?')
        });

        // The edit git would honor on its next status — and so must we, on
        // this settle rather than on whatever touches the tree next.
        match source {
            "global" => std::fs::write(&global, "*.blitprobe\n").unwrap(),
            _ => {
                std::fs::create_dir_all(gitdir.join("info")).unwrap();
                std::fs::write(gitdir.join("info").join("exclude"), "*.blitprobe\n").unwrap();
            }
        }
        wait_until(&mut mirror, "rule-retires-it", &|m| {
            !m.status.iter().any(|s| s.path == "noise.blitprobe")
        });
        state.stop();
    }
}

/// `GIT_PATCH_TEXT` is git's patch format, not a subset of it — checked
/// against the real `git diff` rather than against our own intent
/// (docs/design/git.md "GIT_PATCH_TEXT output"). Covers add, delete,
/// modify, rename, mode change, binary, and a file with no trailing
/// newline in one diff.
///
/// Three documented deviations are normalized away: full-length index
/// oids instead of `core.abbrev` abbreviations, our own similarity
/// percentage, and no `GIT binary patch` payload.
#[test]
fn text_patch_matches_git_diff() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("keep.txt"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(dir.join("gone.txt"), "removed\n").unwrap();
    std::fs::write(dir.join("moved.txt"), "l1\nl2\nl3\nl4\nl5\nl6\n").unwrap();
    std::fs::write(dir.join("chmod.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 0, 3]).unwrap();
    std::fs::write(dir.join("nonl.txt"), "no trailing newline").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);

    // add, delete, modify, rename-with-edit, mode change, binary change,
    // and an edit to the file lacking a trailing newline.
    std::fs::write(dir.join("added.txt"), "brand new\n").unwrap();
    std::fs::remove_file(dir.join("gone.txt")).unwrap();
    std::fs::write(dir.join("keep.txt"), "one\nTWO\nthree\n").unwrap();
    std::fs::rename(dir.join("moved.txt"), dir.join("renamed.txt")).unwrap();
    std::fs::write(dir.join("renamed.txt"), "l1\nl2\nl3\nl4\nl5\nL6\n").unwrap();
    std::fs::write(dir.join("blob.bin"), [0u8, 9, 9, 0, 7]).unwrap();
    std::fs::write(dir.join("nonl.txt"), "still no trailing newline").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("chmod.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-m", "second"]);

    let ours = {
        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let cancel = Cancel::default();
        let req = GitPatchRequest {
            nonce: 1,
            repo_id: 0,
            flags: GIT_PATCH_TEXT | GIT_PATCH_RENAMES,
            context: 3,
            rename: 50,
            old: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&dir, "HEAD~1"),
            },
            new: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&dir, "HEAD"),
            },
            path: "",
            max_len: 0,
            after: "",
            after_pos: 0,
        };
        let (_, status, flags, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        assert_eq!(flags & GIT_PATCH_STRUCTURED, 0, "text mode");
        String::from_utf8(data).expect("patch text is UTF-8")
    };
    let theirs = git_out(
        &dir,
        &["diff", "-M50%", "--no-color", "-U3", "HEAD~1", "HEAD"],
    );

    /// Collapse the documented deviations so the comparison is about the
    /// header set, not about oid abbreviation length or whose similarity
    /// scorer ran.
    fn normalize(patch: &str) -> Vec<String> {
        patch
            .lines()
            .map(|line| {
                if let Some(rest) = line.strip_prefix("index ") {
                    // "index <old>..<new>[ mode]" — keep the mode suffix.
                    let mode = rest.split_once(' ').map(|(_, m)| m).unwrap_or("");
                    return format!("index <oids> {mode}").trim_end().to_string();
                }
                if line.starts_with("similarity index ") {
                    return "similarity index <n>%".to_string();
                }
                line.to_string()
            })
            .collect()
    }

    let (ours_n, theirs_n) = (normalize(&ours), normalize(&theirs));
    assert_eq!(
        ours_n, theirs_n,
        "\n--- ours ---\n{ours}\n--- git ---\n{theirs}\n"
    );

    // The header lines that used to be missing entirely, spelled out so a
    // regression names itself rather than showing a diff of diffs.
    for expected in [
        "new file mode 100644",
        "deleted file mode 100644",
        "rename from moved.txt",
        "rename to renamed.txt",
        "similarity index ",
        "Binary files a/blob.bin and b/blob.bin differ",
        "\\ No newline at end of file",
    ] {
        assert!(ours.contains(expected), "missing {expected:?} in:\n{ours}");
    }
    #[cfg(unix)]
    assert!(ours.contains("new mode 100755"), "mode change:\n{ours}");
}

/// A refs budget must shed the refs nobody decorates with. Dropping
/// `refs/remotes/origin/HEAD` reads as "this branch has no base", which is
/// silently wrong; dropping the last tag is visibly partial and harmless
/// (docs/design/git.md "GIT_STATE / GIT_ACK").
#[test]
fn refs_truncate_tags_before_load_bearing_refs() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    // A remote HEAD, and far more tags than the budget will admit.
    git(&dir, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(
        &dir,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    for i in 0..40 {
        git(&dir, &["tag", &format!("v{i}")]);
    }

    // Budgets are set on the handle rather than through the environment:
    // env vars are process-global, and these tests run beside others that
    // read the same budgets.
    let (mut handle, _info) = open(dir.to_str().unwrap()).unwrap();
    handle.budgets = std::sync::Arc::new(blit_git::Budgets {
        entries_max: 8,
        ..Default::default()
    });
    let msg = wait_first_state(&handle, StateOptions::default());

    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().expect("valid state");
    assert_ne!(
        mirror.flags & GIT_STATE_REFS_TRUNCATED,
        0,
        "40 tags past a cap of 8 must report truncation"
    );
    assert!(
        mirror.refs.contains_key("refs/heads/main"),
        "the checked-out branch survives: {:?}",
        mirror.refs.keys().collect::<Vec<_>>()
    );
    let remote_head = mirror
        .refs
        .get("refs/remotes/origin/HEAD")
        .expect("origin/HEAD survives the cap");
    assert_eq!(
        remote_head.target, "refs/remotes/origin/main",
        "and names its target rather than only peeling to an oid"
    );
    let tags = mirror
        .refs
        .keys()
        .filter(|n| n.starts_with("refs/tags/"))
        .count();
    assert!(tags < 40, "tags are what got shed, not branches");
}

/// `STATE_REMOTE` is opt-in, and carries URLs as configured — the caller
/// already has a shell, so withholding what they can `cat` buys nothing.
#[test]
fn remote_records_report_configured_urls() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    git(
        &dir,
        &[
            "remote",
            "add",
            "origin",
            "https://token@example.com/o/r.git",
        ],
    );
    git(&dir, &["remote", "add", "fork", "git@example.com:me/r.git"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();

    // Off by default: nobody pays for remotes unasked.
    let msg = wait_first_state(&handle, StateOptions::default());
    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().unwrap();
    assert!(mirror.remotes.is_empty(), "REMOTES is opt-in");

    let msg = wait_first_state(
        &handle,
        StateOptions {
            remotes: true,
            ..Default::default()
        },
    );
    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().unwrap();
    assert_eq!(
        mirror.remotes["origin"].fetch_url,
        "https://token@example.com/o/r.git"
    );
    assert_eq!(mirror.remotes["fork"].fetch_url, "git@example.com:me/r.git");
}

/// A prefix filter is what makes a large monorepo affordable: a UI that
/// renders branches stops paying for tags at every settle.
#[test]
fn ref_prefixes_narrow_the_snapshot() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    git(&dir, &["tag", "v1"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let msg = wait_first_state(
        &handle,
        StateOptions {
            ref_prefixes: vec!["refs/heads/".to_string()],
            ..Default::default()
        },
    );
    let mut mirror = GitStateMirror::new();
    mirror.apply_state(&msg).complete().unwrap();
    assert!(mirror.refs.contains_key("refs/heads/main"));
    assert!(
        !mirror.refs.contains_key("refs/tags/v1"),
        "tags are outside the requested prefixes: {:?}",
        mirror.refs.keys().collect::<Vec<_>>()
    );
}

/// `GIT_DISCOVER` answers "what repositories are under here" so a client
/// stops probing a ladder of candidate paths with an fs sync per level.
#[test]
fn discover_finds_repositories_under_a_path() {
    let root = temp_dir();
    for name in ["alpha", "nested/beta"] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
        std::fs::write(dir.join("f.txt"), "x\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-m", "c"]);
    }
    // A plain directory with no repository in it must not be reported.
    std::fs::create_dir_all(root.join("nested/empty")).unwrap();

    let cancel = Cancel::default();
    let resp = blit_git::discover(
        &GitDiscoverRequest {
            nonce: 1,
            flags: 0,
            depth: 3,
            path: root.to_str().unwrap(),
            after: "",
        },
        &cancel,
    );
    let (nonce, status, _flags, records) = parse_git_discover_resp(&resp).unwrap();
    assert_eq!((nonce, status), (1, GIT_STATUS_OK));
    let mut found: Vec<String> = git_discover_records(&records)
        .filter_map(|r| match r {
            GitDiscoverRecord::Repo { workdir, .. } => Some(workdir.to_string()),
            GitDiscoverRecord::Cursor { .. } => None,
        })
        .collect();
    found.sort();
    assert_eq!(found.len(), 2, "found: {found:?}");
    assert!(found.iter().any(|w| w.ends_with("alpha")), "{found:?}");
    assert!(found.iter().any(|w| w.ends_with("beta")), "{found:?}");

    // Depth 1 cannot reach nested/beta.
    let resp = blit_git::discover(
        &GitDiscoverRequest {
            nonce: 2,
            flags: 0,
            depth: 1,
            path: root.to_str().unwrap(),
            after: "",
        },
        &cancel,
    );
    let (_, _, _, records) = parse_git_discover_resp(&resp).unwrap();
    let shallow = git_discover_records(&records)
        .filter(|r| matches!(r, GitDiscoverRecord::Repo { .. }))
        .count();
    assert_eq!(shallow, 1, "depth bounds the walk");
}

/// A discovery capped at one repository per page is walked to the end with
/// the cursor it hands back. The bug this pins: counting the cap from the
/// start of the walk and filtering `after` afterwards makes every page
/// stop at the first repository and return nothing new, so a client pages
/// forever and never sees the rest.
#[test]
fn discover_pages_past_the_result_cap() {
    let root = temp_dir();
    for name in ["alpha", "nested/beta", "nested/deep/gamma"] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
    }
    let cancel = Cancel::default();
    let one_at_a_time = || blit_git::DiscoverLimits {
        depth_max: 16,
        results_max: 1,
        scan_max: 100_000,
    };

    let mut all: Vec<String> = Vec::new();
    let mut after = String::new();
    for page in 0..8 {
        let resp = blit_git::discover_within(
            &GitDiscoverRequest {
                nonce: 1,
                flags: 0,
                depth: 4,
                path: root.to_str().unwrap(),
                after: &after,
            },
            &cancel,
            one_at_a_time(),
        );
        let (_, status, flags, records) = parse_git_discover_resp(&resp).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut cursor = None;
        let mut got = 0usize;
        for record in git_discover_records(&records) {
            match record {
                GitDiscoverRecord::Repo { workdir, .. } => {
                    all.push(workdir.to_string());
                    got += 1;
                }
                GitDiscoverRecord::Cursor { after, .. } => cursor = Some(after.to_string()),
            }
        }
        if flags & GIT_DISCOVER_TRUNCATED == 0 {
            break;
        }
        assert_eq!(got, 1, "page {page} delivered {got} of a one-repo budget");
        let Some(cursor) = cursor else {
            panic!("a truncated page must say where it stopped");
        };
        assert_ne!(cursor, after, "the cursor has to move: page {page}");
        after = cursor;
    }
    all.sort();
    assert_eq!(
        all.len(),
        3,
        "every repository is reachable by paging: {all:?}"
    );
    for name in ["alpha", "beta", "gamma"] {
        assert!(
            all.iter().any(|w| w.ends_with(name)),
            "{name} missing: {all:?}"
        );
    }
}

/// A paged tree listing loses nothing at a boundary between a blob and a
/// subtree sharing a prefix. Git orders a tree as if every directory name
/// ended in `/`, so `lib.rs` (`.` = 0x2e) comes before `lib/` — the reverse
/// of the raw-name order a cursor used to be compared in, which silently
/// dropped the subtree when the page broke between them.
#[test]
fn tree_pages_in_git_order_across_a_prefix_boundary() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    // Names chosen so raw-byte and git order disagree: bytewise `lib` <
    // `lib.rs` < `libx`, but as a tree `lib.rs` < `lib/` < `libx`.
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    for path in ["lib.rs", "lib/inner.rs", "libx", "a.rs", "z.rs"] {
        std::fs::write(dir.join(path), "x\n").unwrap();
    }
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c"]);
    let head = rev(&dir, "HEAD");
    let (mut handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    // One entry per page, so every boundary in the tree is exercised.
    handle.budgets = Arc::new(blit_git::Budgets {
        entries_max: 1,
        ..Default::default()
    });

    let mut names: Vec<String> = Vec::new();
    let mut after = String::new();
    for page in 0..12 {
        let resp = handle.tree(
            &GitTreeRequest {
                nonce: 1,
                repo_id: 0,
                flags: 0,
                oid: head,
                path: "",
                after: &after,
            },
            &cancel,
        );
        let (_, status, flags, records) = parse_git_tree_resp(&resp).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut cursor = None;
        for record in git_tree_records(&records) {
            match record {
                GitTreeRecord::Entry { name, .. } => names.push(name.to_string()),
                GitTreeRecord::Cursor { after, .. } => cursor = Some(after.to_string()),
            }
        }
        if flags & GIT_TREE_TRUNCATED == 0 {
            break;
        }
        let Some(cursor) = cursor else {
            panic!("a truncated listing must say where it stopped");
        };
        assert_ne!(cursor, after, "the cursor has to move: page {page}");
        after = cursor;
    }
    names.sort();
    assert_eq!(
        names,
        vec!["a.rs", "lib", "lib.rs", "libx", "z.rs"],
        "every entry appears exactly once across the pages"
    );
}

/// `GIT_REFLOG` generalizes the reader the stash already used — and is the
/// only way to name an oid no longer reachable from any ref.
#[test]
fn reflog_reaches_an_amended_away_commit() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    let before_amend = rev(&dir, "HEAD");
    std::fs::write(dir.join("a.txt"), "two\n").unwrap();
    git(&dir, &["commit", "-am", "first, amended", "--amend"]);
    let after_amend = rev(&dir, "HEAD");
    assert_ne!(before_amend, after_amend);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let resp = handle.reflog(
        &GitReflogRequest {
            nonce: 1,
            repo_id: 0,
            flags: 0,
            limit: 0,
            ref_name: "",
            after_pos: 0,
        },
        &cancel,
    );
    let (nonce, status, _flags, records) = parse_git_reflog_resp(&resp).unwrap();
    assert_eq!((nonce, status), (1, GIT_STATUS_OK));
    let entries: Vec<_> = git_reflog_records(&records).collect();
    assert!(entries.len() >= 2, "commit and amend: {}", entries.len());
    // Newest first by default, matching `git reflog`.
    let GitReflogRecord::Entry { new, old, msg, .. } = &entries[0] else {
        panic!("expected an entry");
    };
    assert_eq!(*new, after_amend);
    assert_eq!(
        *old, before_amend,
        "the amended-away commit is named here and nowhere else"
    );
    assert!(
        msg.contains("amend"),
        "message carries the operation: {msg}"
    );

    // Oldest-first flips the order.
    let resp = handle.reflog(
        &GitReflogRequest {
            nonce: 2,
            repo_id: 0,
            flags: GIT_REFLOG_OLDEST_FIRST,
            limit: 0,
            ref_name: "HEAD",
            after_pos: 0,
        },
        &cancel,
    );
    let (_, _, _, records) = parse_git_reflog_resp(&resp).unwrap();
    let first = git_reflog_records(&records).next().unwrap();
    let GitReflogRecord::Entry { new, .. } = first else {
        panic!("expected an entry");
    };
    assert_eq!(new, before_amend, "oldest first");
}

/// A reflog longer than one page is walked to the end with the cursor the
/// truncated page hands back, and a ref that exists without a reflog is not
/// reported as missing.
#[test]
fn reflog_pages_and_separates_missing_from_empty() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "0\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "c0"]);
    for n in 1..6 {
        std::fs::write(dir.join("a.txt"), format!("{n}\n")).unwrap();
        git(&dir, &["commit", "-am", &format!("c{n}")]);
    }
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();

    // Two entries at a time, following the cursor.
    let mut messages: Vec<String> = Vec::new();
    let mut after_pos = 0u64;
    for page in 0..10 {
        let resp = handle.reflog(
            &GitReflogRequest {
                nonce: 1,
                repo_id: 0,
                flags: 0,
                limit: 2,
                ref_name: "HEAD",
                after_pos,
            },
            &cancel,
        );
        let (_, status, flags, records) = parse_git_reflog_resp(&resp).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut cursor = None;
        for record in git_reflog_records(&records) {
            match record {
                GitReflogRecord::Entry { msg, .. } => messages.push(msg.to_string()),
                GitReflogRecord::Cursor { pos, .. } => cursor = Some(pos),
            }
        }
        if flags & GIT_REFLOG_TRUNCATED == 0 {
            break;
        }
        let Some(pos) = cursor else {
            panic!("a truncated reflog page must say where it stopped");
        };
        assert!(pos > after_pos, "the cursor has to move: page {page}");
        after_pos = pos;
    }
    assert_eq!(
        messages.len(),
        6,
        "every entry is reachable by paging: {messages:?}"
    );
    // Newest first, and no entry seen twice.
    let mut deduped = messages.clone();
    deduped.dedup();
    assert_eq!(deduped.len(), messages.len(), "pages overlap: {messages:?}");

    // A branch created without moving: it exists, and has a reflog of its
    // own. A ref that does not exist at all is NOT_FOUND — the two answers
    // the doc promises are distinct.
    let resp = handle.reflog(
        &GitReflogRequest {
            nonce: 2,
            repo_id: 0,
            flags: 0,
            limit: 0,
            ref_name: "refs/heads/nope",
            after_pos: 0,
        },
        &cancel,
    );
    let (_, status, _, _) = parse_git_reflog_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_NOT_FOUND, "a missing ref is NOT_FOUND");

    // A tag has no reflog, but it does exist.
    git(&dir, &["tag", "v1"]);
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let resp = handle.reflog(
        &GitReflogRequest {
            nonce: 3,
            repo_id: 0,
            flags: 0,
            limit: 0,
            ref_name: "refs/tags/v1",
            after_pos: 0,
        },
        &cancel,
    );
    let (_, status, _, records) = parse_git_reflog_resp(&resp).unwrap();
    assert_eq!(
        status, GIT_STATUS_OK,
        "a ref with no reflog answers OK with nothing, not NOT_FOUND"
    );
    assert_eq!(git_reflog_records(&records).count(), 0);
}

/// `GIT_FETCH` against a local remote: the reply says per-ref what
/// happened, so "did I actually get these commits" is answerable from it
/// rather than from an exit code that lies.
#[test]
fn fetch_reports_per_ref_outcomes() {
    // Upstream repository with one commit.
    let upstream = temp_dir();
    git(&upstream, &["init", "-b", "main"]);
    std::fs::write(upstream.join("a.txt"), "one\n").unwrap();
    git(&upstream, &["add", "."]);
    git(&upstream, &["commit", "-m", "first"]);

    // A clone, then upstream moves on.
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    git(
        &dir,
        &["remote", "add", "origin", upstream.to_str().unwrap()],
    );
    let (handle, info) = open(dir.to_str().unwrap()).unwrap();
    assert_ne!(
        info.flags & GIT_REPO_FETCHABLE,
        0,
        "the test environment has git, so the open says so"
    );
    let cancel = Cancel::default();

    let resp = handle.fetch(
        &GitFetchRequest {
            nonce: 1,
            repo_id: 0,
            flags: 0,
            timeout_ms: 30_000,
            remote: "origin",
            refspecs: vec!["refs/heads/main:refs/remotes/origin/main"],
        },
        &cancel,
    );
    let (nonce, status, _flags, records) = parse_git_fetch_resp(&resp).unwrap();
    assert_eq!((nonce, status), (1, GIT_STATUS_OK));
    let refs: Vec<_> = git_fetch_records(&records).collect();
    assert!(!refs.is_empty(), "the fetch reported nothing");
    let GitFetchRecord::Ref {
        status, new, name, ..
    } = &refs[0];
    assert_eq!(*status, GIT_STATUS_OK, "refs: {refs:?}");
    assert_eq!(*name, "refs/remotes/origin/main");
    assert_eq!(*new, rev(&upstream, "HEAD"), "and names the oid we got");

    // The ordinary case: upstream advances and the tracking ref
    // fast-forwards. `git fetch --porcelain` writes a *space* as that
    // outcome's flag, so the line begins with two spaces — the most common
    // success is exactly the one a naive split drops.
    std::fs::write(upstream.join("a.txt"), "one\ntwo\n").unwrap();
    git(&upstream, &["commit", "-am", "second"]);
    let advanced = rev(&upstream, "HEAD");
    let resp = handle.fetch(
        &GitFetchRequest {
            nonce: 4,
            repo_id: 0,
            flags: 0,
            timeout_ms: 30_000,
            remote: "origin",
            refspecs: vec!["refs/heads/main:refs/remotes/origin/main"],
        },
        &cancel,
    );
    let (_, status, _, records) = parse_git_fetch_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let refs: Vec<_> = git_fetch_records(&records).collect();
    let Some(GitFetchRecord::Ref {
        flags,
        status,
        new,
        name,
        ..
    }) = refs.first()
    else {
        panic!("a fast-forward must produce a record: {records:?}");
    };
    assert_eq!(
        (*status, *name),
        (GIT_STATUS_OK, "refs/remotes/origin/main")
    );
    assert_eq!(*new, advanced, "and reports where the ref landed");
    assert_eq!(
        *flags & (GIT_FETCH_REF_NEW | GIT_FETCH_REF_FORCED | GIT_FETCH_REF_PRUNED),
        0,
        "a plain fast-forward is none of new, forced or pruned"
    );

    // A tag that moved. git writes `t` for that, its own letter distinct
    // from `+` — and an unhandled letter is a ref the reply never mentions,
    // which is the one thing this response exists to prevent.
    git(&upstream, &["tag", "v1"]);
    let resp = handle.fetch(
        &GitFetchRequest {
            nonce: 5,
            repo_id: 0,
            flags: 0,
            timeout_ms: 30_000,
            remote: "origin",
            refspecs: vec!["refs/tags/*:refs/tags/*"],
        },
        &cancel,
    );
    let (_, status, _, _) = parse_git_fetch_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    std::fs::write(upstream.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    git(&upstream, &["commit", "-am", "third"]);
    git(&upstream, &["tag", "-f", "v1"]);
    let moved = rev(&upstream, "HEAD");
    let resp = handle.fetch(
        &GitFetchRequest {
            nonce: 6,
            repo_id: 0,
            flags: 0,
            timeout_ms: 30_000,
            remote: "origin",
            refspecs: vec!["+refs/tags/*:refs/tags/*"],
        },
        &cancel,
    );
    let (_, status, _, records) = parse_git_fetch_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let tag = git_fetch_records(&records)
        .find(|r| {
            let GitFetchRecord::Ref { name, .. } = r;
            *name == "refs/tags/v1"
        })
        .unwrap_or_else(|| panic!("a moved tag must produce a record: {records:?}"));
    let GitFetchRecord::Ref {
        flags, status, new, ..
    } = tag;
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(new, moved, "and says where the tag landed");
    assert_ne!(
        flags & GIT_FETCH_REF_TAG_UPDATE,
        0,
        "a tag update is its own outcome, not a plain fast-forward"
    );

    // A refspec the remote cannot satisfy is reported, not swallowed.
    let resp = handle.fetch(
        &GitFetchRequest {
            nonce: 2,
            repo_id: 0,
            flags: 0,
            timeout_ms: 30_000,
            remote: "origin",
            refspecs: vec!["refs/heads/nope:refs/remotes/origin/nope"],
        },
        &cancel,
    );
    let (_, status, _, records) = parse_git_fetch_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK, "the request itself succeeded");
    let failed = git_fetch_records(&records).any(|r| {
        let GitFetchRecord::Ref { status, .. } = r;
        status != GIT_STATUS_OK
    });
    assert!(failed, "a refused refspec must surface: {records:?}");

    // An option-looking remote never reaches git's command line.
    let resp = handle.fetch(
        &GitFetchRequest {
            nonce: 3,
            repo_id: 0,
            flags: 0,
            timeout_ms: 0,
            remote: "--upload-pack=touch /tmp/pwned",
            refspecs: vec![],
        },
        &cancel,
    );
    let (_, status, _, _) = parse_git_fetch_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_INVALID);
}

/// `GIT_BLAME` attributes lines, checked against `git blame` rather than
/// against our own intent. Author and message are deliberately absent —
/// the client resolves the returned oids with one GIT_LOG.
#[test]
fn blame_attributes_lines_like_git() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    let first = rev(&dir, "HEAD");
    std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
    git(&dir, &["commit", "-am", "second"]);
    let second = rev(&dir, "HEAD");

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let resp = handle.blame(
        &GitBlameRequest {
            nonce: 1,
            repo_id: 0,
            flags: 0,
            oid: GIT_OID_NONE, // HEAD
            start_line: 0,
            line_count: 0,
            path: "a.txt",
        },
        &cancel,
    );
    let (nonce, status, _flags, records) = parse_git_blame_resp(&resp).unwrap();
    assert_eq!((nonce, status), (1, GIT_STATUS_OK));

    // Flatten to one commit per line, which is what `git blame` prints.
    let mut per_line: Vec<GitOid> = vec![GIT_OID_NONE; 4];
    for record in git_blame_records(&records) {
        let GitBlameRecord::Range {
            commit,
            start_line,
            line_count,
            ..
        } = record
        else {
            continue;
        };
        for line in start_line..start_line + line_count {
            per_line[(line - 1) as usize] = commit;
        }
    }
    assert_eq!(
        per_line[0], first,
        "line 1 is untouched since the first commit"
    );
    assert_eq!(per_line[1], second, "line 2 was rewritten");
    assert_eq!(per_line[2], first);
    assert_eq!(per_line[3], second, "line 4 was added");

    // A line range is the cheap case, and the reason the field exists.
    let resp = handle.blame(
        &GitBlameRequest {
            nonce: 2,
            repo_id: 0,
            flags: 0,
            oid: GIT_OID_NONE,
            start_line: 2,
            line_count: 1,
            path: "a.txt",
        },
        &cancel,
    );
    let (_, status, _, records) = parse_git_blame_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let ranges: Vec<_> = git_blame_records(&records).collect();
    assert_eq!(ranges.len(), 1, "one line, one range: {ranges:?}");
    let GitBlameRecord::Range {
        commit, start_line, ..
    } = &ranges[0]
    else {
        panic!("expected a range");
    };
    assert_eq!((*start_line, *commit), (2, second));

    // A viewport that runs past the end of the file, and "from line N to
    // the end". gix rejects a range longer than the file rather than
    // clamping it, so both of these used to fail as NOT_FOUND — blaming the
    // last page of a file being the common review action.
    for (nonce, start_line, line_count) in [(3u16, 3u32, 50u32), (4, 3, 0), (5, 1, 999)] {
        let resp = handle.blame(
            &GitBlameRequest {
                nonce,
                repo_id: 0,
                flags: 0,
                oid: GIT_OID_NONE,
                start_line,
                line_count,
                path: "a.txt",
            },
            &cancel,
        );
        let (_, status, flags, records) = parse_git_blame_resp(&resp).unwrap();
        assert_eq!(
            status, GIT_STATUS_OK,
            "start={start_line} count={line_count} must not fail"
        );
        assert_eq!(flags & GIT_BLAME_TRUNCATED, 0, "the file fits the budget");
        let last = git_blame_records(&records)
            .filter_map(|r| match r {
                GitBlameRecord::Range {
                    start_line,
                    line_count,
                    ..
                } => Some(start_line + line_count - 1),
                GitBlameRecord::Cursor { .. } => None,
            })
            .max();
        assert_eq!(
            last,
            Some(4),
            "the answer runs to the end of the file, not past it"
        );
    }

    // Beginning past the end is an empty answer, not an error.
    let resp = handle.blame(
        &GitBlameRequest {
            nonce: 6,
            repo_id: 0,
            flags: 0,
            oid: GIT_OID_NONE,
            start_line: 99,
            line_count: 10,
            path: "a.txt",
        },
        &cancel,
    );
    let (_, status, _, records) = parse_git_blame_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(git_blame_records(&records).count(), 0);
}

/// A blame the line budget cuts short says where it stopped and resumes
/// from there, rather than reporting the request's own size as truncation
/// and leaving the rest unreachable.
#[test]
fn blame_pages_past_the_line_budget() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    let body: String = (0..20).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.join("a.txt"), &body).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    let (mut handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    // Six lines a page, so a 20-line file takes four of them.
    handle.budgets = Arc::new(blit_git::Budgets {
        blame_lines_max: 6,
        ..Default::default()
    });
    let mut covered: Vec<u32> = Vec::new();
    let mut start_line = 1u32;
    for page in 0..10 {
        let resp = handle.blame(
            &GitBlameRequest {
                nonce: 1,
                repo_id: 0,
                flags: 0,
                oid: GIT_OID_NONE,
                start_line,
                line_count: 0,
                path: "a.txt",
            },
            &cancel,
        );
        let (_, status, flags, records) = parse_git_blame_resp(&resp).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut cursor = None;
        for record in git_blame_records(&records) {
            match record {
                GitBlameRecord::Range {
                    start_line,
                    line_count,
                    ..
                } => covered.extend(start_line..start_line + line_count),
                GitBlameRecord::Cursor { pos, .. } => cursor = Some(pos as u32),
            }
        }
        if flags & GIT_BLAME_TRUNCATED == 0 {
            break;
        }
        let Some(pos) = cursor else {
            panic!("a truncated blame must say where it stopped");
        };
        assert!(pos >= start_line, "the cursor has to move: page {page}");
        start_line = pos + 1;
    }
    covered.sort_unstable();
    assert_eq!(
        covered,
        (1..=20).collect::<Vec<u32>>(),
        "every line is attributed exactly once across the pages"
    );
}

/// indent-com/blit#120: a write that leaves a file's status letters
/// alone used to produce a byte-identical snapshot, which the engine
/// suppresses — so the server knew the worktree had moved and had no way
/// to say so. The STATUS record now carries the worktree content hash,
/// which the existing dedupe compares for free.
#[test]
fn worktree_edit_keeping_status_letters_still_pushes_state() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    // Already modified: the letters are ' M' before and after the edit.
    std::fs::write(dir.join("a.txt"), "two\n").unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let opts = StateOptions {
        status: true,
        status_latency: Duration::from_millis(20),
        refs_latency: Duration::from_millis(20),
        ..Default::default()
    };
    let state = handle.start_state(
        1,
        opts,
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );

    let mut mirror = GitStateMirror::new();
    let mut seen = 0usize;
    let oid_of = |mirror: &GitStateMirror| {
        mirror
            .status
            .iter()
            .find(|s| s.path == "a.txt")
            .map(|s| s.oid)
    };
    // First snapshot.
    let deadline = Instant::now() + Duration::from_secs(45);
    let first_oid = loop {
        if let Some(msg) = sent.lock().unwrap().get(seen).cloned() {
            seen += 1;
            if let GitStateApply::Complete(id) = mirror.apply_state(&msg) {
                state.ack(id);
                if let Some(oid) = oid_of(&mirror) {
                    assert_ne!(oid, GIT_OID_NONE, "the worktree hash is populated");
                    break oid;
                }
            }
        }
        assert!(Instant::now() < deadline, "no first snapshot with status");
        std::thread::sleep(Duration::from_millis(10));
    };

    // Edit again: still ' M', different content.
    std::fs::write(dir.join("a.txt"), "three\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(msg) = sent.lock().unwrap().get(seen).cloned() {
            seen += 1;
            if let GitStateApply::Complete(id) = mirror.apply_state(&msg) {
                state.ack(id);
                if let Some(oid) = oid_of(&mirror)
                    && oid != first_oid
                {
                    break;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "an edit that keeps the letters produced no snapshot (#120)"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    state.stop();
}

/// A snapshot past the per-message byte budget spans several `GIT_STATE`
/// messages sharing one `state_id`, and the mirror installs them together
/// — so a consumer never observes a half-built map, and only the final
/// chunk is acknowledged.
#[test]
fn oversized_snapshots_chunk_and_reassemble() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    // Enough refs that the snapshot cannot fit in a tiny byte budget.
    for i in 0..40 {
        git(&dir, &["branch", &format!("topic/{i}")]);
    }

    let (mut handle, _info) = open(dir.to_str().unwrap()).unwrap();
    handle.budgets = std::sync::Arc::new(blit_git::Budgets {
        bytes_max: 512,
        ..Default::default()
    });
    let sent: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let sent2 = sent.clone();
    let state = handle.start_state(
        1,
        StateOptions::default(),
        Box::new(move |m| {
            sent2.lock().unwrap().push(m);
            true
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut mirror = GitStateMirror::new();
    let mut applied = 0usize;
    let mut partials = 0usize;
    let mut acked = None;
    while acked.is_none() {
        let msgs = sent.lock().unwrap().clone();
        while applied < msgs.len() && acked.is_none() {
            match mirror.apply_state(&msgs[applied]) {
                GitStateApply::Partial => {
                    partials += 1;
                    // Nothing is installed while the snapshot assembles.
                    assert!(mirror.refs.is_empty(), "a partial chunk installed early");
                }
                // Only the final chunk carries a state_id to acknowledge.
                GitStateApply::Complete(id) => acked = Some(id),
                GitStateApply::Malformed => panic!("malformed chunk"),
            }
            applied += 1;
        }
        assert!(Instant::now() < deadline, "no complete snapshot arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
    state.stop();

    assert!(
        partials > 0,
        "40 branches under a 512-byte budget must span several messages"
    );
    assert!(
        mirror.refs.contains_key("refs/heads/main"),
        "the reassembled snapshot is the whole map"
    );
    assert!(
        mirror
            .refs
            .keys()
            .filter(|k| k.starts_with("refs/heads/topic/"))
            .count()
            > 1,
        "chunks from every part of the snapshot were kept: {:?}",
        mirror.refs.len()
    );
}

/// #117 level 2: with `text` in .gitattributes the object store holds LF,
/// so a CRLF worktree must not read as every line changed — a file the
/// user did not touch, shown as a rewrite. `RAW` opts back out.
#[test]
fn eol_attributes_normalize_the_worktree_side() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join(".gitattributes"), "*.txt text\n").unwrap();
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    // Same content, CRLF on disk: the object side stays LF-normalized.
    std::fs::write(dir.join("a.txt"), "one\r\ntwo\r\n").unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = |flags: u8| GitDiffRequest {
        nonce: 1,
        repo_id: 0,
        flags,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
        after: "",
    };

    let (_, status, _, records) = parse_git_diff_resp(&handle.diff(&req(0), &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let changed: Vec<_> = git_diff_records(&records)
        .filter_map(|r| match r {
            GitDiffRecord::Entry { new_path, .. } => Some(new_path.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        !changed.iter().any(|p| p == "a.txt"),
        "line endings alone are not a change: {changed:?}"
    );

    // RAW compares on-disk bytes as they are, which is a real question —
    // just not the default one.
    let (_, _, _, records) =
        parse_git_diff_resp(&handle.diff(&req(GIT_DIFF_RAW), &cancel)).unwrap();
    let raw: Vec<_> = git_diff_records(&records)
        .filter_map(|r| match r {
            GitDiffRecord::Entry { new_path, .. } => Some(new_path.to_string()),
            _ => None,
        })
        .collect();
    assert!(
        raw.iter().any(|p| p == "a.txt"),
        "RAW sees the bytes that are actually there: {raw:?}"
    );
}

/// `GIT_OPEN.parent_repo_id`: a submodule is named by `(parent, path)`
/// rather than by a filesystem location a client guessed from `.gitmodules`,
/// which blit does not expose. The refusals matter as much as the open — the
/// path is attacker-controlled and lands on the filesystem.
#[test]
fn open_submodule_by_parent_and_path() {
    // An upstream to be a submodule of the superproject.
    let sub = temp_dir();
    git(&sub, &["init", "-b", "main"]);
    std::fs::write(sub.join("s.txt"), "sub\n").unwrap();
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-m", "sub first"]);

    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("a.txt"), "top\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    git(
        &dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            sub.to_str().unwrap(),
            "vendor/lib",
        ],
    );
    git(&dir, &["commit", "-m", "add submodule"]);

    let (parent, parent_info) = open(dir.to_str().unwrap()).unwrap();
    let (_child, child_info) = blit_git::open_submodule(&parent, "vendor/lib").unwrap();
    assert_ne!(
        child_info.gitdir, parent_info.gitdir,
        "the submodule's own gitdir, not the parent answering again"
    );
    assert!(
        child_info.gitdir.starts_with(&parent_info.gitdir),
        "git absorbs a submodule's gitdir under the parent's .git/modules: {}",
        child_info.gitdir
    );

    // A refusal carries its code and a reason; opening instead is the bug.
    let refused = |path: &str| match blit_git::open_submodule(&parent, path) {
        Ok((_, info)) => panic!("{path} must be refused, opened {}", info.workdir),
        Err(err) => err,
    };

    // A path that is not a submodule: the parent answers for it, and saying
    // so is the whole point of comparing gitdirs.
    let (status, detail) = refused("");
    assert_eq!(status, GIT_STATUS_WRONG_TYPE, "{detail}");

    // Initialized but never updated — an empty directory. Discovery from it
    // walks up and finds the parent, so this used to report WRONG_TYPE; the
    // honest answer is that there is no checkout there yet.
    std::fs::create_dir_all(dir.join("vendor/empty")).unwrap();
    let (status, detail) = refused("vendor/empty");
    assert_eq!(status, GIT_STATUS_NOT_FOUND, "{detail}");

    // A symlink out of the worktree. `..` and absolute paths are refused
    // lexically; a symlink is neither, so the path is resolved and checked.
    #[cfg(unix)]
    {
        let outside = temp_dir();
        git(&outside, &["init", "-b", "main"]);
        std::fs::write(outside.join("o.txt"), "x\n").unwrap();
        git(&outside, &["add", "."]);
        git(&outside, &["commit", "-m", "outside"]);
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();
        let (status, detail) = refused("escape");
        assert_eq!(
            status, GIT_STATUS_INVALID,
            "a symlinked escape is refused, not opened: {detail}"
        );
    }

    // And the lexical refusals still hold.
    for path in ["../elsewhere", "/etc"] {
        let (status, detail) = refused(path);
        assert_eq!(status, GIT_STATUS_INVALID, "{path}: {detail}");
    }
}

/// A `GIT_PATCH` cut short mid-file resumes mid-file. `after` names the
/// file and `after_pos` the rows of it already delivered — without which a
/// file whose rows outgrow the byte budget was re-sent from row 0 on every
/// request, so the tail of a large diff was unreachable and the rows the
/// client already had came back as duplicates.
#[test]
fn patch_resumes_inside_one_large_file() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    // One file big enough that its rows cannot fit in a small budget, plus a
    // second so the boundary between files is exercised too.
    let before: String = (0..400).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.join("big.txt"), &before).unwrap();
    std::fs::write(dir.join("z.txt"), "one\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "first"]);
    let after_body: String = (0..400).map(|n| format!("LINE {n}\n")).collect();
    std::fs::write(dir.join("big.txt"), &after_body).unwrap();
    std::fs::write(dir.join("z.txt"), "two\n").unwrap();
    git(&dir, &["commit", "-am", "second"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let old = GitEndpoint {
        kind: GIT_ENDPOINT_COMMIT,
        oid: rev(&dir, "HEAD~1"),
    };
    let new = GitEndpoint {
        kind: GIT_ENDPOINT_COMMIT,
        oid: rev(&dir, "HEAD"),
    };

    // Rows collected as (file, old_line, new_line) across every page.
    let mut rows: Vec<(String, u32, u32)> = Vec::new();
    let mut after = String::new();
    let mut after_pos = 0u64;
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages < 200, "paging did not converge");
        let resp = handle.patch(
            &GitPatchRequest {
                nonce: 1,
                repo_id: 0,
                flags: 0,
                context: 3,
                rename: 0,
                old,
                new,
                path: "",
                // Small enough to cut inside big.txt several times over.
                max_len: 2048,
                after: &after,
                after_pos,
            },
            &cancel,
        );
        let (_, status, flags, payload) = parse_git_patch_resp(&resp).unwrap();
        assert_eq!(status, GIT_STATUS_OK, "page {pages}");
        let mut file = String::new();
        let mut cursor: Option<(String, u64)> = None;
        for record in git_patch_records(&payload) {
            match record {
                GitPatchRecord::File { new_path, .. } => file = new_path.to_string(),
                GitPatchRecord::Row {
                    old_line, new_line, ..
                } => rows.push((file.clone(), old_line, new_line)),
                GitPatchRecord::Cursor { after, pos } => cursor = Some((after.to_string(), pos)),
                _ => {}
            }
        }
        if flags & GIT_PATCH_TRUNCATED == 0 {
            break;
        }
        let Some((cursor_after, cursor_pos)) = cursor else {
            panic!("a truncated patch must say where it stopped: page {pages}");
        };
        // The cursor has to change, or the loop is the bug it tests for. It
        // is not monotone in `pos`: `pos` 0 means "past this file entirely",
        // which is how a cut *between* files is named.
        assert!(
            (cursor_after.as_str(), cursor_pos) != (after.as_str(), after_pos),
            "the cursor has to move: page {pages} at {cursor_after}:{cursor_pos}"
        );
        after = cursor_after;
        after_pos = cursor_pos;
    }
    assert!(pages > 2, "the budget was meant to force several pages");

    // Every changed line of both files, once each, and in order.
    let deduped = {
        let mut d = rows.clone();
        d.dedup();
        d
    };
    assert_eq!(deduped, rows, "pages re-sent rows the client already had");
    let big_rows: Vec<u32> = rows
        .iter()
        .filter(|(f, ..)| f == "big.txt")
        .map(|&(_, _, new_line)| new_line)
        .collect();
    assert_eq!(
        big_rows,
        (1..=400).collect::<Vec<u32>>(),
        "every line of the large file is delivered exactly once"
    );
    assert!(
        rows.iter().any(|(f, ..)| f == "z.txt"),
        "and the file after it is reached: {:?}",
        rows.iter().map(|(f, ..)| f).collect::<Vec<_>>()
    );
}

/// Sibling repositories are walked in path order, not in whatever order the
/// filesystem hands them back. A stateless resume replays the walk and finds
/// the cursor by path, so the order has to be the same on every call —
/// `read_dir` guarantees none, and a page that reached the cursor at a
/// different point would skip or repeat its neighbours.
#[test]
fn discover_walks_siblings_in_path_order() {
    let root = temp_dir();
    // Created in an order that is neither alphabetical nor reverse, so an
    // unsorted walk is unlikely to come out sorted by accident.
    for name in ["mid", "zeta", "alpha", "kappa", "beta"] {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-b", "main"]);
    }
    let cancel = Cancel::default();

    // One per page, so the sequence is the walk order rather than one
    // response's record order.
    let mut seen: Vec<String> = Vec::new();
    let mut after = String::new();
    for _ in 0..12 {
        let resp = blit_git::discover_within(
            &GitDiscoverRequest {
                nonce: 1,
                flags: 0,
                depth: 2,
                path: root.to_str().unwrap(),
                after: &after,
            },
            &cancel,
            blit_git::DiscoverLimits {
                depth_max: 16,
                results_max: 1,
                scan_max: 100_000,
            },
        );
        let (_, status, flags, records) = parse_git_discover_resp(&resp).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut cursor = None;
        for record in git_discover_records(&records) {
            match record {
                GitDiscoverRecord::Repo { workdir, .. } => seen.push(workdir.to_string()),
                GitDiscoverRecord::Cursor { after, .. } => cursor = Some(after.to_string()),
            }
        }
        if flags & GIT_DISCOVER_TRUNCATED == 0 {
            break;
        }
        after = cursor.expect("a truncated page names its stopping point");
    }
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted, "the walk order is the path order: {seen:?}");
    assert_eq!(seen.len(), 5, "and every repository is reached: {seen:?}");
}

/// `GIT_PATCH` makes the same rejections `GIT_DIFF` does. The low flag bits
/// and `rename` are the same fields, so a threshold above 100 or an
/// undefined flag bit is `INVALID` on both — not accepted by one endpoint and
/// silently degraded to the exact-oid join.
#[test]
fn patch_rejects_what_diff_rejects() {
    let dir = fixture();
    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let head = GitEndpoint {
        kind: GIT_ENDPOINT_COMMIT,
        oid: rev(&dir, "HEAD"),
    };
    let worktree = GitEndpoint {
        kind: GIT_ENDPOINT_WORKTREE,
        oid: GIT_OID_NONE,
    };
    let patch = |flags: u16, rename: u8| {
        let resp = handle.patch(
            &GitPatchRequest {
                nonce: 1,
                repo_id: 0,
                flags,
                context: 3,
                rename,
                old: head,
                new: worktree,
                path: "",
                max_len: 0,
                after: "",
                after_pos: 0,
            },
            &cancel,
        );
        parse_git_patch_resp(&resp).unwrap().1
    };
    let diff = |flags: u8, rename: u8| {
        let resp = handle.diff(
            &GitDiffRequest {
                nonce: 1,
                repo_id: 0,
                flags,
                rename,
                old: head,
                new: worktree,
                path: "",
                after: "",
            },
            &cancel,
        );
        parse_git_diff_resp(&resp).unwrap().1
    };

    // A threshold past 100 is not a percentage.
    assert_eq!(diff(GIT_DIFF_RENAMES, 101), GIT_STATUS_INVALID);
    assert_eq!(patch(GIT_PATCH_RENAMES, 101), GIT_STATUS_INVALID);
    // An undefined flag bit is refused, not ignored.
    assert_eq!(patch(1 << 15, 0), GIT_STATUS_INVALID);
    // And the valid extremes still work.
    assert_eq!(patch(GIT_PATCH_RENAMES, 100), GIT_STATUS_OK);
    assert_eq!(patch(GIT_PATCH_TEXT, 0), GIT_STATUS_OK);
}

/// `BINARY` emits git's `GIT binary patch` block, and the test of that is
/// not what the bytes look like but whether git accepts them: the patch is
/// applied with `git apply --binary` to the pre-change content and the
/// result compared byte for byte. Reversing it is part of the format —
/// git writes a second body for `-R` — so that is applied too.
#[test]
fn binary_patch_applies_with_git_apply() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    // A modified binary, a new one, and a deleted one: the three shapes,
    // including the empty side an add and a delete each have.
    let before: Vec<u8> = (0u16..600).map(|n| (n % 251) as u8).collect();
    let after: Vec<u8> = (0u16..600).map(|n| (n % 241) as u8).collect();
    let added: Vec<u8> = vec![0u8, 1, 2, 0, 255, 0, 7];
    let removed: Vec<u8> = vec![9u8, 0, 9, 0, 9];
    std::fs::write(dir.join("mod.bin"), &before).unwrap();
    std::fs::write(dir.join("gone.bin"), &removed).unwrap();
    std::fs::write(dir.join("keep.txt"), "text\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::write(dir.join("mod.bin"), &after).unwrap();
    std::fs::write(dir.join("new.bin"), &added).unwrap();
    std::fs::remove_file(dir.join("gone.bin")).unwrap();
    std::fs::write(dir.join("keep.txt"), "text again\n").unwrap();
    git(&dir, &["add", "-A"]);

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let patch_text = |flags: u16| {
        let req = GitPatchRequest {
            nonce: 1,
            repo_id: 0,
            flags,
            context: 3,
            rename: 0,
            old: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&dir, "HEAD"),
            },
            new: GitEndpoint {
                kind: GIT_ENDPOINT_INDEX,
                oid: GIT_OID_NONE,
            },
            path: "",
            max_len: 0,
            after: "",
            after_pos: 0,
        };
        let (_, status, _, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        data
    };

    // Without the flag: git's sentence, and no content — `git diff`'s own
    // behaviour without `--binary`.
    let plain = String::from_utf8(patch_text(GIT_PATCH_TEXT)).unwrap();
    assert!(
        plain.contains("Binary files a/mod.bin and b/mod.bin differ"),
        "{plain}"
    );
    assert!(!plain.contains("GIT binary patch"), "{plain}");

    let data = patch_text(GIT_PATCH_TEXT | GIT_PATCH_BINARY);
    let text = String::from_utf8(data.clone()).unwrap();
    assert_eq!(
        text.matches("GIT binary patch").count(),
        3,
        "one block per binary file, and none for the text file: {text}"
    );
    assert!(
        !text.contains("Binary files"),
        "the content replaces the sentence: {text}"
    );

    // Apply it forward: the tree must land byte-identical to the new side.
    let apply = temp_dir();
    git(&apply, &["init", "-b", "main"]);
    std::fs::write(apply.join("mod.bin"), &before).unwrap();
    std::fs::write(apply.join("gone.bin"), &removed).unwrap();
    std::fs::write(apply.join("keep.txt"), "text\n").unwrap();
    git(&apply, &["add", "."]);
    git(&apply, &["commit", "-m", "seed"]);
    std::fs::write(apply.join("patch.diff"), &data).unwrap();
    git(&apply, &["apply", "--binary", "patch.diff"]);
    assert_eq!(std::fs::read(apply.join("mod.bin")).unwrap(), after);
    assert_eq!(std::fs::read(apply.join("new.bin")).unwrap(), added);
    assert!(!apply.join("gone.bin").exists(), "the delete applied");

    // And back: the reverse body is why the block has two halves.
    git(&apply, &["apply", "--binary", "-R", "patch.diff"]);
    assert_eq!(std::fs::read(apply.join("mod.bin")).unwrap(), before);
    assert_eq!(std::fs::read(apply.join("gone.bin")).unwrap(), removed);
    assert!(!apply.join("new.bin").exists(), "the add reversed");
}

/// A worktree side carries the hash `modified_status` computed for the file it
/// read, and nothing writes that blob to the object database — so an unstaged
/// edit exists only on disk. Reading the side by oid finds nothing, and an
/// empty new side renders as the whole file deleted: `git diff HEAD` is the
/// oracle, since every unstaged change in a checkout goes through this.
#[test]
fn worktree_patch_reads_unstaged_content_from_disk() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    std::fs::write(dir.join("append.md"), "line one\nline two\n").unwrap();
    std::fs::write(dir.join("rewrite.md"), "before\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    std::fs::write(dir.join("append.md"), "line one\nline two\nthree\nfour\n").unwrap();
    std::fs::write(dir.join("rewrite.md"), "after\n").unwrap();
    std::fs::write(dir.join("fresh.md"), "untracked\n").unwrap();

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let req = GitPatchRequest {
        nonce: 1,
        repo_id: 0,
        flags: GIT_PATCH_TEXT | GIT_PATCH_UNTRACKED,
        context: 3,
        rename: 0,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: rev(&dir, "HEAD"),
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
        max_len: 0,
        after: "",
        after_pos: 0,
    };
    let (_, status, _, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let ours = String::from_utf8(data).expect("patch text is UTF-8");

    // An append leaves its old lines as context, so their presence as `-`
    // rows is exactly the regression this covers.
    assert!(
        !ours.contains("-line one"),
        "the file's committed content read as deleted:\n{ours}"
    );
    for expected in [
        "+three\n",
        "+four\n",
        "-before\n",
        "+after\n",
        "+untracked\n",
    ] {
        assert!(ours.contains(expected), "missing {expected:?} in:\n{ours}");
    }

    // The tracked half against git itself, oids aside: hunk headers included,
    // since a wrong new side gets those wrong too.
    let theirs = git_out(&dir, &["diff", "--no-color", "-U3", "HEAD"]);
    let mut in_untracked = false;
    let tracked: Vec<String> = ours
        .lines()
        .filter(|line| {
            if line.starts_with("diff --git ") {
                in_untracked = line.starts_with("diff --git a/fresh.md");
            }
            !in_untracked
        })
        .map(collapse_patch_oids)
        .collect();
    let expected: Vec<String> = theirs.lines().map(collapse_patch_oids).collect();
    assert_eq!(
        tracked, expected,
        "\n--- ours ---\n{ours}\n--- git ---\n{theirs}\n"
    );
}

/// "index <old>..<new> [mode]" without the oids: comparing against `git diff`
/// is about the header set and the hunks, not about abbreviation length.
fn collapse_patch_oids(line: &str) -> String {
    match line.strip_prefix("index ") {
        Some(rest) => {
            let mode = rest.split_once(' ').map(|(_, m)| m).unwrap_or("");
            format!("index <oids> {mode}").trim_end().to_string()
        }
        None => line.to_string(),
    }
}

/// `TEXT` mode's budget is a file boundary, not a wall: appending a file that
/// crosses it rolls that file back and reports `TRUNCATED`, so a change set
/// larger than `max_len` still renders. Only a single file too big to describe
/// on its own is `TOO_LARGE` (docs/design/git.md "GIT_PATCH").
#[test]
fn text_patch_truncates_at_a_file_boundary() {
    let dir = temp_dir();
    git(&dir, &["init", "-b", "main"]);
    let body = |tag: &str| {
        (0..40)
            .map(|n| format!("{tag} line {n}\n"))
            .collect::<String>()
    };
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), body("old")).unwrap();
    }
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-m", "seed"]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(dir.join(name), body("new")).unwrap();
    }

    let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
    let cancel = Cancel::default();
    let page = |max_len: u32, after: &str| {
        let req = GitPatchRequest {
            nonce: 1,
            repo_id: 0,
            flags: GIT_PATCH_TEXT,
            context: 3,
            rename: 0,
            old: GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&dir, "HEAD"),
            },
            new: GitEndpoint {
                kind: GIT_ENDPOINT_WORKTREE,
                oid: GIT_OID_NONE,
            },
            path: "",
            max_len,
            after,
            after_pos: 0,
        };
        let (_, status, flags, data) = parse_git_patch_resp(&handle.patch(&req, &cancel)).unwrap();
        (
            status,
            flags,
            String::from_utf8(data).expect("patch text is UTF-8"),
        )
    };

    let whole = page(0, "").2;
    let one_file = whole
        .find("diff --git a/b.txt")
        .expect("three file sections");
    // A budget between one and two files: the first must arrive whole.
    let (status, flags, first) = page((one_file + 8) as u32, "");
    assert_eq!(status, GIT_STATUS_OK, "a page-sized change set refused");
    assert_ne!(flags & GIT_PATCH_TRUNCATED, 0, "partial page not reported");
    assert_eq!(
        first.matches("diff --git ").count(),
        1,
        "the file that crossed the budget was kept:\n{first}"
    );
    assert!(first.starts_with("diff --git a/a.txt"), "{first}");
    assert!(
        first.len() <= one_file + 8,
        "over budget after rolling back:\n{first}"
    );

    // Resuming from the last path delivered makes progress, per the TEXT-mode
    // contract (no CURSOR record to hand back).
    let (status, _, second) = page((one_file + 8) as u32, "a.txt");
    assert_eq!(status, GIT_STATUS_OK);
    assert!(second.starts_with("diff --git a/b.txt"), "{second}");

    // One file alone over the budget is the case that has nowhere to cut.
    assert_eq!(page(64, "").0, GIT_STATUS_TOO_LARGE);
}

/// A gitlink's path is a directory on disk, and the worktree side skipped
/// every directory: a clean superproject reported each of its submodules
/// deleted, and the untracked walk claimed the submodule's own checkout —
/// `.git` included — as new files. Git reports neither. A gitlink now reads
/// the checked-out submodule's HEAD, an uninitialized one stays clean, and a
/// moved one is a single M entry carrying the SUBMODULE flag.
#[test]
fn submodule_reads_its_checked_out_head() {
    fn worktree_diff(dir: &Path, old: GitEndpoint) -> Vec<(u8, String, u8, GitOid)> {
        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let req = GitDiffRequest {
            nonce: 1,
            repo_id: 0,
            flags: GIT_DIFF_UNTRACKED,
            rename: 0,
            old,
            new: GitEndpoint {
                kind: GIT_ENDPOINT_WORKTREE,
                oid: GIT_OID_NONE,
            },
            path: "",
            after: "",
        };
        let (_, status, _, records) =
            parse_git_diff_resp(&handle.diff(&req, &Cancel::default())).unwrap();
        assert_eq!(status, GIT_STATUS_OK);
        let mut got: Vec<(u8, String, u8, GitOid)> = git_diff_records(&records)
            .filter_map(|r| match r {
                GitDiffRecord::Entry {
                    st,
                    new_path,
                    dflags,
                    new_oid,
                    ..
                } => Some((st, new_path.to_string(), dflags, new_oid)),
                _ => None,
            })
            .collect();
        got.sort_by(|a, b| a.1.cmp(&b.1));
        got
    }
    let names = |got: &[(u8, String, u8, GitOid)]| -> Vec<String> {
        got.iter()
            .map(|(st, path, ..)| format!("{} {path}", *st as char))
            .collect()
    };

    let root = temp_dir();
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-b", "main"]);
    std::fs::write(sub.join("s.txt"), "one\n").unwrap();
    git(&sub, &["add", "."]);
    git(&sub, &["commit", "-m", "s1"]);

    let sup = root.join("super");
    std::fs::create_dir_all(&sup).unwrap();
    git(&sup, &["init", "-b", "main"]);
    std::fs::write(sup.join("top.txt"), "top\n").unwrap();
    git(&sup, &["add", "."]);
    git(&sup, &["commit", "-m", "c1"]);
    let file_urls = ["-c", "protocol.file.allow=always"];
    let add = [
        &file_urls[..],
        &["submodule", "add", sub.to_str().unwrap(), "deps/mod"],
    ]
    .concat();
    git(&sup, &add);
    git(&sup, &["commit", "-m", "add submodule"]);

    // Clean checkout: git reports nothing, from either old side.
    assert_eq!(git_out(&sup, &["status", "--porcelain"]), "");
    let head = rev(&sup, "HEAD");
    assert_eq!(
        names(&worktree_diff(
            &sup,
            GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: head,
            },
        )),
        Vec::<String>::new()
    );
    assert_eq!(
        names(&worktree_diff(
            &sup,
            GitEndpoint {
                kind: GIT_ENDPOINT_INDEX,
                oid: GIT_OID_NONE,
            },
        )),
        Vec::<String>::new()
    );

    // Same for the STATUS records a review UI lists its files from.
    let status_letters = |dir: &Path| -> Vec<(String, u8, u8)> {
        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let msg = wait_first_state(
            &handle,
            StateOptions {
                status: true,
                untracked: true,
                ..Default::default()
            },
        );
        let mut mirror = GitStateMirror::new();
        mirror.apply_state(&msg).complete().unwrap();
        let mut out: Vec<(String, u8, u8)> = mirror
            .status
            .into_iter()
            .map(|s| (s.path, s.staged, s.unstaged))
            .collect();
        out.sort();
        out
    };
    assert_eq!(status_letters(&sup), Vec::new());

    // A clone that never ran `submodule update` has an empty directory
    // there, and no repository to read a HEAD from: still clean.
    let clone = [&file_urls[..], &["clone", sup.to_str().unwrap(), "clone"]].concat();
    git(&root, &clone);
    let cloned = root.join("clone");
    assert_eq!(git_out(&cloned, &["status", "--porcelain"]), "");
    assert!(
        std::fs::read_dir(cloned.join("deps/mod"))
            .unwrap()
            .next()
            .is_none()
    );
    assert_eq!(
        names(&worktree_diff(
            &cloned,
            GitEndpoint {
                kind: GIT_ENDPOINT_COMMIT,
                oid: rev(&cloned, "HEAD"),
            },
        )),
        Vec::<String>::new()
    );

    // Moving the submodule's HEAD is one M entry flagged SUBMODULE, whose
    // new oid is the commit now checked out there.
    std::fs::write(sup.join("deps/mod/s.txt"), "two\n").unwrap();
    git(&sup.join("deps/mod"), &["commit", "-am", "s2"]);
    let moved = rev(&sup.join("deps/mod"), "HEAD");
    assert_eq!(
        git_out(&sup, &["status", "--porcelain"]),
        " M deps/mod",
        "git's own view"
    );
    let got = worktree_diff(
        &sup,
        GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: head,
        },
    );
    assert_eq!(names(&got), vec!["M deps/mod".to_string()]);
    assert_ne!(got[0].2 & GIT_DIFF_ENTRY_SUBMODULE, 0, "SUBMODULE flag");
    assert_eq!(got[0].3, moved, "new side is the submodule's HEAD");
    assert_eq!(
        status_letters(&sup),
        vec![("deps/mod".to_string(), b' ', b'M')]
    );
}
