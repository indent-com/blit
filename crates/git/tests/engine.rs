//! End-to-end engine tests against fixture repositories built with the
//! real git CLI (docs/git.md).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blit_git::{Cancel, StateOptions, open};
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
    // A non-repo fails with WRONG_TYPE.
    let plain = temp_dir();
    assert!(open(plain.to_str().unwrap()).is_err());
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
    let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
    let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
    // Unknown ref.
    assert_eq!(resolve("nope").0, GIT_STATUS_NOT_FOUND);
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
