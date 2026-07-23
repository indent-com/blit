//! Git introspection wire protocol (docs/git.md).
//!
//! Mutable, small repository state — HEAD, refs, in-progress operation,
//! index/worktree status — is *pushed* as whole-snapshot `GIT_STATE`
//! messages the client applies by replacement ([`GitStateMirror`]).
//! Immutable, large content — commits, trees, blobs, diffs, patches — is
//! *pulled* by content address through nonce-correlated request/response
//! pairs that share an opcode value across directions.
//!
//! All integers little-endian, tightly packed, as everywhere in the protocol.

use std::collections::BTreeMap;

/// `S2C_HELLO` feature bit: server supports the `GIT_*` message family.
pub const FEATURE_GIT: u32 = 1 << 7;

// C2S opcodes.

/// Open (discover) a repository: [0x50][nonce:2][flags:1][refs_latency_ms:2][status_latency_ms:2][path_len:2][path:N]
/// `path` is plain UTF-8 (client-chosen filesystem location, like `FS_SYNC`).
pub const C2S_GIT_OPEN: u8 = 0x50;
/// Release a repo id: [0x51][repo_id:2]
pub const C2S_GIT_CLOSE: u8 = 0x51;
/// Acknowledge a state snapshot: [0x52][repo_id:2][state_id:4]
pub const C2S_GIT_ACK: u8 = 0x52;
/// Walk `hides..tips`: [0x53][nonce:2][repo_id:2][flags:1][limit:2][path_len:2][path:N][n_tips:2][tips:32·N][n_hides:2][hides:32·N]
pub const C2S_GIT_LOG: u8 = 0x53;
/// List one tree level: [0x54][nonce:2][repo_id:2][oid:32][path_len:2][path:N]
pub const C2S_GIT_TREE: u8 = 0x54;
/// Fetch object bytes: [0x55][nonce:2][repo_id:2][oid:32][path_len:2][path:N][max_len:4]
pub const C2S_GIT_BLOB: u8 = 0x55;
/// File-level diff between two endpoints: [0x56][nonce:2][repo_id:2][flags:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N]
pub const C2S_GIT_DIFF: u8 = 0x56;
/// Render-ready patch rows: [0x57][nonce:2][repo_id:2][flags:1][context:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N][max_len:4]
pub const C2S_GIT_PATCH: u8 = 0x57;
/// Enumerate index entries: [0x58][nonce:2][repo_id:2][path_len:2][path:N]
pub const C2S_GIT_INDEX: u8 = 0x58;
/// Advisory cancel of an in-flight request: [0x59][nonce:2]
pub const C2S_GIT_CANCEL: u8 = 0x59;
/// Merge bases of an oid set: [0x5A][nonce:2][repo_id:2][n_oids:1][oids:32·N]
pub const C2S_GIT_BASE: u8 = 0x5A;
/// Resolve a revision spec to commit oids: [0x5B][nonce:2][repo_id:2][spec_len:2][spec:N]
/// `spec` is any git revision expression — a ref name, (short) oid,
/// `HEAD~3`, or a range `A..B` / `A...B`. The response gives `tips`/`hides`
/// ready to feed [`msg_git_log`].
pub const C2S_GIT_RESOLVE: u8 = 0x5B;
/// Subscribe to a live log of a spec: [0x5C][log_id:2][repo_id:2][flags:1][limit:2][spec_len:2][spec:N]
/// The server resolves `spec` and pushes a `GIT_LOG_PAGE`, re-emitting
/// whenever the resolved endpoints move (a ref the spec names changes).
/// `log_id` is a client-assigned subscription id (unique per connection).
/// `flags` are the same `GIT_LOG_*` bits.
pub const C2S_GIT_LOG_WATCH: u8 = 0x5C;
/// End a log subscription: [0x5D][log_id:2][repo_id:2]
pub const C2S_GIT_LOG_UNWATCH: u8 = 0x5D;
/// Acknowledge a log page (coalescing pacing): [0x5E][log_id:2][repo_id:2][update_id:4]
pub const C2S_GIT_LOG_ACK: u8 = 0x5E;

// S2C opcodes.

/// Open outcome: [0x50][nonce:2][repo_id:2][status:1][oid_format:1][flags:1][workdir_len:2][workdir:N][gitdir_len:2][gitdir:N]
/// On failure `repo_id` = [`GIT_REPO_ID_INVALID`] and `workdir` carries a
/// diagnostic; on success both paths are canonical, escaped.
pub const S2C_GIT_REPO: u8 = 0x50;
/// Whole-state snapshot: [0x51][repo_id:2][state_id:4][flags:1][records:LZ4]
pub const S2C_GIT_STATE: u8 = 0x51;
/// Repo ended server-side: [0x52][repo_id:2][reason:1]
pub const S2C_GIT_CLOSED: u8 = 0x52;
/// Log response: [0x53][nonce:2][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4]
pub const S2C_GIT_COMMITS: u8 = 0x53;
/// Tree response: [0x54][nonce:2][status:1][flags:1][records:LZ4]
pub const S2C_GIT_TREE: u8 = 0x54;
/// Blob response: [0x55][nonce:2][status:1][size:8][data:LZ4]
/// `size` is always the true object size, even on `TOO_LARGE`.
pub const S2C_GIT_BLOB: u8 = 0x55;
/// Diff response: [0x56][nonce:2][status:1][flags:1][records:LZ4]
pub const S2C_GIT_DIFF: u8 = 0x56;
/// Patch response: [0x57][nonce:2][status:1][flags:1][data:LZ4]
/// `data` is records when `STRUCTURED`, else a classic unified diff.
pub const S2C_GIT_PATCH: u8 = 0x57;
/// Index response: [0x58][nonce:2][status:1][flags:1][records:LZ4]
pub const S2C_GIT_INDEX: u8 = 0x58;
/// Merge-base response: [0x5A][nonce:2][status:1][n_bases:1][bases:32·N]
pub const S2C_GIT_BASE: u8 = 0x5A;
/// Resolve response: [0x5B][nonce:2][status:1][n_tips:2][tips:32·N][n_hides:2][hides:32·N]
pub const S2C_GIT_RESOLVE: u8 = 0x5B;
/// Live log page: [0x5C][log_id:2][update_id:4][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4]
/// Same records as `GIT_COMMITS`; re-sent (coalesced, acked) when the
/// subscription's resolved endpoints move. `flags` bit 0 `MORE` marks a
/// truncated head page — pull older history statelessly with `GIT_LOG`
/// from `frontier`.
pub const S2C_GIT_LOG_PAGE: u8 = 0x5C;

// Unified status table: every `status` byte in the family (docs/git.md
// "Statuses"). Codes 0-4 coincide with `FS_SYNCED`'s where semantics overlap.
pub const GIT_STATUS_OK: u8 = 0;
/// `repo_id` unknown or already closed.
pub const GIT_STATUS_UNKNOWN_ID: u8 = 1;
/// Path or object does not exist.
pub const GIT_STATUS_NOT_FOUND: u8 = 2;
/// Object is not what the request requires.
pub const GIT_STATUS_WRONG_TYPE: u8 = 3;
pub const GIT_STATUS_PERMISSION: u8 = 4;
/// Over `max_len` or a size cap; size fields still carry truth.
pub const GIT_STATUS_TOO_LARGE: u8 = 5;
/// A budget was exhausted with no way to paginate or truncate.
pub const GIT_STATUS_BUDGET: u8 = 6;
/// Malformed request (unknown flags, bad endpoint combination).
pub const GIT_STATUS_INVALID: u8 = 7;
/// Ended by `GIT_CANCEL`.
pub const GIT_STATUS_CANCELLED: u8 = 8;
/// Diagnostic in the message's detail field where it has one.
pub const GIT_STATUS_OTHER: u8 = 9;

/// Human-readable name for a `GIT_STATUS_*` code.
pub fn git_status_text(status: u8) -> &'static str {
    match status {
        GIT_STATUS_OK => "ok",
        GIT_STATUS_UNKNOWN_ID => "unknown repo",
        GIT_STATUS_NOT_FOUND => "not found",
        GIT_STATUS_WRONG_TYPE => "wrong object type",
        GIT_STATUS_PERMISSION => "permission denied",
        GIT_STATUS_TOO_LARGE => "too large",
        GIT_STATUS_BUDGET => "budget exhausted",
        GIT_STATUS_INVALID => "invalid request",
        GIT_STATUS_CANCELLED => "cancelled",
        _ => "error",
    }
}

// C2S_GIT_OPEN flags.
/// Stream `GIT_STATE`.
pub const GIT_OPEN_WATCH: u8 = 1 << 0;
/// Include index/worktree status records in state; implies `WATCH`.
pub const GIT_OPEN_STATUS: u8 = 1 << 1;
/// Status includes untracked files.
pub const GIT_OPEN_UNTRACKED: u8 = 1 << 2;
/// Status includes ignored files; implies `UNTRACKED`.
pub const GIT_OPEN_IGNORED: u8 = 1 << 3;
/// Include per-branch upstream records in state; implies `WATCH`.
pub const GIT_OPEN_TRACKING: u8 = 1 << 4;

/// `repo_id` reported by a failed `GIT_REPO`.
pub const GIT_REPO_ID_INVALID: u16 = 0xFFFF;

// S2C_GIT_REPO oid_format: the repository hash width; oids on the wire are
// always 32 bytes, zero-padded past it.
/// SHA-1: 20 bytes used.
pub const GIT_OID_FORMAT_SHA1: u8 = 0;
/// SHA-256: all 32 bytes used.
pub const GIT_OID_FORMAT_SHA256: u8 = 1;

// S2C_GIT_REPO flags.
pub const GIT_REPO_BARE: u8 = 1 << 0;
pub const GIT_REPO_SHALLOW: u8 = 1 << 1;
/// Sparse-checkout active.
pub const GIT_REPO_SPARSE: u8 = 1 << 2;
/// Linked worktree.
pub const GIT_REPO_LINKED: u8 = 1 << 3;

// S2C_GIT_CLOSED reasons.
pub const GIT_CLOSED_CLIENT_REQUEST: u8 = 0;
pub const GIT_CLOSED_REPO_GONE: u8 = 1;
pub const GIT_CLOSED_PERMISSION_LOST: u8 = 2;
pub const GIT_CLOSED_BACKEND_FAILED: u8 = 3;
pub const GIT_CLOSED_RESOURCE_LIMIT: u8 = 4;

// S2C_GIT_STATE flags: entry budget hit; counts accurate up to the cap.
pub const GIT_STATE_REFS_TRUNCATED: u8 = 1 << 0;
pub const GIT_STATE_STATUS_TRUNCATED: u8 = 1 << 1;

// C2S_GIT_LOG flags.
pub const GIT_LOG_FIRST_PARENT: u8 = 1 << 0;
/// Topological order; default committer-date.
pub const GIT_LOG_TOPO: u8 = 1 << 1;
/// Full commit message; default first line only.
pub const GIT_LOG_FULL_MESSAGE: u8 = 1 << 2;
/// `path` must name a single file; the walk tracks it across renames.
pub const GIT_LOG_FOLLOW: u8 = 1 << 3;
/// After each commit, emit the object at the rename-adjusted `path`.
pub const GIT_LOG_PATH_OIDS: u8 = 1 << 4;

// S2C_GIT_COMMITS flags.
/// Partial page; continue with `tips = frontier` and the same `hides`.
pub const GIT_COMMITS_MORE: u8 = 1 << 0;

// C2S_GIT_DIFF request flags; C2S_GIT_PATCH shares bits 0-4.
/// Rename/copy detection.
pub const GIT_DIFF_RENAMES: u8 = 1 << 0;
/// Worktree endpoint reports untracked files as additions.
pub const GIT_DIFF_UNTRACKED: u8 = 1 << 1;
pub const GIT_DIFF_IGNORED: u8 = 1 << 2;
/// Runs of whitespace compare equal, trailing whitespace ignored (git `-b`).
pub const GIT_DIFF_IGNORE_SPACE_CHANGE: u8 = 1 << 3;
/// Whitespace ignored entirely (git `-w`).
pub const GIT_DIFF_IGNORE_ALL_SPACE: u8 = 1 << 4;

// C2S_GIT_PATCH-only request flags.
/// Classic unified diff as raw `data` instead of records.
pub const GIT_PATCH_TEXT: u8 = 1 << 5;
/// Character-granularity spans instead of the default word granularity.
pub const GIT_PATCH_CHAR_SPANS: u8 = 1 << 6;
/// Skip intraline refinement entirely, for whole-line renderers.
pub const GIT_PATCH_NO_SPANS: u8 = 1 << 7;

// Response flags: S2C_GIT_TREE / S2C_GIT_DIFF / S2C_GIT_INDEX bit 0 is the
// entry-budget truncation marker; S2C_GIT_PATCH uses bit 0 for payload form.
pub const GIT_TREE_TRUNCATED: u8 = 1 << 0;
pub const GIT_DIFF_TRUNCATED: u8 = 1 << 0;
pub const GIT_INDEX_TRUNCATED: u8 = 1 << 0;
/// `data` is records (the default); clear = classic unified diff text.
pub const GIT_PATCH_STRUCTURED: u8 = 1 << 0;
pub const GIT_PATCH_TRUNCATED: u8 = 1 << 1;

// Diff/patch endpoint kinds (docs/git.md "GIT_DIFF").
pub const GIT_ENDPOINT_EMPTY: u8 = 0;
pub const GIT_ENDPOINT_COMMIT: u8 = 1;
pub const GIT_ENDPOINT_TREE: u8 = 2;
pub const GIT_ENDPOINT_INDEX: u8 = 3;
pub const GIT_ENDPOINT_WORKTREE: u8 = 4;
/// Old side only: the server substitutes `merge-base(oid, new)`.
pub const GIT_ENDPOINT_MERGE_BASE: u8 = 5;

// Record kinds inside GIT_STATE.
pub const GIT_STATE_RECORD_HEAD: u8 = 0x01;
pub const GIT_STATE_RECORD_REF: u8 = 0x02;
pub const GIT_STATE_RECORD_OP: u8 = 0x03;
pub const GIT_STATE_RECORD_STATUS: u8 = 0x04;
pub const GIT_STATE_RECORD_UPSTREAM: u8 = 0x05;
pub const GIT_STATE_RECORD_STASH: u8 = 0x06;

// HEAD record flags.
pub const GIT_HEAD_DETACHED: u8 = 1 << 0;
pub const GIT_HEAD_UNBORN: u8 = 1 << 1;

// STATE_REF record flags.
/// `peeled` is valid (annotated tag).
pub const GIT_REF_PEELED_VALID: u8 = 1 << 0;
pub const GIT_REF_SYMBOLIC: u8 = 1 << 1;

// OP record operations.
pub const GIT_OP_MERGE: u8 = 1;
pub const GIT_OP_REBASE: u8 = 2;
pub const GIT_OP_CHERRY_PICK: u8 = 3;
pub const GIT_OP_REVERT: u8 = 4;
pub const GIT_OP_BISECT: u8 = 5;

// STATUS record flags.
pub const GIT_STATUS_ENTRY_CONFLICTED: u8 = 1 << 0;

// UPSTREAM record flags.
/// Upstream configured but its ref is missing; counts zero.
pub const GIT_UPSTREAM_GONE: u8 = 1 << 0;
/// Unset when the walk budget was hit; names still valid.
pub const GIT_UPSTREAM_COUNTS_VALID: u8 = 1 << 1;

// Record kinds inside GIT_COMMITS.
pub const GIT_COMMIT_RECORD_COMMIT: u8 = 0x01;
pub const GIT_COMMIT_RECORD_PATH_AT: u8 = 0x02;

// COMMIT record flags.
/// Bytes were replaced re-encoding name/email/message to UTF-8.
pub const GIT_COMMIT_LOSSY_ENCODING: u8 = 1 << 0;

// Record kind inside the GIT_TREE response.
pub const GIT_TREE_RECORD_ENTRY: u8 = 0x02;

// TREE_ENTRY / PATH_AT object types.
/// Submodule: the entry's oid is a commit.
pub const GIT_OTYPE_COMMIT: u8 = 1;
pub const GIT_OTYPE_TREE: u8 = 2;
pub const GIT_OTYPE_BLOB: u8 = 3;

// Record kinds inside the GIT_DIFF response.
pub const GIT_DIFF_RECORD_ENTRY: u8 = 0x03;
pub const GIT_DIFF_RECORD_BASE: u8 = 0x04;

// DIFF_ENTRY dflags.
pub const GIT_DIFF_ENTRY_BINARY: u8 = 1 << 0;
pub const GIT_DIFF_ENTRY_SUBMODULE: u8 = 1 << 1;

// Record kinds inside the GIT_PATCH response (structured form).
pub const GIT_PATCH_RECORD_FILE: u8 = 0x01;
pub const GIT_PATCH_RECORD_ROW: u8 = 0x02;
pub const GIT_PATCH_RECORD_GAP: u8 = 0x03;
pub const GIT_PATCH_RECORD_BASE: u8 = 0x04;

// PATCH_FILE flags.
/// Binary file: no rows follow.
pub const GIT_PATCH_FILE_BINARY: u8 = 1 << 0;

// Record kind inside the GIT_INDEX response.
pub const GIT_INDEX_RECORD_ENTRY: u8 = 0x04;

// INDEX_ENTRY iflags.
pub const GIT_INDEX_INTENT_TO_ADD: u8 = 1 << 0;
pub const GIT_INDEX_SKIP_WORKTREE: u8 = 1 << 1;

/// An object id: always 32 bytes on the wire, zero-padded past the
/// repository's hash width (`GIT_REPO.oid_format`).
pub type GitOid = [u8; 32];

/// The all-zero oid: absent (unborn branch, unhashed worktree side,
/// deleted side of a diff).
pub const GIT_OID_NONE: GitOid = [0; 32];

/// One side of a `GIT_DIFF`/`GIT_PATCH`: `[kind:1][oid:32]`. The oid is
/// meaningful only for `COMMIT`, `TREE`, and `MERGE_BASE` kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitEndpoint {
    pub kind: u8,
    pub oid: GitOid,
}

/// Decompress a `compress_prepend_size` payload, refusing declared sizes
/// over the protocol-wide [`crate::MAX_DECOMPRESSED`] *before* allocating
/// (docs/protocol.md "Compressed payloads").
fn decompress_guarded(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }
    let declared = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if declared > crate::MAX_DECOMPRESSED {
        return None;
    }
    lz4_flex::decompress_size_prepended(data).ok()
}

// ---------------------------------------------------------------------------
// Field codec helpers
// ---------------------------------------------------------------------------

fn push_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    buf.extend_from_slice(&(b.len() as u16).to_le_bytes());
    buf.extend_from_slice(b);
}

/// A u32-length-prefixed byte string (patch row text, commit messages).
fn push_bytes(buf: &mut Vec<u8>, b: &[u8]) {
    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
    buf.extend_from_slice(b);
}

fn push_oids(buf: &mut Vec<u8>, oids: &[GitOid]) {
    for oid in oids {
        buf.extend_from_slice(oid);
    }
}

fn take_u8(b: &mut &[u8]) -> Option<u8> {
    let (&x, rest) = b.split_first()?;
    *b = rest;
    Some(x)
}

fn take_u16(b: &mut &[u8]) -> Option<u16> {
    if b.len() < 2 {
        return None;
    }
    let v = u16::from_le_bytes([b[0], b[1]]);
    *b = &b[2..];
    Some(v)
}

fn take_i16(b: &mut &[u8]) -> Option<i16> {
    Some(take_u16(b)? as i16)
}

fn take_u32(b: &mut &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    let v = u32::from_le_bytes(b[0..4].try_into().unwrap());
    *b = &b[4..];
    Some(v)
}

fn take_u64(b: &mut &[u8]) -> Option<u64> {
    if b.len() < 8 {
        return None;
    }
    let v = u64::from_le_bytes(b[0..8].try_into().unwrap());
    *b = &b[8..];
    Some(v)
}

fn take_i64(b: &mut &[u8]) -> Option<i64> {
    Some(take_u64(b)? as i64)
}

fn take_oid(b: &mut &[u8]) -> Option<GitOid> {
    if b.len() < 32 {
        return None;
    }
    let oid: GitOid = b[0..32].try_into().unwrap();
    *b = &b[32..];
    Some(oid)
}

fn take_oids(b: &mut &[u8], n: usize) -> Option<Vec<GitOid>> {
    if b.len() < n * 32 {
        return None;
    }
    let mut oids = Vec::with_capacity(n);
    for _ in 0..n {
        oids.push(take_oid(b)?);
    }
    Some(oids)
}

fn take_str<'a>(b: &mut &'a [u8]) -> Option<&'a str> {
    let len = take_u16(b)? as usize;
    if b.len() < len {
        return None;
    }
    let s = std::str::from_utf8(&b[..len]).ok()?;
    *b = &b[len..];
    Some(s)
}

fn take_bytes<'a>(b: &mut &'a [u8]) -> Option<&'a [u8]> {
    let len = take_u32(b)? as usize;
    if b.len() < len {
        return None;
    }
    let bytes = &b[..len];
    *b = &b[len..];
    Some(bytes)
}

fn take_endpoint(b: &mut &[u8]) -> Option<GitEndpoint> {
    let kind = take_u8(b)?;
    let oid = take_oid(b)?;
    Some(GitEndpoint { kind, oid })
}

fn push_endpoint(buf: &mut Vec<u8>, endpoint: GitEndpoint) {
    buf.push(endpoint.kind);
    buf.extend_from_slice(&endpoint.oid);
}

/// Check `msg` starts with `opcode` and return the body after it.
fn body_of(msg: &[u8], opcode: u8) -> Option<&[u8]> {
    if msg.first() != Some(&opcode) {
        return None;
    }
    Some(&msg[1..])
}

// ---------------------------------------------------------------------------
// C2S message builders and parsers
// ---------------------------------------------------------------------------

pub fn msg_git_open(
    nonce: u16,
    flags: u8,
    refs_latency_ms: u16,
    status_latency_ms: u16,
    path: &str,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(10 + path.len());
    msg.push(C2S_GIT_OPEN);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&refs_latency_ms.to_le_bytes());
    msg.extend_from_slice(&status_latency_ms.to_le_bytes());
    push_str(&mut msg, path);
    msg
}

/// Parse `C2S_GIT_OPEN` into `(nonce, flags, refs_latency_ms, status_latency_ms, path)`.
pub fn parse_git_open(msg: &[u8]) -> Option<(u16, u8, u16, u16, &str)> {
    let mut b = body_of(msg, C2S_GIT_OPEN)?;
    let nonce = take_u16(&mut b)?;
    let flags = take_u8(&mut b)?;
    let refs_latency_ms = take_u16(&mut b)?;
    let status_latency_ms = take_u16(&mut b)?;
    let path = take_str(&mut b)?;
    Some((nonce, flags, refs_latency_ms, status_latency_ms, path))
}

pub fn msg_git_close(repo_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_GIT_CLOSE);
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg
}

pub fn parse_git_close(msg: &[u8]) -> Option<u16> {
    let mut b = body_of(msg, C2S_GIT_CLOSE)?;
    take_u16(&mut b)
}

pub fn msg_git_ack(repo_id: u16, state_id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_GIT_ACK);
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.extend_from_slice(&state_id.to_le_bytes());
    msg
}

/// Parse `C2S_GIT_ACK` into `(repo_id, state_id)`.
pub fn parse_git_ack(msg: &[u8]) -> Option<(u16, u32)> {
    let mut b = body_of(msg, C2S_GIT_ACK)?;
    let repo_id = take_u16(&mut b)?;
    let state_id = take_u32(&mut b)?;
    Some((repo_id, state_id))
}

/// A decoded `C2S_GIT_LOG` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitLogRequest<'a> {
    pub nonce: u16,
    pub repo_id: u16,
    pub flags: u8,
    /// `0` = server default; clamped to the server maximum.
    pub limit: u16,
    /// Empty = no path filter; escaped form.
    pub path: &'a str,
    /// Empty = HEAD.
    pub tips: Vec<GitOid>,
    pub hides: Vec<GitOid>,
}

pub fn msg_git_log(
    nonce: u16,
    repo_id: u16,
    flags: u8,
    limit: u16,
    path: &str,
    tips: &[GitOid],
    hides: &[GitOid],
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(14 + path.len() + 32 * (tips.len() + hides.len()));
    msg.push(C2S_GIT_LOG);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&limit.to_le_bytes());
    push_str(&mut msg, path);
    msg.extend_from_slice(&(tips.len() as u16).to_le_bytes());
    push_oids(&mut msg, tips);
    msg.extend_from_slice(&(hides.len() as u16).to_le_bytes());
    push_oids(&mut msg, hides);
    msg
}

pub fn parse_git_log(msg: &[u8]) -> Option<GitLogRequest<'_>> {
    let mut b = body_of(msg, C2S_GIT_LOG)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let flags = take_u8(&mut b)?;
    let limit = take_u16(&mut b)?;
    let path = take_str(&mut b)?;
    let n_tips = take_u16(&mut b)? as usize;
    let tips = take_oids(&mut b, n_tips)?;
    let n_hides = take_u16(&mut b)? as usize;
    let hides = take_oids(&mut b, n_hides)?;
    Some(GitLogRequest {
        nonce,
        repo_id,
        flags,
        limit,
        path,
        tips,
        hides,
    })
}

pub fn msg_git_tree(nonce: u16, repo_id: u16, oid: &GitOid, path: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(39 + path.len());
    msg.push(C2S_GIT_TREE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.extend_from_slice(oid);
    push_str(&mut msg, path);
    msg
}

/// Parse `C2S_GIT_TREE` into `(nonce, repo_id, oid, path)`.
pub fn parse_git_tree(msg: &[u8]) -> Option<(u16, u16, GitOid, &str)> {
    let mut b = body_of(msg, C2S_GIT_TREE)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let oid = take_oid(&mut b)?;
    let path = take_str(&mut b)?;
    Some((nonce, repo_id, oid, path))
}

pub fn msg_git_blob(nonce: u16, repo_id: u16, oid: &GitOid, path: &str, max_len: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(43 + path.len());
    msg.push(C2S_GIT_BLOB);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.extend_from_slice(oid);
    push_str(&mut msg, path);
    msg.extend_from_slice(&max_len.to_le_bytes());
    msg
}

/// Parse `C2S_GIT_BLOB` into `(nonce, repo_id, oid, path, max_len)`.
pub fn parse_git_blob(msg: &[u8]) -> Option<(u16, u16, GitOid, &str, u32)> {
    let mut b = body_of(msg, C2S_GIT_BLOB)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let oid = take_oid(&mut b)?;
    let path = take_str(&mut b)?;
    let max_len = take_u32(&mut b)?;
    Some((nonce, repo_id, oid, path, max_len))
}

/// A decoded `C2S_GIT_DIFF` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitDiffRequest<'a> {
    pub nonce: u16,
    pub repo_id: u16,
    pub flags: u8,
    pub old: GitEndpoint,
    pub new: GitEndpoint,
    /// Empty = whole tree; escaped form.
    pub path: &'a str,
}

pub fn msg_git_diff(
    nonce: u16,
    repo_id: u16,
    flags: u8,
    old: GitEndpoint,
    new: GitEndpoint,
    path: &str,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(74 + path.len());
    msg.push(C2S_GIT_DIFF);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(flags);
    push_endpoint(&mut msg, old);
    push_endpoint(&mut msg, new);
    push_str(&mut msg, path);
    msg
}

pub fn parse_git_diff(msg: &[u8]) -> Option<GitDiffRequest<'_>> {
    let mut b = body_of(msg, C2S_GIT_DIFF)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let flags = take_u8(&mut b)?;
    let old = take_endpoint(&mut b)?;
    let new = take_endpoint(&mut b)?;
    let path = take_str(&mut b)?;
    Some(GitDiffRequest {
        nonce,
        repo_id,
        flags,
        old,
        new,
        path,
    })
}

/// A decoded `C2S_GIT_PATCH` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPatchRequest<'a> {
    pub nonce: u16,
    pub repo_id: u16,
    pub flags: u8,
    /// Context lines; `0` = server default (3).
    pub context: u8,
    pub old: GitEndpoint,
    pub new: GitEndpoint,
    /// Non-empty = one file's patch; empty = the whole diff.
    pub path: &'a str,
    pub max_len: u32,
}

#[allow(clippy::too_many_arguments)] // mirrors the wire layout field-for-field
pub fn msg_git_patch(
    nonce: u16,
    repo_id: u16,
    flags: u8,
    context: u8,
    old: GitEndpoint,
    new: GitEndpoint,
    path: &str,
    max_len: u32,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(79 + path.len());
    msg.push(C2S_GIT_PATCH);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(flags);
    msg.push(context);
    push_endpoint(&mut msg, old);
    push_endpoint(&mut msg, new);
    push_str(&mut msg, path);
    msg.extend_from_slice(&max_len.to_le_bytes());
    msg
}

pub fn parse_git_patch(msg: &[u8]) -> Option<GitPatchRequest<'_>> {
    let mut b = body_of(msg, C2S_GIT_PATCH)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let flags = take_u8(&mut b)?;
    let context = take_u8(&mut b)?;
    let old = take_endpoint(&mut b)?;
    let new = take_endpoint(&mut b)?;
    let path = take_str(&mut b)?;
    let max_len = take_u32(&mut b)?;
    Some(GitPatchRequest {
        nonce,
        repo_id,
        flags,
        context,
        old,
        new,
        path,
        max_len,
    })
}

pub fn msg_git_index(nonce: u16, repo_id: u16, path: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7 + path.len());
    msg.push(C2S_GIT_INDEX);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    push_str(&mut msg, path);
    msg
}

/// Parse `C2S_GIT_INDEX` into `(nonce, repo_id, path)`.
pub fn parse_git_index(msg: &[u8]) -> Option<(u16, u16, &str)> {
    let mut b = body_of(msg, C2S_GIT_INDEX)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let path = take_str(&mut b)?;
    Some((nonce, repo_id, path))
}

pub fn msg_git_cancel(nonce: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_GIT_CANCEL);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg
}

pub fn parse_git_cancel(msg: &[u8]) -> Option<u16> {
    let mut b = body_of(msg, C2S_GIT_CANCEL)?;
    take_u16(&mut b)
}

pub fn msg_git_base(nonce: u16, repo_id: u16, oids: &[GitOid]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(6 + 32 * oids.len());
    msg.push(C2S_GIT_BASE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(oids.len() as u8);
    push_oids(&mut msg, oids);
    msg
}

/// Parse `C2S_GIT_BASE` into `(nonce, repo_id, oids)`.
pub fn parse_git_base(msg: &[u8]) -> Option<(u16, u16, Vec<GitOid>)> {
    let mut b = body_of(msg, C2S_GIT_BASE)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let n = take_u8(&mut b)? as usize;
    let oids = take_oids(&mut b, n)?;
    Some((nonce, repo_id, oids))
}

pub fn msg_git_resolve(nonce: u16, repo_id: u16, spec: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7 + spec.len());
    msg.push(C2S_GIT_RESOLVE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    push_str(&mut msg, spec);
    msg
}

/// Parse `C2S_GIT_RESOLVE` into `(nonce, repo_id, spec)`.
pub fn parse_git_resolve(msg: &[u8]) -> Option<(u16, u16, &str)> {
    let mut b = body_of(msg, C2S_GIT_RESOLVE)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let spec = take_str(&mut b)?;
    Some((nonce, repo_id, spec))
}

pub fn msg_git_log_watch(log_id: u16, repo_id: u16, flags: u8, limit: u16, spec: &str) -> Vec<u8> {
    let mut msg = Vec::with_capacity(10 + spec.len());
    msg.push(C2S_GIT_LOG_WATCH);
    msg.extend_from_slice(&log_id.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&limit.to_le_bytes());
    push_str(&mut msg, spec);
    msg
}

/// Parse `C2S_GIT_LOG_WATCH` into `(log_id, repo_id, flags, limit, spec)`.
pub fn parse_git_log_watch(msg: &[u8]) -> Option<(u16, u16, u8, u16, &str)> {
    let mut b = body_of(msg, C2S_GIT_LOG_WATCH)?;
    let log_id = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let flags = take_u8(&mut b)?;
    let limit = take_u16(&mut b)?;
    let spec = take_str(&mut b)?;
    Some((log_id, repo_id, flags, limit, spec))
}

pub fn msg_git_log_unwatch(log_id: u16, repo_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(C2S_GIT_LOG_UNWATCH);
    msg.extend_from_slice(&log_id.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg
}

/// Parse `C2S_GIT_LOG_UNWATCH` into `(log_id, repo_id)`.
pub fn parse_git_log_unwatch(msg: &[u8]) -> Option<(u16, u16)> {
    let mut b = body_of(msg, C2S_GIT_LOG_UNWATCH)?;
    let log_id = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    Some((log_id, repo_id))
}

pub fn msg_git_log_ack(log_id: u16, repo_id: u16, update_id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(9);
    msg.push(C2S_GIT_LOG_ACK);
    msg.extend_from_slice(&log_id.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg
}

/// Parse `C2S_GIT_LOG_ACK` into `(log_id, repo_id, update_id)`.
pub fn parse_git_log_ack(msg: &[u8]) -> Option<(u16, u16, u32)> {
    let mut b = body_of(msg, C2S_GIT_LOG_ACK)?;
    let log_id = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let update_id = take_u32(&mut b)?;
    Some((log_id, repo_id, update_id))
}

// ---------------------------------------------------------------------------
// S2C message builders and parsers
// ---------------------------------------------------------------------------

/// A decoded `S2C_GIT_REPO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRepoInfo<'a> {
    pub nonce: u16,
    pub repo_id: u16,
    pub status: u8,
    pub oid_format: u8,
    pub flags: u8,
    /// Canonical worktree root (empty for bare); a diagnostic on failure.
    pub workdir: &'a str,
    pub gitdir: &'a str,
}

pub fn msg_git_repo(
    nonce: u16,
    repo_id: u16,
    status: u8,
    oid_format: u8,
    flags: u8,
    workdir: &str,
    gitdir: &str,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(12 + workdir.len() + gitdir.len());
    msg.push(S2C_GIT_REPO);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(status);
    msg.push(oid_format);
    msg.push(flags);
    push_str(&mut msg, workdir);
    push_str(&mut msg, gitdir);
    msg
}

pub fn parse_git_repo(msg: &[u8]) -> Option<GitRepoInfo<'_>> {
    let mut b = body_of(msg, S2C_GIT_REPO)?;
    let nonce = take_u16(&mut b)?;
    let repo_id = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let oid_format = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let workdir = take_str(&mut b)?;
    let gitdir = take_str(&mut b)?;
    Some(GitRepoInfo {
        nonce,
        repo_id,
        status,
        oid_format,
        flags,
        workdir,
        gitdir,
    })
}

/// Build a `GIT_STATE` from an uncompressed records buffer.
pub fn msg_git_state(repo_id: u16, state_id: u32, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(8 + compressed.len());
    msg.push(S2C_GIT_STATE);
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.extend_from_slice(&state_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse `S2C_GIT_STATE` into `(repo_id, state_id, flags, records)`,
/// decompressed under the standard guard.
pub fn parse_git_state(msg: &[u8]) -> Option<(u16, u32, u8, Vec<u8>)> {
    let mut b = body_of(msg, S2C_GIT_STATE)?;
    let repo_id = take_u16(&mut b)?;
    let state_id = take_u32(&mut b)?;
    let flags = take_u8(&mut b)?;
    let records = decompress_guarded(b)?;
    Some((repo_id, state_id, flags, records))
}

pub fn msg_git_closed(repo_id: u16, reason: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(S2C_GIT_CLOSED);
    msg.extend_from_slice(&repo_id.to_le_bytes());
    msg.push(reason);
    msg
}

/// Parse `S2C_GIT_CLOSED` into `(repo_id, reason)`.
pub fn parse_git_closed(msg: &[u8]) -> Option<(u16, u8)> {
    let mut b = body_of(msg, S2C_GIT_CLOSED)?;
    let repo_id = take_u16(&mut b)?;
    let reason = take_u8(&mut b)?;
    Some((repo_id, reason))
}

/// Build a `GIT_COMMITS` from an uncompressed records buffer.
pub fn msg_git_commits(
    nonce: u16,
    status: u8,
    flags: u8,
    frontier: &[GitOid],
    records: &[u8],
) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(7 + 32 * frontier.len() + compressed.len());
    msg.push(S2C_GIT_COMMITS);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    msg.extend_from_slice(&(frontier.len() as u16).to_le_bytes());
    push_oids(&mut msg, frontier);
    msg.extend_from_slice(&compressed);
    msg
}

/// A decoded `S2C_GIT_COMMITS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommitsPage {
    pub nonce: u16,
    pub status: u8,
    pub flags: u8,
    /// The walk's pending boundary when `MORE` is set: re-issue `GIT_LOG`
    /// with `tips = frontier` and the same `hides` to continue.
    pub frontier: Vec<GitOid>,
    /// Decompressed records buffer; iterate with [`git_commit_records`].
    pub records: Vec<u8>,
}

pub fn parse_git_commits(msg: &[u8]) -> Option<GitCommitsPage> {
    let mut b = body_of(msg, S2C_GIT_COMMITS)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let n = take_u16(&mut b)? as usize;
    let frontier = take_oids(&mut b, n)?;
    let records = decompress_guarded(b)?;
    Some(GitCommitsPage {
        nonce,
        status,
        flags,
        frontier,
        records,
    })
}

/// Build the `[nonce:2][status:1][flags:1][payload:LZ4]` response shape
/// shared by the tree, diff, patch, and index responses.
fn msg_nonce_status_flags_lz4(
    opcode: u8,
    nonce: u16,
    status: u8,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(payload);
    let mut msg = Vec::with_capacity(5 + compressed.len());
    msg.push(opcode);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse the shared `[nonce:2][status:1][flags:1][payload:LZ4]` shape.
fn parse_nonce_status_flags_lz4(msg: &[u8], opcode: u8) -> Option<(u16, u8, u8, Vec<u8>)> {
    let mut b = body_of(msg, opcode)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let payload = decompress_guarded(b)?;
    Some((nonce, status, flags, payload))
}

/// Build a `GIT_TREE` response from an uncompressed records buffer.
pub fn msg_git_tree_resp(nonce: u16, status: u8, flags: u8, records: &[u8]) -> Vec<u8> {
    msg_nonce_status_flags_lz4(S2C_GIT_TREE, nonce, status, flags, records)
}

/// Parse an `S2C_GIT_TREE` into `(nonce, status, flags, records)`.
pub fn parse_git_tree_resp(msg: &[u8]) -> Option<(u16, u8, u8, Vec<u8>)> {
    parse_nonce_status_flags_lz4(msg, S2C_GIT_TREE)
}

/// Build a `GIT_BLOB` response; `size` is the true object size, `data` the
/// (possibly truncated to nothing on error) raw object bytes.
pub fn msg_git_blob_resp(nonce: u16, status: u8, size: u64, data: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(data);
    let mut msg = Vec::with_capacity(12 + compressed.len());
    msg.push(S2C_GIT_BLOB);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&size.to_le_bytes());
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse an `S2C_GIT_BLOB` into `(nonce, status, size, data)`.
pub fn parse_git_blob_resp(msg: &[u8]) -> Option<(u16, u8, u64, Vec<u8>)> {
    let mut b = body_of(msg, S2C_GIT_BLOB)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let size = take_u64(&mut b)?;
    let data = decompress_guarded(b)?;
    Some((nonce, status, size, data))
}

/// Build a `GIT_DIFF` response from an uncompressed records buffer.
pub fn msg_git_diff_resp(nonce: u16, status: u8, flags: u8, records: &[u8]) -> Vec<u8> {
    msg_nonce_status_flags_lz4(S2C_GIT_DIFF, nonce, status, flags, records)
}

/// Parse an `S2C_GIT_DIFF` into `(nonce, status, flags, records)`.
pub fn parse_git_diff_resp(msg: &[u8]) -> Option<(u16, u8, u8, Vec<u8>)> {
    parse_nonce_status_flags_lz4(msg, S2C_GIT_DIFF)
}

/// Build a `GIT_PATCH` response. `data` is an uncompressed records buffer
/// when `flags` has [`GIT_PATCH_STRUCTURED`], else unified-diff text.
pub fn msg_git_patch_resp(nonce: u16, status: u8, flags: u8, data: &[u8]) -> Vec<u8> {
    msg_nonce_status_flags_lz4(S2C_GIT_PATCH, nonce, status, flags, data)
}

/// Parse an `S2C_GIT_PATCH` into `(nonce, status, flags, data)`.
pub fn parse_git_patch_resp(msg: &[u8]) -> Option<(u16, u8, u8, Vec<u8>)> {
    parse_nonce_status_flags_lz4(msg, S2C_GIT_PATCH)
}

/// Build a `GIT_INDEX` response from an uncompressed records buffer.
pub fn msg_git_index_resp(nonce: u16, status: u8, flags: u8, records: &[u8]) -> Vec<u8> {
    msg_nonce_status_flags_lz4(S2C_GIT_INDEX, nonce, status, flags, records)
}

/// Parse an `S2C_GIT_INDEX` into `(nonce, status, flags, records)`.
pub fn parse_git_index_resp(msg: &[u8]) -> Option<(u16, u8, u8, Vec<u8>)> {
    parse_nonce_status_flags_lz4(msg, S2C_GIT_INDEX)
}

/// Build a `GIT_BASE` response; `bases` comes best-first, empty with `OK`
/// meaning disjoint histories.
pub fn msg_git_base_resp(nonce: u16, status: u8, bases: &[GitOid]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5 + 32 * bases.len());
    msg.push(S2C_GIT_BASE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.push(bases.len() as u8);
    push_oids(&mut msg, bases);
    msg
}

/// Parse an `S2C_GIT_BASE` into `(nonce, status, bases)`.
pub fn parse_git_base_resp(msg: &[u8]) -> Option<(u16, u8, Vec<GitOid>)> {
    let mut b = body_of(msg, S2C_GIT_BASE)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let n = take_u8(&mut b)? as usize;
    let bases = take_oids(&mut b, n)?;
    Some((nonce, status, bases))
}

pub fn msg_git_resolve_resp(nonce: u16, status: u8, tips: &[GitOid], hides: &[GitOid]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7 + 32 * (tips.len() + hides.len()));
    msg.push(S2C_GIT_RESOLVE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&(tips.len() as u16).to_le_bytes());
    push_oids(&mut msg, tips);
    msg.extend_from_slice(&(hides.len() as u16).to_le_bytes());
    push_oids(&mut msg, hides);
    msg
}

/// Parse an `S2C_GIT_RESOLVE` into `(nonce, status, tips, hides)`.
pub fn parse_git_resolve_resp(msg: &[u8]) -> Option<(u16, u8, Vec<GitOid>, Vec<GitOid>)> {
    let mut b = body_of(msg, S2C_GIT_RESOLVE)?;
    let nonce = take_u16(&mut b)?;
    let status = take_u8(&mut b)?;
    let n_tips = take_u16(&mut b)? as usize;
    let tips = take_oids(&mut b, n_tips)?;
    let n_hides = take_u16(&mut b)? as usize;
    let hides = take_oids(&mut b, n_hides)?;
    Some((nonce, status, tips, hides))
}

pub fn msg_git_log_page(
    log_id: u16,
    update_id: u32,
    status: u8,
    flags: u8,
    frontier: &[GitOid],
    records: &[u8],
) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(11 + 32 * frontier.len() + compressed.len());
    msg.push(S2C_GIT_LOG_PAGE);
    msg.extend_from_slice(&log_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg.push(status);
    msg.push(flags);
    msg.extend_from_slice(&(frontier.len() as u16).to_le_bytes());
    push_oids(&mut msg, frontier);
    msg.extend_from_slice(&compressed);
    msg
}

/// A decoded `S2C_GIT_LOG_PAGE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitLogPage {
    pub log_id: u16,
    /// Acknowledge with [`msg_git_log_ack`] to receive later updates.
    pub update_id: u32,
    pub status: u8,
    pub flags: u8,
    pub frontier: Vec<GitOid>,
    pub records: Vec<u8>,
}

pub fn parse_git_log_page(msg: &[u8]) -> Option<GitLogPage> {
    let mut b = body_of(msg, S2C_GIT_LOG_PAGE)?;
    let log_id = take_u16(&mut b)?;
    let update_id = take_u32(&mut b)?;
    let status = take_u8(&mut b)?;
    let flags = take_u8(&mut b)?;
    let n = take_u16(&mut b)? as usize;
    let frontier = take_oids(&mut b, n)?;
    let records = decompress_guarded(b)?;
    Some(GitLogPage {
        log_id,
        update_id,
        status,
        flags,
        frontier,
        records,
    })
}

// ---------------------------------------------------------------------------
// Record codecs
//
// Every `records:LZ4` payload uses the fs-family framing
// (docs/fs-watch.md): `[record_len:4][kind:1][…]`, unknown kinds skipped
// via `record_len`, a malformed record ends the payload. Kinds are
// namespaced per message type.
// ---------------------------------------------------------------------------

/// Write the `record_len` placeholder; pair with [`end_record`].
fn begin_record(buf: &mut Vec<u8>) -> usize {
    let start = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes());
    start
}

fn end_record(buf: &mut [u8], start: usize) {
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Pop the next framed record as `(kind, body)`. `None` on exhaustion or
/// malformed framing.
fn next_record<'a>(data: &mut &'a [u8]) -> Option<(u8, &'a [u8])> {
    if data.len() < 4 {
        return None;
    }
    let rec_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if rec_len == 0 || data.len() < 4 + rec_len {
        return None;
    }
    let body = &data[4..4 + rec_len];
    *data = &data[4 + rec_len..];
    Some((body[0], &body[1..]))
}

/// One decoded record from a `GIT_STATE` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitStateRecord<'a> {
    /// HEAD 0x01: [kind:1][flags:1][oid:32][name_len:2][name:N]
    /// `name` is the symbolic target (empty when detached).
    Head {
        flags: u8,
        oid: GitOid,
        name: &'a str,
    },
    /// STATE_REF 0x02: [kind:1][flags:1][oid:32][peeled:32][name_len:2][name:N]
    Ref {
        flags: u8,
        oid: GitOid,
        peeled: GitOid,
        name: &'a str,
    },
    /// OP 0x03: [kind:1][op:1][oid:32][detail_len:2][detail:N]
    /// `oid` is the operation head; an absent record means no operation.
    Op {
        op: u8,
        oid: GitOid,
        detail: &'a str,
    },
    /// STATUS 0x04: [kind:1][staged:1][unstaged:1][flags:1][old_len:2][old_path:N][path_len:2][path:N]
    /// `staged`/`unstaged` are porcelain letters (ASCII ` `AMDRTU, `?`, `!`);
    /// `old_path` is non-empty only for renames.
    Status {
        staged: u8,
        unstaged: u8,
        flags: u8,
        old_path: &'a str,
        path: &'a str,
    },
    /// UPSTREAM 0x05: [kind:1][flags:1][ahead:4][behind:4][name_len:2][name:N][upstream_len:2][upstream:N]
    /// One per local branch with a configured upstream; `name` joins
    /// `Ref` records by ref name.
    Upstream {
        flags: u8,
        ahead: u32,
        behind: u32,
        name: &'a str,
        upstream: &'a str,
    },
    /// STASH 0x06: [kind:1][index:2][oid:32][time:8 i64 s][tz:2 i16 min][msg_len:2][msg:N]
    /// `index` is the N of `stash@{N}`, `oid` the stash commit.
    Stash {
        index: u16,
        oid: GitOid,
        time: i64,
        tz: i16,
        msg: &'a str,
    },
}

/// Append one record to an uncompressed `GIT_STATE` records buffer.
pub fn append_git_state_record(buf: &mut Vec<u8>, record: &GitStateRecord<'_>) {
    let start = begin_record(buf);
    match record {
        GitStateRecord::Head { flags, oid, name } => {
            buf.push(GIT_STATE_RECORD_HEAD);
            buf.push(*flags);
            buf.extend_from_slice(oid);
            push_str(buf, name);
        }
        GitStateRecord::Ref {
            flags,
            oid,
            peeled,
            name,
        } => {
            buf.push(GIT_STATE_RECORD_REF);
            buf.push(*flags);
            buf.extend_from_slice(oid);
            buf.extend_from_slice(peeled);
            push_str(buf, name);
        }
        GitStateRecord::Op { op, oid, detail } => {
            buf.push(GIT_STATE_RECORD_OP);
            buf.push(*op);
            buf.extend_from_slice(oid);
            push_str(buf, detail);
        }
        GitStateRecord::Status {
            staged,
            unstaged,
            flags,
            old_path,
            path,
        } => {
            buf.push(GIT_STATE_RECORD_STATUS);
            buf.push(*staged);
            buf.push(*unstaged);
            buf.push(*flags);
            push_str(buf, old_path);
            push_str(buf, path);
        }
        GitStateRecord::Upstream {
            flags,
            ahead,
            behind,
            name,
            upstream,
        } => {
            buf.push(GIT_STATE_RECORD_UPSTREAM);
            buf.push(*flags);
            buf.extend_from_slice(&ahead.to_le_bytes());
            buf.extend_from_slice(&behind.to_le_bytes());
            push_str(buf, name);
            push_str(buf, upstream);
        }
        GitStateRecord::Stash {
            index,
            oid,
            time,
            tz,
            msg,
        } => {
            buf.push(GIT_STATE_RECORD_STASH);
            buf.extend_from_slice(&index.to_le_bytes());
            buf.extend_from_slice(oid);
            buf.extend_from_slice(&time.to_le_bytes());
            buf.extend_from_slice(&tz.to_le_bytes());
            push_str(buf, msg);
        }
    }
    end_record(buf, start);
}

pub struct GitStateRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `GIT_STATE` payload.
pub fn git_state_records(data: &[u8]) -> GitStateRecordIter<'_> {
    GitStateRecordIter { data }
}

impl<'a> Iterator for GitStateRecordIter<'a> {
    type Item = GitStateRecord<'a>;

    fn next(&mut self) -> Option<GitStateRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                GIT_STATE_RECORD_HEAD => {
                    let flags = take_u8(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let name = take_str(&mut b)?;
                    return Some(GitStateRecord::Head { flags, oid, name });
                }
                GIT_STATE_RECORD_REF => {
                    let flags = take_u8(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let peeled = take_oid(&mut b)?;
                    let name = take_str(&mut b)?;
                    return Some(GitStateRecord::Ref {
                        flags,
                        oid,
                        peeled,
                        name,
                    });
                }
                GIT_STATE_RECORD_OP => {
                    let op = take_u8(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let detail = take_str(&mut b)?;
                    return Some(GitStateRecord::Op { op, oid, detail });
                }
                GIT_STATE_RECORD_STATUS => {
                    let staged = take_u8(&mut b)?;
                    let unstaged = take_u8(&mut b)?;
                    let flags = take_u8(&mut b)?;
                    let old_path = take_str(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(GitStateRecord::Status {
                        staged,
                        unstaged,
                        flags,
                        old_path,
                        path,
                    });
                }
                GIT_STATE_RECORD_UPSTREAM => {
                    let flags = take_u8(&mut b)?;
                    let ahead = take_u32(&mut b)?;
                    let behind = take_u32(&mut b)?;
                    let name = take_str(&mut b)?;
                    let upstream = take_str(&mut b)?;
                    return Some(GitStateRecord::Upstream {
                        flags,
                        ahead,
                        behind,
                        name,
                        upstream,
                    });
                }
                GIT_STATE_RECORD_STASH => {
                    let index = take_u16(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let time = take_i64(&mut b)?;
                    let tz = take_i16(&mut b)?;
                    let msg = take_str(&mut b)?;
                    return Some(GitStateRecord::Stash {
                        index,
                        oid,
                        time,
                        tz,
                        msg,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

/// One decoded record from a `GIT_COMMITS` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitCommitRecord<'a> {
    /// COMMIT 0x01: [kind:1][flags:1][oid:32][tree:32][n_parents:1][parents:32·N]
    /// [author_time:8 i64 s][author_tz:2 i16 min][committer_time:8][committer_tz:2]
    /// [author_name_len:2][author_name:N][author_email_len:2][email:N]
    /// [committer_name_len:2][…][committer_email_len:2][…][msg_len:4][message:N]
    Commit {
        flags: u8,
        oid: GitOid,
        tree: GitOid,
        parents: Vec<GitOid>,
        author_time: i64,
        author_tz: i16,
        committer_time: i64,
        committer_tz: i16,
        author_name: &'a str,
        author_email: &'a str,
        committer_name: &'a str,
        committer_email: &'a str,
        message: &'a str,
    },
    /// PATH_AT 0x02: [kind:1][otype:1][mode:4][oid:32][path_len:2][path:N]
    /// With `PATH_OIDS`: the object at the followed path as of the preceding
    /// COMMIT record; zero oid when that commit deletes it.
    PathAt {
        otype: u8,
        mode: u32,
        oid: GitOid,
        path: &'a str,
    },
}

/// Append one record to an uncompressed `GIT_COMMITS` records buffer.
pub fn append_git_commit_record(buf: &mut Vec<u8>, record: &GitCommitRecord<'_>) {
    let start = begin_record(buf);
    match record {
        GitCommitRecord::Commit {
            flags,
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
        } => {
            buf.push(GIT_COMMIT_RECORD_COMMIT);
            buf.push(*flags);
            buf.extend_from_slice(oid);
            buf.extend_from_slice(tree);
            buf.push(parents.len() as u8);
            push_oids(buf, parents);
            buf.extend_from_slice(&author_time.to_le_bytes());
            buf.extend_from_slice(&author_tz.to_le_bytes());
            buf.extend_from_slice(&committer_time.to_le_bytes());
            buf.extend_from_slice(&committer_tz.to_le_bytes());
            push_str(buf, author_name);
            push_str(buf, author_email);
            push_str(buf, committer_name);
            push_str(buf, committer_email);
            push_bytes(buf, message.as_bytes());
        }
        GitCommitRecord::PathAt {
            otype,
            mode,
            oid,
            path,
        } => {
            buf.push(GIT_COMMIT_RECORD_PATH_AT);
            buf.push(*otype);
            buf.extend_from_slice(&mode.to_le_bytes());
            buf.extend_from_slice(oid);
            push_str(buf, path);
        }
    }
    end_record(buf, start);
}

pub struct GitCommitRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `GIT_COMMITS` payload.
pub fn git_commit_records(data: &[u8]) -> GitCommitRecordIter<'_> {
    GitCommitRecordIter { data }
}

impl<'a> Iterator for GitCommitRecordIter<'a> {
    type Item = GitCommitRecord<'a>;

    fn next(&mut self) -> Option<GitCommitRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                GIT_COMMIT_RECORD_COMMIT => {
                    let flags = take_u8(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let tree = take_oid(&mut b)?;
                    let n_parents = take_u8(&mut b)? as usize;
                    let parents = take_oids(&mut b, n_parents)?;
                    let author_time = take_i64(&mut b)?;
                    let author_tz = take_i16(&mut b)?;
                    let committer_time = take_i64(&mut b)?;
                    let committer_tz = take_i16(&mut b)?;
                    let author_name = take_str(&mut b)?;
                    let author_email = take_str(&mut b)?;
                    let committer_name = take_str(&mut b)?;
                    let committer_email = take_str(&mut b)?;
                    let message = std::str::from_utf8(take_bytes(&mut b)?).ok()?;
                    return Some(GitCommitRecord::Commit {
                        flags,
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
                    });
                }
                GIT_COMMIT_RECORD_PATH_AT => {
                    let otype = take_u8(&mut b)?;
                    let mode = take_u32(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(GitCommitRecord::PathAt {
                        otype,
                        mode,
                        oid,
                        path,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

/// One decoded record from a `GIT_TREE` response payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitTreeRecord<'a> {
    /// TREE_ENTRY 0x02: [kind:1][otype:1][mode:4][oid:32][name_len:2][name:N]
    /// `mode` is the raw git mode (100644, 100755, 120000, 40000, 160000).
    Entry {
        otype: u8,
        mode: u32,
        oid: GitOid,
        name: &'a str,
    },
}

/// Append one record to an uncompressed `GIT_TREE` records buffer.
pub fn append_git_tree_record(buf: &mut Vec<u8>, record: &GitTreeRecord<'_>) {
    let start = begin_record(buf);
    match record {
        GitTreeRecord::Entry {
            otype,
            mode,
            oid,
            name,
        } => {
            buf.push(GIT_TREE_RECORD_ENTRY);
            buf.push(*otype);
            buf.extend_from_slice(&mode.to_le_bytes());
            buf.extend_from_slice(oid);
            push_str(buf, name);
        }
    }
    end_record(buf, start);
}

pub struct GitTreeRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `GIT_TREE` response payload.
pub fn git_tree_records(data: &[u8]) -> GitTreeRecordIter<'_> {
    GitTreeRecordIter { data }
}

impl<'a> Iterator for GitTreeRecordIter<'a> {
    type Item = GitTreeRecord<'a>;

    fn next(&mut self) -> Option<GitTreeRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                GIT_TREE_RECORD_ENTRY => {
                    let otype = take_u8(&mut b)?;
                    let mode = take_u32(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let name = take_str(&mut b)?;
                    return Some(GitTreeRecord::Entry {
                        otype,
                        mode,
                        oid,
                        name,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

/// One decoded record from a `GIT_DIFF` response payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitDiffRecord<'a> {
    /// DIFF_ENTRY 0x03: [kind:1][st:1][similarity:1][dflags:1]
    /// [old_mode:4][new_mode:4][old_oid:32][new_oid:32]
    /// [old_len:2][old_path:N][new_len:2][new_path:N]
    /// `st` is an ASCII porcelain letter (A M D R C T U); `similarity`
    /// 0-100 for renames/copies.
    Entry {
        st: u8,
        similarity: u8,
        dflags: u8,
        old_mode: u32,
        new_mode: u32,
        old_oid: GitOid,
        new_oid: GitOid,
        old_path: &'a str,
        new_path: &'a str,
    },
    /// BASE 0x04: [kind:1][oid:32]
    /// First record when a MERGE_BASE endpoint was used: the chosen base.
    Base { oid: GitOid },
}

/// Append one record to an uncompressed `GIT_DIFF` records buffer.
pub fn append_git_diff_record(buf: &mut Vec<u8>, record: &GitDiffRecord<'_>) {
    let start = begin_record(buf);
    match record {
        GitDiffRecord::Entry {
            st,
            similarity,
            dflags,
            old_mode,
            new_mode,
            old_oid,
            new_oid,
            old_path,
            new_path,
        } => {
            buf.push(GIT_DIFF_RECORD_ENTRY);
            buf.push(*st);
            buf.push(*similarity);
            buf.push(*dflags);
            buf.extend_from_slice(&old_mode.to_le_bytes());
            buf.extend_from_slice(&new_mode.to_le_bytes());
            buf.extend_from_slice(old_oid);
            buf.extend_from_slice(new_oid);
            push_str(buf, old_path);
            push_str(buf, new_path);
        }
        GitDiffRecord::Base { oid } => {
            buf.push(GIT_DIFF_RECORD_BASE);
            buf.extend_from_slice(oid);
        }
    }
    end_record(buf, start);
}

pub struct GitDiffRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `GIT_DIFF` response payload.
pub fn git_diff_records(data: &[u8]) -> GitDiffRecordIter<'_> {
    GitDiffRecordIter { data }
}

impl<'a> Iterator for GitDiffRecordIter<'a> {
    type Item = GitDiffRecord<'a>;

    fn next(&mut self) -> Option<GitDiffRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                GIT_DIFF_RECORD_ENTRY => {
                    let st = take_u8(&mut b)?;
                    let similarity = take_u8(&mut b)?;
                    let dflags = take_u8(&mut b)?;
                    let old_mode = take_u32(&mut b)?;
                    let new_mode = take_u32(&mut b)?;
                    let old_oid = take_oid(&mut b)?;
                    let new_oid = take_oid(&mut b)?;
                    let old_path = take_str(&mut b)?;
                    let new_path = take_str(&mut b)?;
                    return Some(GitDiffRecord::Entry {
                        st,
                        similarity,
                        dflags,
                        old_mode,
                        new_mode,
                        old_oid,
                        new_oid,
                        old_path,
                        new_path,
                    });
                }
                GIT_DIFF_RECORD_BASE => {
                    let oid = take_oid(&mut b)?;
                    return Some(GitDiffRecord::Base { oid });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

/// One decoded record from a structured `GIT_PATCH` response payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPatchRecord<'a> {
    /// PATCH_FILE 0x01: [kind:1][flags:1][old_len:2][old_path:N][new_len:2][new_path:N]
    /// Begins a file section.
    File {
        flags: u8,
        old_path: &'a str,
        new_path: &'a str,
    },
    /// PATCH_ROW 0x02: [kind:1][old_line:4][new_line:4]
    /// [old_text_len:4][old_text:N][new_text_len:4][new_text:N]
    /// [n_old_spans:2][spans:(start:4,len:4)·N][n_new_spans:2][spans:(start:4,len:4)·N]
    /// Line numbers are 1-based; 0 = side absent (pure addition/deletion).
    /// Text is the side's true bytes; spans are byte ranges within it.
    Row {
        old_line: u32,
        new_line: u32,
        old_text: &'a [u8],
        new_text: &'a [u8],
        old_spans: Vec<(u32, u32)>,
        new_spans: Vec<(u32, u32)>,
    },
    /// PATCH_GAP 0x03: [kind:1][old_line:4][new_line:4]
    /// Elision between hunks (the "@@" of a unified diff).
    Gap { old_line: u32, new_line: u32 },
    /// BASE 0x04: [kind:1][oid:32] — as in `GIT_DIFF`.
    Base { oid: GitOid },
}

fn push_spans(buf: &mut Vec<u8>, spans: &[(u32, u32)]) {
    buf.extend_from_slice(&(spans.len() as u16).to_le_bytes());
    for (start, len) in spans {
        buf.extend_from_slice(&start.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
    }
}

fn take_spans(b: &mut &[u8]) -> Option<Vec<(u32, u32)>> {
    let n = take_u16(b)? as usize;
    if b.len() < n * 8 {
        return None;
    }
    let mut spans = Vec::with_capacity(n);
    for _ in 0..n {
        let start = take_u32(b)?;
        let len = take_u32(b)?;
        spans.push((start, len));
    }
    Some(spans)
}

/// Append one record to an uncompressed `GIT_PATCH` records buffer.
pub fn append_git_patch_record(buf: &mut Vec<u8>, record: &GitPatchRecord<'_>) {
    let start = begin_record(buf);
    match record {
        GitPatchRecord::File {
            flags,
            old_path,
            new_path,
        } => {
            buf.push(GIT_PATCH_RECORD_FILE);
            buf.push(*flags);
            push_str(buf, old_path);
            push_str(buf, new_path);
        }
        GitPatchRecord::Row {
            old_line,
            new_line,
            old_text,
            new_text,
            old_spans,
            new_spans,
        } => {
            buf.push(GIT_PATCH_RECORD_ROW);
            buf.extend_from_slice(&old_line.to_le_bytes());
            buf.extend_from_slice(&new_line.to_le_bytes());
            push_bytes(buf, old_text);
            push_bytes(buf, new_text);
            push_spans(buf, old_spans);
            push_spans(buf, new_spans);
        }
        GitPatchRecord::Gap { old_line, new_line } => {
            buf.push(GIT_PATCH_RECORD_GAP);
            buf.extend_from_slice(&old_line.to_le_bytes());
            buf.extend_from_slice(&new_line.to_le_bytes());
        }
        GitPatchRecord::Base { oid } => {
            buf.push(GIT_PATCH_RECORD_BASE);
            buf.extend_from_slice(oid);
        }
    }
    end_record(buf, start);
}

pub struct GitPatchRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed structured `GIT_PATCH` payload.
pub fn git_patch_records(data: &[u8]) -> GitPatchRecordIter<'_> {
    GitPatchRecordIter { data }
}

impl<'a> Iterator for GitPatchRecordIter<'a> {
    type Item = GitPatchRecord<'a>;

    fn next(&mut self) -> Option<GitPatchRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                GIT_PATCH_RECORD_FILE => {
                    let flags = take_u8(&mut b)?;
                    let old_path = take_str(&mut b)?;
                    let new_path = take_str(&mut b)?;
                    return Some(GitPatchRecord::File {
                        flags,
                        old_path,
                        new_path,
                    });
                }
                GIT_PATCH_RECORD_ROW => {
                    let old_line = take_u32(&mut b)?;
                    let new_line = take_u32(&mut b)?;
                    let old_text = take_bytes(&mut b)?;
                    let new_text = take_bytes(&mut b)?;
                    let old_spans = take_spans(&mut b)?;
                    let new_spans = take_spans(&mut b)?;
                    return Some(GitPatchRecord::Row {
                        old_line,
                        new_line,
                        old_text,
                        new_text,
                        old_spans,
                        new_spans,
                    });
                }
                GIT_PATCH_RECORD_GAP => {
                    let old_line = take_u32(&mut b)?;
                    let new_line = take_u32(&mut b)?;
                    return Some(GitPatchRecord::Gap { old_line, new_line });
                }
                GIT_PATCH_RECORD_BASE => {
                    let oid = take_oid(&mut b)?;
                    return Some(GitPatchRecord::Base { oid });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

/// One decoded record from a `GIT_INDEX` response payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitIndexRecord<'a> {
    /// INDEX_ENTRY 0x04: [kind:1][stage:1][iflags:1][mode:4][size:8][mtime_ns:8][oid:32][path_len:2][path:N]
    /// Conflicted paths appear as their stage-1/2/3 entries.
    Entry {
        stage: u8,
        iflags: u8,
        mode: u32,
        size: u64,
        mtime_ns: u64,
        oid: GitOid,
        path: &'a str,
    },
}

/// Append one record to an uncompressed `GIT_INDEX` records buffer.
pub fn append_git_index_record(buf: &mut Vec<u8>, record: &GitIndexRecord<'_>) {
    let start = begin_record(buf);
    match record {
        GitIndexRecord::Entry {
            stage,
            iflags,
            mode,
            size,
            mtime_ns,
            oid,
            path,
        } => {
            buf.push(GIT_INDEX_RECORD_ENTRY);
            buf.push(*stage);
            buf.push(*iflags);
            buf.extend_from_slice(&mode.to_le_bytes());
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&mtime_ns.to_le_bytes());
            buf.extend_from_slice(oid);
            push_str(buf, path);
        }
    }
    end_record(buf, start);
}

pub struct GitIndexRecordIter<'a> {
    data: &'a [u8],
}

/// Iterate records in an uncompressed `GIT_INDEX` response payload.
pub fn git_index_records(data: &[u8]) -> GitIndexRecordIter<'_> {
    GitIndexRecordIter { data }
}

impl<'a> Iterator for GitIndexRecordIter<'a> {
    type Item = GitIndexRecord<'a>;

    fn next(&mut self) -> Option<GitIndexRecord<'a>> {
        loop {
            let (kind, mut b) = next_record(&mut self.data)?;
            match kind {
                GIT_INDEX_RECORD_ENTRY => {
                    let stage = take_u8(&mut b)?;
                    let iflags = take_u8(&mut b)?;
                    let mode = take_u32(&mut b)?;
                    let size = take_u64(&mut b)?;
                    let mtime_ns = take_u64(&mut b)?;
                    let oid = take_oid(&mut b)?;
                    let path = take_str(&mut b)?;
                    return Some(GitIndexRecord::Entry {
                        stage,
                        iflags,
                        mode,
                        size,
                        mtime_ns,
                        oid,
                        path,
                    });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Client-side state reducer
// ---------------------------------------------------------------------------

/// The current HEAD.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitHead {
    pub flags: u8,
    pub oid: GitOid,
    /// Symbolic target (empty when detached).
    pub name: String,
}

/// One ref, keyed by name in [`GitStateMirror::refs`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitRefState {
    pub flags: u8,
    pub oid: GitOid,
    /// Valid only with [`GIT_REF_PEELED_VALID`].
    pub peeled: GitOid,
}

/// The in-progress operation, if any.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitOpState {
    pub op: u8,
    pub oid: GitOid,
    pub detail: String,
}

/// One index/worktree status entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStatusEntry {
    pub staged: u8,
    pub unstaged: u8,
    pub flags: u8,
    /// Non-empty only for renames.
    pub old_path: String,
    pub path: String,
}

/// Upstream tracking for one local branch, keyed by branch ref name in
/// [`GitStateMirror::upstreams`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitUpstreamState {
    pub flags: u8,
    pub ahead: u32,
    pub behind: u32,
    pub upstream: String,
}

/// One stash entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStashEntry {
    /// The N of `stash@{N}`.
    pub index: u16,
    pub oid: GitOid,
    pub time: i64,
    pub tz: i16,
    pub message: String,
}

/// The complete client obligation for `GIT_STATE`: each snapshot replaces
/// the whole typed state — no diffing, no staging (docs/git.md
/// "GIT_STATE / GIT_ACK").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitStateMirror {
    pub head: Option<GitHead>,
    /// Keyed by escaped ref name.
    pub refs: BTreeMap<String, GitRefState>,
    pub op: Option<GitOpState>,
    pub status: Vec<GitStatusEntry>,
    /// Keyed by local branch ref name (joins `refs`).
    pub upstreams: BTreeMap<String, GitUpstreamState>,
    pub stashes: Vec<GitStashEntry>,
    /// The last snapshot's truncation flags (`GIT_STATE_*_TRUNCATED`).
    pub flags: u8,
}

impl GitStateMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `GIT_STATE` message (starting at the opcode byte),
    /// replacing the whole state. Returns `Some(state_id)` to acknowledge,
    /// `None` if malformed.
    pub fn apply_state(&mut self, msg: &[u8]) -> Option<u32> {
        let (_repo_id, state_id, flags, records) = parse_git_state(msg)?;
        let mut next = GitStateMirror {
            flags,
            ..Default::default()
        };
        for record in git_state_records(&records) {
            match record {
                GitStateRecord::Head { flags, oid, name } => {
                    next.head = Some(GitHead {
                        flags,
                        oid,
                        name: name.to_string(),
                    });
                }
                GitStateRecord::Ref {
                    flags,
                    oid,
                    peeled,
                    name,
                } => {
                    next.refs
                        .insert(name.to_string(), GitRefState { flags, oid, peeled });
                }
                GitStateRecord::Op { op, oid, detail } => {
                    next.op = Some(GitOpState {
                        op,
                        oid,
                        detail: detail.to_string(),
                    });
                }
                GitStateRecord::Status {
                    staged,
                    unstaged,
                    flags,
                    old_path,
                    path,
                } => {
                    next.status.push(GitStatusEntry {
                        staged,
                        unstaged,
                        flags,
                        old_path: old_path.to_string(),
                        path: path.to_string(),
                    });
                }
                GitStateRecord::Upstream {
                    flags,
                    ahead,
                    behind,
                    name,
                    upstream,
                } => {
                    next.upstreams.insert(
                        name.to_string(),
                        GitUpstreamState {
                            flags,
                            ahead,
                            behind,
                            upstream: upstream.to_string(),
                        },
                    );
                }
                GitStateRecord::Stash {
                    index,
                    oid,
                    time,
                    tz,
                    msg,
                } => {
                    next.stashes.push(GitStashEntry {
                        index,
                        oid,
                        time,
                        tz,
                        message: msg.to_string(),
                    });
                }
            }
        }
        *self = next;
        Some(state_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture oid: `fill` repeated over the hash width, zero-padded to
    /// 32 bytes like a SHA-1 oid on the wire.
    fn oid(fill: u8) -> GitOid {
        let mut o = [0u8; 32];
        o[..20].fill(fill);
        o
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn request_roundtrips() {
        let msg = msg_git_open(1, GIT_OPEN_WATCH | GIT_OPEN_STATUS, 50, 500, "/repo");
        assert_eq!(
            parse_git_open(&msg),
            Some((1, GIT_OPEN_WATCH | GIT_OPEN_STATUS, 50, 500, "/repo"))
        );
        // Empty path and zero windows (server defaults).
        let msg = msg_git_open(0, 0, 0, 0, "");
        assert_eq!(parse_git_open(&msg), Some((0, 0, 0, 0, "")));

        assert_eq!(parse_git_close(&msg_git_close(7)), Some(7));
        assert_eq!(
            parse_git_ack(&msg_git_ack(7, u32::MAX)),
            Some((7, u32::MAX))
        );

        let tips = vec![oid(0xAA), oid(0xAB)];
        let hides = vec![oid(0xBB)];
        let msg = msg_git_log(
            3,
            7,
            GIT_LOG_FOLLOW | GIT_LOG_PATH_OIDS,
            100,
            "src/a.rs",
            &tips,
            &hides,
        );
        assert_eq!(
            parse_git_log(&msg),
            Some(GitLogRequest {
                nonce: 3,
                repo_id: 7,
                flags: GIT_LOG_FOLLOW | GIT_LOG_PATH_OIDS,
                limit: 100,
                path: "src/a.rs",
                tips,
                hides,
            })
        );
        // Empty tips (= HEAD), empty hides, no filter.
        let msg = msg_git_log(4, 7, 0, 0, "", &[], &[]);
        assert_eq!(
            parse_git_log(&msg),
            Some(GitLogRequest {
                nonce: 4,
                repo_id: 7,
                flags: 0,
                limit: 0,
                path: "",
                tips: vec![],
                hides: vec![],
            })
        );

        let msg = msg_git_tree(4, 7, &oid(0xCC), "dir/%FF");
        assert_eq!(parse_git_tree(&msg), Some((4, 7, oid(0xCC), "dir/%FF")));

        let msg = msg_git_blob(5, 7, &oid(0xDD), "", 1 << 20);
        assert_eq!(parse_git_blob(&msg), Some((5, 7, oid(0xDD), "", 1 << 20)));

        let old = GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: oid(0x11),
        };
        let new = GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        };
        let msg = msg_git_diff(6, 7, GIT_DIFF_RENAMES, old, new, "sub");
        assert_eq!(
            parse_git_diff(&msg),
            Some(GitDiffRequest {
                nonce: 6,
                repo_id: 7,
                flags: GIT_DIFF_RENAMES,
                old,
                new,
                path: "sub",
            })
        );

        let msg = msg_git_patch(
            8,
            7,
            GIT_DIFF_RENAMES | GIT_PATCH_CHAR_SPANS,
            5,
            old,
            new,
            "a.txt",
            1 << 16,
        );
        assert_eq!(
            parse_git_patch(&msg),
            Some(GitPatchRequest {
                nonce: 8,
                repo_id: 7,
                flags: GIT_DIFF_RENAMES | GIT_PATCH_CHAR_SPANS,
                context: 5,
                old,
                new,
                path: "a.txt",
                max_len: 1 << 16,
            })
        );

        assert_eq!(
            parse_git_index(&msg_git_index(9, 7, "sub")),
            Some((9, 7, "sub"))
        );
        assert_eq!(parse_git_cancel(&msg_git_cancel(10)), Some(10));

        let oids = vec![oid(0xAA), oid(0xBB), oid(0xCC)];
        let msg = msg_git_base(11, 7, &oids);
        assert_eq!(parse_git_base(&msg), Some((11, 7, oids)));

        assert_eq!(
            parse_git_resolve(&msg_git_resolve(12, 7, "main..dev")),
            Some((12, 7, "main..dev"))
        );
        assert_eq!(
            parse_git_log_watch(&msg_git_log_watch(1, 7, GIT_LOG_FIRST_PARENT, 100, "main")),
            Some((1, 7, GIT_LOG_FIRST_PARENT, 100, "main"))
        );
        // Empty spec (= HEAD default), zero limit (server default).
        assert_eq!(
            parse_git_log_watch(&msg_git_log_watch(2, 7, 0, 0, "")),
            Some((2, 7, 0, 0, ""))
        );
        assert_eq!(
            parse_git_log_unwatch(&msg_git_log_unwatch(1, 7)),
            Some((1, 7))
        );
        assert_eq!(
            parse_git_log_ack(&msg_git_log_ack(1, 7, u32::MAX)),
            Some((1, 7, u32::MAX))
        );

        // Wrong opcode is rejected.
        assert_eq!(parse_git_close(&msg_git_cancel(1)), None);
        // Truncated message is rejected.
        assert_eq!(parse_git_open(&msg_git_open(1, 0, 0, 0, "x")[..5]), None);
    }

    #[test]
    fn response_roundtrips() {
        let msg = msg_git_repo(
            1,
            2,
            GIT_STATUS_OK,
            GIT_OID_FORMAT_SHA1,
            GIT_REPO_LINKED,
            "/w",
            "/w/.git",
        );
        assert_eq!(
            parse_git_repo(&msg),
            Some(GitRepoInfo {
                nonce: 1,
                repo_id: 2,
                status: GIT_STATUS_OK,
                oid_format: GIT_OID_FORMAT_SHA1,
                flags: GIT_REPO_LINKED,
                workdir: "/w",
                gitdir: "/w/.git",
            })
        );
        // Failure shape: invalid repo id, diagnostic in workdir, empty gitdir.
        let msg = msg_git_repo(
            1,
            GIT_REPO_ID_INVALID,
            GIT_STATUS_NOT_FOUND,
            0,
            0,
            "no repo",
            "",
        );
        let info = parse_git_repo(&msg).unwrap();
        assert_eq!(info.repo_id, GIT_REPO_ID_INVALID);
        assert_eq!(info.status, GIT_STATUS_NOT_FOUND);
        assert_eq!(info.workdir, "no repo");
        assert_eq!(info.gitdir, "");

        let msg = msg_git_state(2, 9, GIT_STATE_REFS_TRUNCATED, b"records");
        assert_eq!(
            parse_git_state(&msg),
            Some((2, 9, GIT_STATE_REFS_TRUNCATED, b"records".to_vec()))
        );

        assert_eq!(
            parse_git_closed(&msg_git_closed(2, GIT_CLOSED_REPO_GONE)),
            Some((2, GIT_CLOSED_REPO_GONE))
        );

        let frontier = vec![oid(0xEE)];
        let msg = msg_git_commits(3, GIT_STATUS_OK, GIT_COMMITS_MORE, &frontier, b"recs");
        assert_eq!(
            parse_git_commits(&msg),
            Some(GitCommitsPage {
                nonce: 3,
                status: GIT_STATUS_OK,
                flags: GIT_COMMITS_MORE,
                frontier,
                records: b"recs".to_vec(),
            })
        );
        // Terminal page: empty frontier, empty records.
        let msg = msg_git_commits(4, GIT_STATUS_OK, 0, &[], &[]);
        assert_eq!(
            parse_git_commits(&msg),
            Some(GitCommitsPage {
                nonce: 4,
                status: GIT_STATUS_OK,
                flags: 0,
                frontier: vec![],
                records: vec![],
            })
        );

        let msg = msg_git_tree_resp(5, GIT_STATUS_OK, GIT_TREE_TRUNCATED, b"t");
        assert_eq!(
            parse_git_tree_resp(&msg),
            Some((5, GIT_STATUS_OK, GIT_TREE_TRUNCATED, b"t".to_vec()))
        );

        let msg = msg_git_blob_resp(6, GIT_STATUS_OK, 11, b"hello world");
        assert_eq!(
            parse_git_blob_resp(&msg),
            Some((6, GIT_STATUS_OK, 11, b"hello world".to_vec()))
        );
        // TOO_LARGE still carries the true size, with empty data.
        let msg = msg_git_blob_resp(7, GIT_STATUS_TOO_LARGE, 1 << 40, &[]);
        assert_eq!(
            parse_git_blob_resp(&msg),
            Some((7, GIT_STATUS_TOO_LARGE, 1 << 40, vec![]))
        );

        let msg = msg_git_diff_resp(8, GIT_STATUS_OK, 0, b"d");
        assert_eq!(
            parse_git_diff_resp(&msg),
            Some((8, GIT_STATUS_OK, 0, b"d".to_vec()))
        );

        let msg = msg_git_patch_resp(9, GIT_STATUS_OK, GIT_PATCH_STRUCTURED, b"p");
        assert_eq!(
            parse_git_patch_resp(&msg),
            Some((9, GIT_STATUS_OK, GIT_PATCH_STRUCTURED, b"p".to_vec()))
        );

        let msg = msg_git_index_resp(10, GIT_STATUS_OK, 0, b"i");
        assert_eq!(
            parse_git_index_resp(&msg),
            Some((10, GIT_STATUS_OK, 0, b"i".to_vec()))
        );

        let bases = vec![oid(0xAB)];
        let msg = msg_git_base_resp(11, GIT_STATUS_OK, &bases);
        assert_eq!(parse_git_base_resp(&msg), Some((11, GIT_STATUS_OK, bases)));
        // Disjoint histories: OK with zero bases.
        let msg = msg_git_base_resp(12, GIT_STATUS_OK, &[]);
        assert_eq!(parse_git_base_resp(&msg), Some((12, GIT_STATUS_OK, vec![])));

        let tips = vec![oid(0xCC)];
        let hides = vec![oid(0xDD)];
        let msg = msg_git_resolve_resp(13, GIT_STATUS_OK, &tips, &hides);
        assert_eq!(
            parse_git_resolve_resp(&msg),
            Some((13, GIT_STATUS_OK, tips, hides))
        );
        // A single tip, no hides (a plain ref/oid).
        let msg = msg_git_resolve_resp(14, GIT_STATUS_OK, &[oid(0xCC)], &[]);
        assert_eq!(
            parse_git_resolve_resp(&msg),
            Some((14, GIT_STATUS_OK, vec![oid(0xCC)], vec![]))
        );

        let frontier = vec![oid(0xEE)];
        let msg = msg_git_log_page(1, 42, GIT_STATUS_OK, GIT_COMMITS_MORE, &frontier, b"recs");
        assert_eq!(
            parse_git_log_page(&msg),
            Some(GitLogPage {
                log_id: 1,
                update_id: 42,
                status: GIT_STATUS_OK,
                flags: GIT_COMMITS_MORE,
                frontier,
                records: b"recs".to_vec(),
            })
        );
        // Unresolvable spec: status carries the error, empty frontier/records.
        let msg = msg_git_log_page(2, 0, GIT_STATUS_NOT_FOUND, 0, &[], &[]);
        assert_eq!(
            parse_git_log_page(&msg),
            Some(GitLogPage {
                log_id: 2,
                update_id: 0,
                status: GIT_STATUS_NOT_FOUND,
                flags: 0,
                frontier: vec![],
                records: vec![],
            })
        );
    }

    #[test]
    fn state_record_roundtrip() {
        let records = vec![
            GitStateRecord::Head {
                flags: 0,
                oid: oid(0x01),
                name: "refs/heads/main",
            },
            // Unborn HEAD: zero oid, empty name edge on another kind below.
            GitStateRecord::Head {
                flags: GIT_HEAD_UNBORN,
                oid: GIT_OID_NONE,
                name: "refs/heads/new",
            },
            GitStateRecord::Ref {
                flags: GIT_REF_PEELED_VALID,
                oid: oid(0x02),
                peeled: oid(0x03),
                name: "refs/tags/v1",
            },
            GitStateRecord::Op {
                op: GIT_OP_REBASE,
                oid: oid(0x04),
                detail: "",
            },
            GitStateRecord::Status {
                staged: b'R',
                unstaged: b' ',
                flags: 0,
                old_path: "old.txt",
                path: "new.txt",
            },
            GitStateRecord::Status {
                staged: b'?',
                unstaged: b'?',
                flags: GIT_STATUS_ENTRY_CONFLICTED,
                old_path: "",
                path: "%FF.bin",
            },
            GitStateRecord::Upstream {
                flags: GIT_UPSTREAM_COUNTS_VALID,
                ahead: 2,
                behind: 3,
                name: "refs/heads/main",
                upstream: "refs/remotes/origin/main",
            },
            GitStateRecord::Stash {
                index: 0,
                oid: oid(0x05),
                time: 1_700_000_000,
                tz: -300,
                msg: "WIP on main",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_git_state_record(&mut buf, r);
        }
        let decoded: Vec<_> = git_state_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn commit_record_roundtrip() {
        let records = vec![
            GitCommitRecord::Commit {
                flags: GIT_COMMIT_LOSSY_ENCODING,
                oid: oid(0x0A),
                tree: oid(0x0B),
                parents: vec![oid(0x0C), oid(0x0D)],
                author_time: 1_700_000_000,
                author_tz: 60,
                committer_time: 1_700_000_001,
                committer_tz: -300,
                author_name: "Ann Author",
                author_email: "ann@example.com",
                committer_name: "Cam Committer",
                committer_email: "cam@example.com",
                message: "subject\n\nbody\n",
            },
            // Root commit: no parents, empty message.
            GitCommitRecord::Commit {
                flags: 0,
                oid: oid(0x0E),
                tree: oid(0x0B),
                parents: vec![],
                author_time: 0,
                author_tz: 0,
                committer_time: 0,
                committer_tz: 0,
                author_name: "",
                author_email: "",
                committer_name: "",
                committer_email: "",
                message: "",
            },
            GitCommitRecord::PathAt {
                otype: GIT_OTYPE_BLOB,
                mode: 0o100644,
                oid: oid(0x0F),
                path: "src/lib.rs",
            },
            // Deleted at this commit: zero oid.
            GitCommitRecord::PathAt {
                otype: GIT_OTYPE_BLOB,
                mode: 0,
                oid: GIT_OID_NONE,
                path: "gone.rs",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_git_commit_record(&mut buf, r);
        }
        let decoded: Vec<_> = git_commit_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn tree_record_roundtrip() {
        let records = vec![
            GitTreeRecord::Entry {
                otype: GIT_OTYPE_TREE,
                mode: 0o40000,
                oid: oid(0x0E),
                name: "src",
            },
            GitTreeRecord::Entry {
                otype: GIT_OTYPE_BLOB,
                mode: 0o100644,
                oid: oid(0x0F),
                name: "%FF.bin", // server-escaped non-UTF-8 name
            },
            GitTreeRecord::Entry {
                otype: GIT_OTYPE_COMMIT,
                mode: 0o160000,
                oid: oid(0x10),
                name: "submodule",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_git_tree_record(&mut buf, r);
        }
        let decoded: Vec<_> = git_tree_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn diff_record_roundtrip() {
        let records = vec![
            GitDiffRecord::Base { oid: oid(0x10) },
            GitDiffRecord::Entry {
                st: b'R',
                similarity: 90,
                dflags: 0,
                old_mode: 0o100644,
                new_mode: 0o100755,
                old_oid: oid(0x11),
                new_oid: oid(0x12),
                old_path: "old.txt",
                new_path: "new.txt",
            },
            // Untracked addition: absent old side, unhashed new side.
            GitDiffRecord::Entry {
                st: b'A',
                similarity: 0,
                dflags: GIT_DIFF_ENTRY_BINARY,
                old_mode: 0,
                new_mode: 0o100644,
                old_oid: GIT_OID_NONE,
                new_oid: GIT_OID_NONE,
                old_path: "",
                new_path: "new.bin",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_git_diff_record(&mut buf, r);
        }
        let decoded: Vec<_> = git_diff_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn patch_record_roundtrip() {
        let records = vec![
            GitPatchRecord::Base { oid: oid(0x13) },
            GitPatchRecord::File {
                flags: 0,
                old_path: "a.txt",
                new_path: "a.txt",
            },
            GitPatchRecord::Row {
                old_line: 1,
                new_line: 1,
                old_text: b"hello world",
                new_text: b"hallo world",
                old_spans: vec![(1, 1)],
                new_spans: vec![(1, 1)],
            },
            // Context row: no spans; pure addition: absent old side.
            GitPatchRecord::Row {
                old_line: 2,
                new_line: 2,
                old_text: b"same",
                new_text: b"same",
                old_spans: vec![],
                new_spans: vec![],
            },
            GitPatchRecord::Row {
                old_line: 0,
                new_line: 3,
                old_text: b"",
                new_text: b"added",
                old_spans: vec![],
                new_spans: vec![(0, 5)],
            },
            GitPatchRecord::Gap {
                old_line: 10,
                new_line: 11,
            },
            GitPatchRecord::File {
                flags: GIT_PATCH_FILE_BINARY,
                old_path: "img.png",
                new_path: "img.png",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_git_patch_record(&mut buf, r);
        }
        let decoded: Vec<_> = git_patch_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn index_record_roundtrip() {
        let records = vec![
            GitIndexRecord::Entry {
                stage: 0,
                iflags: GIT_INDEX_INTENT_TO_ADD,
                mode: 0o100644,
                size: 5,
                mtime_ns: 1_700_000_000_000_000_000,
                oid: oid(0x14),
                path: "a.txt",
            },
            // Conflict stage entry.
            GitIndexRecord::Entry {
                stage: 2,
                iflags: 0,
                mode: 0o100644,
                size: 0,
                mtime_ns: 0,
                oid: oid(0x15),
                path: "conflicted.txt",
            },
        ];
        let mut buf = Vec::new();
        for r in &records {
            append_git_index_record(&mut buf, r);
        }
        let decoded: Vec<_> = git_index_records(&buf).collect();
        assert_eq!(decoded, records);
    }

    #[test]
    fn unknown_record_kind_is_skipped() {
        // A future record kind 0x7F with 3 payload bytes, then a valid
        // record, for every family.
        let mut unknown = Vec::new();
        unknown.extend_from_slice(&4u32.to_le_bytes());
        unknown.push(0x7F);
        unknown.extend_from_slice(&[1, 2, 3]);

        let mut buf = unknown.clone();
        append_git_state_record(
            &mut buf,
            &GitStateRecord::Op {
                op: GIT_OP_MERGE,
                oid: oid(1),
                detail: "",
            },
        );
        assert_eq!(git_state_records(&buf).count(), 1);

        let mut buf = unknown.clone();
        append_git_commit_record(
            &mut buf,
            &GitCommitRecord::PathAt {
                otype: GIT_OTYPE_BLOB,
                mode: 0,
                oid: oid(1),
                path: "p",
            },
        );
        assert_eq!(git_commit_records(&buf).count(), 1);

        let mut buf = unknown.clone();
        append_git_tree_record(
            &mut buf,
            &GitTreeRecord::Entry {
                otype: GIT_OTYPE_BLOB,
                mode: 0,
                oid: oid(1),
                name: "n",
            },
        );
        assert_eq!(git_tree_records(&buf).count(), 1);

        let mut buf = unknown.clone();
        append_git_diff_record(&mut buf, &GitDiffRecord::Base { oid: oid(1) });
        assert_eq!(git_diff_records(&buf).count(), 1);

        let mut buf = unknown.clone();
        append_git_patch_record(
            &mut buf,
            &GitPatchRecord::Gap {
                old_line: 1,
                new_line: 1,
            },
        );
        assert_eq!(git_patch_records(&buf).count(), 1);

        let mut buf = unknown.clone();
        append_git_index_record(
            &mut buf,
            &GitIndexRecord::Entry {
                stage: 0,
                iflags: 0,
                mode: 0,
                size: 0,
                mtime_ns: 0,
                oid: oid(1),
                path: "p",
            },
        );
        assert_eq!(git_index_records(&buf).count(), 1);
    }

    #[test]
    fn malformed_record_ends_iteration() {
        // A HEAD record whose body is truncated to one byte.
        let mut buf = Vec::new();
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.push(GIT_STATE_RECORD_HEAD);
        buf.push(0);
        append_git_state_record(
            &mut buf,
            &GitStateRecord::Head {
                flags: 0,
                oid: oid(1),
                name: "refs/heads/main",
            },
        );
        assert_eq!(git_state_records(&buf).next(), None);
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_allocation() {
        // Hand-forged messages whose LZ4 size prefix declares 1 GiB.
        fn forged(opcode: u8, header: &[u8]) -> Vec<u8> {
            let mut msg = vec![opcode];
            msg.extend_from_slice(header);
            msg.extend_from_slice(&(1u32 << 30).to_le_bytes());
            msg.extend_from_slice(&[0u8; 16]);
            msg
        }

        let state = forged(S2C_GIT_STATE, &[1, 0, 1, 0, 0, 0, 0]);
        assert_eq!(parse_git_state(&state), None);
        assert_eq!(GitStateMirror::new().apply_state(&state), None);
        assert_eq!(
            parse_git_commits(&forged(S2C_GIT_COMMITS, &[1, 0, 0, 0, 0, 0])),
            None
        );
        assert_eq!(
            parse_git_tree_resp(&forged(S2C_GIT_TREE, &[1, 0, 0, 0])),
            None
        );
        assert_eq!(
            parse_git_blob_resp(&forged(S2C_GIT_BLOB, &[1, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0])),
            None
        );
        assert_eq!(
            parse_git_diff_resp(&forged(S2C_GIT_DIFF, &[1, 0, 0, 0])),
            None
        );
        assert_eq!(
            parse_git_patch_resp(&forged(S2C_GIT_PATCH, &[1, 0, 0, 0])),
            None
        );
        assert_eq!(
            parse_git_index_resp(&forged(S2C_GIT_INDEX, &[1, 0, 0, 0])),
            None
        );
    }

    #[test]
    fn state_mirror_replaces_whole_state() {
        let mut mirror = GitStateMirror::new();

        let mut records = Vec::new();
        append_git_state_record(
            &mut records,
            &GitStateRecord::Head {
                flags: 0,
                oid: oid(1),
                name: "refs/heads/main",
            },
        );
        append_git_state_record(
            &mut records,
            &GitStateRecord::Ref {
                flags: 0,
                oid: oid(1),
                peeled: GIT_OID_NONE,
                name: "refs/heads/main",
            },
        );
        append_git_state_record(
            &mut records,
            &GitStateRecord::Ref {
                flags: GIT_REF_PEELED_VALID,
                oid: oid(2),
                peeled: oid(3),
                name: "refs/tags/v1",
            },
        );
        append_git_state_record(
            &mut records,
            &GitStateRecord::Op {
                op: GIT_OP_MERGE,
                oid: oid(4),
                detail: "",
            },
        );
        append_git_state_record(
            &mut records,
            &GitStateRecord::Status {
                staged: b'M',
                unstaged: b' ',
                flags: 0,
                old_path: "",
                path: "a.txt",
            },
        );
        append_git_state_record(
            &mut records,
            &GitStateRecord::Upstream {
                flags: GIT_UPSTREAM_COUNTS_VALID,
                ahead: 2,
                behind: 3,
                name: "refs/heads/main",
                upstream: "refs/remotes/origin/main",
            },
        );
        append_git_state_record(
            &mut records,
            &GitStateRecord::Stash {
                index: 0,
                oid: oid(5),
                time: 1_700_000_000,
                tz: -300,
                msg: "WIP on main",
            },
        );
        let msg = msg_git_state(1, 1, GIT_STATE_STATUS_TRUNCATED, &records);
        assert_eq!(mirror.apply_state(&msg), Some(1));
        assert_eq!(
            mirror.head,
            Some(GitHead {
                flags: 0,
                oid: oid(1),
                name: "refs/heads/main".to_string(),
            })
        );
        assert_eq!(mirror.refs.len(), 2);
        assert_eq!(mirror.refs["refs/tags/v1"].peeled, oid(3));
        assert_eq!(mirror.op.as_ref().unwrap().op, GIT_OP_MERGE);
        assert_eq!(mirror.status.len(), 1);
        assert_eq!(mirror.upstreams["refs/heads/main"].ahead, 2);
        assert_eq!(mirror.stashes[0].message, "WIP on main");
        assert_eq!(mirror.flags, GIT_STATE_STATUS_TRUNCATED);

        // The next snapshot replaces everything: op ended, status cleared,
        // detached HEAD.
        let mut records = Vec::new();
        append_git_state_record(
            &mut records,
            &GitStateRecord::Head {
                flags: GIT_HEAD_DETACHED,
                oid: oid(6),
                name: "",
            },
        );
        let msg = msg_git_state(1, 2, 0, &records);
        assert_eq!(mirror.apply_state(&msg), Some(2));
        assert_eq!(mirror.head.as_ref().unwrap().flags, GIT_HEAD_DETACHED);
        assert!(mirror.refs.is_empty());
        assert_eq!(mirror.op, None);
        assert!(mirror.status.is_empty());
        assert!(mirror.upstreams.is_empty());
        assert!(mirror.stashes.is_empty());
        assert_eq!(mirror.flags, 0);

        // Malformed: wrong opcode, truncated header.
        assert_eq!(mirror.apply_state(&msg_git_closed(1, 0)), None);
        assert_eq!(mirror.apply_state(&msg[..6]), None);
    }

    /// Byte fixtures shared with the TypeScript codecs
    /// (`js/core/src/__tests__/git.test.ts` pins the same hex), so codec
    /// drift fails on one side or the other. Buffers that cross LZ4 are
    /// pinned uncompressed — LZ4 output may legitimately change across
    /// `lz4_flex` versions, while these bytes never can.
    #[test]
    fn wire_fixtures() {
        let zeros = "0".repeat(24);
        let zero_oid = "0".repeat(64);
        let o = |fill: u8| format!("{}{zeros}", hex(&[fill; 20]));

        assert_eq!(
            hex(&msg_git_open(
                0x0102,
                GIT_OPEN_WATCH | GIT_OPEN_STATUS,
                50,
                500,
                "/repo"
            )),
            "500201033200f40105002f7265706f"
        );
        assert_eq!(hex(&msg_git_close(7)), "510700");
        assert_eq!(hex(&msg_git_ack(7, 0x01020304)), "52070004030201");
        assert_eq!(
            hex(&msg_git_log(
                3,
                7,
                GIT_LOG_FIRST_PARENT,
                100,
                "src",
                &[oid(0xAA)],
                &[oid(0xBB)]
            )),
            format!("530300070001640003007372630100{}0100{}", o(0xAA), o(0xBB))
        );
        assert_eq!(
            hex(&msg_git_tree(4, 7, &oid(0xCC), "dir/%FF")),
            format!("5404000700{}07006469722f254646", o(0xCC))
        );
        assert_eq!(
            hex(&msg_git_blob(5, 7, &oid(0xDD), "", 1 << 20)),
            format!("5505000700{}000000001000", o(0xDD))
        );
        let old = GitEndpoint {
            kind: GIT_ENDPOINT_COMMIT,
            oid: oid(0x11),
        };
        let new = GitEndpoint {
            kind: GIT_ENDPOINT_WORKTREE,
            oid: GIT_OID_NONE,
        };
        assert_eq!(
            hex(&msg_git_diff(6, 7, GIT_DIFF_RENAMES, old, new, "")),
            format!("56060007000101{}04{zero_oid}0000", o(0x11))
        );
        assert_eq!(
            hex(&msg_git_patch(
                8,
                7,
                GIT_DIFF_RENAMES | GIT_PATCH_CHAR_SPANS,
                5,
                old,
                new,
                "a.txt",
                0
            )),
            format!(
                "5708000700410501{}04{zero_oid}0500612e74787400000000",
                o(0x11)
            )
        );
        assert_eq!(hex(&msg_git_index(9, 7, "sub")), "58090007000300737562");
        assert_eq!(hex(&msg_git_cancel(10)), "590a00");
        assert_eq!(
            hex(&msg_git_base(11, 7, &[oid(0xAA), oid(0xBB)])),
            format!("5a0b00070002{}{}", o(0xAA), o(0xBB))
        );
        assert_eq!(
            hex(&msg_git_resolve(12, 7, "main..dev")),
            "5b0c00070009006d61696e2e2e646576"
        );
        assert_eq!(
            hex(&msg_git_resolve_resp(
                12,
                GIT_STATUS_OK,
                &[oid(0xCC)],
                &[oid(0xDD)]
            )),
            format!("5b0c00000100{}0100{}", o(0xCC), o(0xDD))
        );
        assert_eq!(
            hex(&msg_git_log_watch(1, 7, GIT_LOG_FIRST_PARENT, 100, "main")),
            "5c0100070001640004006d61696e"
        );
        assert_eq!(hex(&msg_git_log_unwatch(1, 7)), "5d01000700");
        assert_eq!(
            hex(&msg_git_log_ack(1, 7, 0x0102_0304)),
            "5e0100070004030201"
        );
        assert_eq!(
            hex(&msg_git_repo(
                0x0102,
                1,
                GIT_STATUS_OK,
                GIT_OID_FORMAT_SHA1,
                GIT_REPO_LINKED,
                "/w",
                "/w/.git"
            )),
            "500201010000000802002f7707002f772f2e676974"
        );
        assert_eq!(hex(&msg_git_closed(1, GIT_CLOSED_REPO_GONE)), "52010001");
        assert_eq!(
            hex(&msg_git_base_resp(11, GIT_STATUS_OK, &[oid(0xAB)])),
            format!("5a0b000001{}", o(0xAB))
        );

        // Records buffers, uncompressed.
        let mut state = Vec::new();
        append_git_state_record(
            &mut state,
            &GitStateRecord::Head {
                flags: 0,
                oid: oid(0x01),
                name: "refs/heads/main",
            },
        );
        append_git_state_record(
            &mut state,
            &GitStateRecord::Upstream {
                flags: GIT_UPSTREAM_COUNTS_VALID,
                ahead: 2,
                behind: 3,
                name: "refs/heads/main",
                upstream: "refs/remotes/origin/main",
            },
        );
        assert_eq!(
            hex(&state),
            "33000000010001010101010101010101010101010101010101010000000000000000000000000f00726566732f68656164732f6d61696e35000000050202000000030000000f00726566732f68656164732f6d61696e1800726566732f72656d6f7465732f6f726967696e2f6d61696e"
        );

        let mut commits = Vec::new();
        append_git_commit_record(
            &mut commits,
            &GitCommitRecord::Commit {
                flags: GIT_COMMIT_LOSSY_ENCODING,
                oid: oid(0x0A),
                tree: oid(0x0B),
                parents: vec![oid(0x0C)],
                author_time: 1_700_000_000,
                author_tz: 60,
                committer_time: 1_700_000_001,
                committer_tz: -300,
                author_name: "Ann Author",
                author_email: "ann@example.com",
                committer_name: "Cam Committer",
                committer_email: "cam@example.com",
                message: "subject",
            },
        );
        append_git_commit_record(
            &mut commits,
            &GitCommitRecord::PathAt {
                otype: GIT_OTYPE_BLOB,
                mode: 0o100644,
                oid: oid(0x0D),
                path: "src/lib.rs",
            },
        );
        assert_eq!(
            hex(&commits),
            "bf00000001010a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0000000000000000000000000b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b000000000000000000000000010c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c00000000000000000000000000f15365000000003c0001f1536500000000d4fe0a00416e6e20417574686f720f00616e6e406578616d706c652e636f6d0d0043616d20436f6d6d69747465720f0063616d406578616d706c652e636f6d070000007375626a656374320000000203a48100000d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0d0000000000000000000000000a007372632f6c69622e7273"
        );

        let mut tree = Vec::new();
        append_git_tree_record(
            &mut tree,
            &GitTreeRecord::Entry {
                otype: GIT_OTYPE_TREE,
                mode: 0o40000,
                oid: oid(0x0E),
                name: "src",
            },
        );
        append_git_tree_record(
            &mut tree,
            &GitTreeRecord::Entry {
                otype: GIT_OTYPE_BLOB,
                mode: 0o100644,
                oid: oid(0x0F),
                name: "%FF.bin",
            },
        );
        assert_eq!(
            hex(&tree),
            "2b0000000202004000000e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e0e00000000000000000000000003007372632f0000000203a48100000f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f00000000000000000000000007002546462e62696e"
        );

        let mut diff = Vec::new();
        append_git_diff_record(&mut diff, &GitDiffRecord::Base { oid: oid(0x10) });
        append_git_diff_record(
            &mut diff,
            &GitDiffRecord::Entry {
                st: b'R',
                similarity: 90,
                dflags: 0,
                old_mode: 0o100644,
                new_mode: 0o100644,
                old_oid: oid(0x11),
                new_oid: oid(0x12),
                old_path: "old.txt",
                new_path: "new.txt",
            },
        );
        assert_eq!(
            hex(&diff),
            "210000000410101010101010101010101010101010101010100000000000000000000000005e00000003525a00a4810000a48100001111111111111111111111111111111111111111000000000000000000000000121212121212121212121212121212121212121200000000000000000000000007006f6c642e74787407006e65772e747874"
        );

        let mut patch = Vec::new();
        append_git_patch_record(
            &mut patch,
            &GitPatchRecord::File {
                flags: 0,
                old_path: "a.txt",
                new_path: "a.txt",
            },
        );
        append_git_patch_record(
            &mut patch,
            &GitPatchRecord::Row {
                old_line: 1,
                new_line: 1,
                old_text: b"hello",
                new_text: b"hallo",
                old_spans: vec![(1, 1)],
                new_spans: vec![(1, 1)],
            },
        );
        append_git_patch_record(
            &mut patch,
            &GitPatchRecord::Gap {
                old_line: 3,
                new_line: 3,
            },
        );
        assert_eq!(
            hex(&patch),
            "1000000001000500612e7478740500612e7478742f0000000201000000010000000500000068656c6c6f0500000068616c6c6f010001000000010000000100010000000100000009000000030300000003000000"
        );

        let mut index = Vec::new();
        append_git_index_record(
            &mut index,
            &GitIndexRecord::Entry {
                stage: 0,
                iflags: GIT_INDEX_INTENT_TO_ADD,
                mode: 0o100644,
                size: 5,
                mtime_ns: 1_700_000_000_000_000_000,
                oid: oid(0x14),
                path: "a.txt",
            },
        );
        assert_eq!(
            hex(&index),
            "3e000000040001a4810000050000000000000000002a36fe9c971714141414141414141414141414141414141414140000000000000000000000000500612e747874"
        );

        // Decode direction: the pinned bytes parse back to the same records.
        assert_eq!(git_state_records(&state).count(), 2);
        assert_eq!(git_commit_records(&commits).count(), 2);
        assert_eq!(git_tree_records(&tree).count(), 2);
        assert_eq!(git_diff_records(&diff).count(), 2);
        assert_eq!(git_patch_records(&patch).count(), 3);
        assert_eq!(git_index_records(&index).count(), 1);
    }
}
