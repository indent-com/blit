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

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .output()
        .expect("run git");
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
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .output()
        .expect("run git");
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
    let out = Command::new("git")
        .current_dir(dir)
        .args(["rev-parse", spec])
        .output()
        .unwrap();
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
    // empty/NUL path OTHER (docs/git.md status table).
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
    assert_eq!(open("").err().unwrap().0, GIT_STATUS_OTHER);
    assert_eq!(open("bad\0path").err().unwrap().0, GIT_STATUS_OTHER);
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
    assert_eq!(mirror.apply_state(&msg), Some(state_id));
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
        let _ = Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .args(args)
            .output()
            .expect("run git");
    };
    let state_mirror = |dir: &Path| {
        let (handle, _info) = open(dir.to_str().unwrap()).unwrap();
        let msg = wait_first_state(&handle, StateOptions::default());
        let mut mirror = GitStateMirror::new();
        mirror.apply_state(&msg).expect("valid state");
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
    let id = mirror.apply_state(&first).expect("valid state");
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
            let id = mirror.apply_state(&msg).expect("valid update");
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
                    let id = mirror.apply_state(&msg).expect("valid state");
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
    let resp = handle.tree(1, &head, "", &cancel);
    let (nonce, status, _flags, records) = parse_git_tree_resp(&resp).unwrap();
    assert_eq!((nonce, status), (1, GIT_STATUS_OK));
    let names: Vec<String> = git_tree_records(&records)
        .map(|record| match record {
            GitTreeRecord::Entry { name, .. } => name.to_string(),
        })
        .collect();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
    // Blob by commit + path.
    let resp = handle.blob(2, &head, "a.txt", 0);
    let (_, status, size, data) = parse_git_blob_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    assert_eq!(size as usize, data.len());
    assert_eq!(data, b"alpha\nBETA\ngamma\n");
    // TOO_LARGE carries the true size.
    let resp = handle.blob(3, &head, "a.txt", 4);
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
        old: commit(rev(&dir, "HEAD~1")),
        new: commit(rev(&dir, "HEAD")),
        path: "",
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
        old: commit(rev(&dir, "HEAD")),
        new: plain(GIT_ENDPOINT_INDEX),
        path: "",
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
        old: plain(GIT_ENDPOINT_INDEX),
        new: plain(GIT_ENDPOINT_WORKTREE),
        path: "",
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: rev(&dir, "HEAD~1"),
        },
        new: commit(rev(&dir, "HEAD")),
        path: "",
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
        old: commit(rev(&dir, "HEAD~1")),
        new: commit(rev(&dir, "HEAD")),
        path: "",
        max_len: 0,
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
        old: plain(GIT_ENDPOINT_INDEX),
        new: plain(GIT_ENDPOINT_WORKTREE),
        path: "a.txt",
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
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
    let resp = handle.index(1, "", &cancel);
    let (_, status, _flags, records) = parse_git_index_resp(&resp).unwrap();
    assert_eq!(status, GIT_STATUS_OK);
    let paths: Vec<String> = git_index_records(&records)
        .map(|r| match r {
            GitIndexRecord::Entry { path, stage, .. } => {
                assert_eq!(stage, 0);
                path.to_string()
            }
        })
        .collect();
    assert_eq!(paths, vec!["a.txt", "c.txt"]);
    // Staged rename detected by exact oid.
    let req = GitDiffRequest {
        nonce: 2,
        repo_id: 0,
        flags: GIT_DIFF_RENAMES,
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: rev(&dir, "HEAD"),
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        path: "",
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
        mirror.apply_state(&msg).unwrap();
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
    mirror.apply_state(&msg).unwrap();
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_INDEX,
            oid: GIT_OID_NONE,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        },
        path: "",
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: main_tip,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: feature_tip,
        },
        path: "",
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: main_tip,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_MERGE_BASE,
            oid: feature_tip,
        },
        path: "",
    };
    let (_, status, _, _) = parse_git_diff_resp(&handle.diff(&bad, &cancel)).unwrap();
    assert_eq!(status, GIT_STATUS_INVALID);
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
/// bases and `A...B` resolves with no hides.
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
        old: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: a,
        },
        new: GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: b,
        },
        path: "",
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
    mirror.apply_state(&msg).expect("valid state");
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
    mirror.apply_state(&msg).expect("valid state");
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
                    let id = mirror.apply_state(&msg).expect("valid state");
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
                    let id = mirror.apply_state(&msg).expect("valid state");
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
    let first_id = mirror.apply_state(&first).expect("valid state");
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
    let second_id = mirror.apply_state(&second).expect("valid state");
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
    let id = mirror.apply_state(&first_b).expect("valid state");
    state_b.ack(id);
    git(&dir, &["branch", "side"]);
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut applied = 1usize;
    loop {
        let next = sent_b.lock().unwrap().get(applied).cloned();
        if let Some(msg) = next {
            applied += 1;
            let id = mirror.apply_state(&msg).expect("valid state");
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
        mirror.apply_state(msg).expect("valid state");
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
            let id = mirror.apply_state(&msg).expect("valid state");
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
                    let id = mirror.apply_state(&msg).expect("valid state");
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
                    let id = mirror.apply_state(&msg).expect("valid state");
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
