//! Filesystem state sync wire protocol (docs/fs-watch.md).
//!
//! The server maintains a canonical replica of a watched tree and streams
//! per-client state diffs (`FS_UPDATE`). Clients apply records to a map and
//! acknowledge. Snapshots and recovery are `RESET … SYNC` staged series;
//! loss and overflow are not wire concepts.
//!
//! All integers little-endian, tightly packed, as everywhere in the protocol.

use std::collections::BTreeMap;

// Paths, globs and match text are bounded by no protocol rule, so `push_str`
// clips rather than wrapping the `u16` prefix.
use crate::push_str;

/// Start (or replace) a sync: [0x40][nonce:2][flags:2][latency_ms:2][inline_max:4][path_len:2][path:N]
/// then, when `FS_SYNC_EXCLUDE` is set, [exclude_len:2][exclude:M]; then,
/// when `FS_SYNC_FROM_PTY` is set, [src_pty_id:2].
pub const C2S_FS_SYNC: u8 = 0x40;
/// Stop a sync: [0x41][sync_id:2]
pub const C2S_FS_STOP: u8 = 0x41;
/// Cumulative acknowledgement: [0x42][sync_id:2][update_id:4]
pub const C2S_FS_ACK: u8 = 0x42;
/// Fetch full content of one file: [0x43][nonce:2][sync_id:2][path_len:2][path:N]
pub const C2S_FS_FETCH: u8 = 0x43;
/// Write file content (CAS): [0x44][nonce:2][sync_id:2][flags:1][base:16][mode:4][content_kind:1][path_len:2][path:N][content:LZ4]
pub const C2S_FS_WRITE: u8 = 0x44;
/// Metadata op (mkdir/remove/rename): [0x45][nonce:2][sync_id:2][op:1][flags:1][base:16][mode:4][a_len:2][a:N][b_len:2][b:N]
pub const C2S_FS_OP: u8 = 0x45;
/// Fuzzy file search under a root (no sync): [0x46][nonce:2][limit:2][root_len:2][root:N][query_len:2][query:N].
/// Returns paths (root-relative) whose basename subsequence-matches `query`.
pub const C2S_FS_SEARCH: u8 = 0x46;
/// Fetch the candidate file list under a root (no sync), for client-side
/// fuzzy search (docs/design/fs-search.md): [0x47][nonce:2][flags:1][root_len:2][root:N].
/// `flags` is `FS_INDEX_DIRS_ONLY`; unknown bits answer `INVALID`.
pub const C2S_FS_INDEX: u8 = 0x47;

/// Content search under a root (no sync), docs/design/fs-grep.md:
/// [0x48][nonce:2][flags:1][max_matches:2][max_per_file:2][root_len:2][root:N][query_len:2][query:N].
/// `flags` is `FS_GREP_CASE_SENSITIVE` | `FS_GREP_REGEX`; zero maxima mean
/// the server defaults. Unlike `FS_INDEX` the walk does not skip ignored
/// files — it ranks them last.
pub const C2S_FS_GREP: u8 = 0x48;

/// Begin a chunked upload into a synced root:
/// [0x49][nonce:2][sync_id:2][flags:1][base:16][mode:4][size:8][path_len:2][path:N].
/// `flags` is `FS_UPLOAD_*`; `base` is the CAS precondition with `FS_WRITE`'s
/// exact semantics; `size` is the total plaintext bytes; `path` is relative
/// to the sync root with the same %-encoding and traversal validation as
/// `FS_WRITE`.
pub const C2S_FS_UPLOAD_BEGIN: u8 = 0x49;
/// Append one chunk: [0x4A][upload_id:2][offset:8][data:LZ4]. Chunks are
/// sequential — `offset` must equal the bytes accepted so far.
pub const C2S_FS_UPLOAD_CHUNK: u8 = 0x4A;
/// Land the upload: [0x4B][nonce:2][upload_id:2]. Terminates the upload
/// whatever the outcome.
pub const C2S_FS_UPLOAD_FINISH: u8 = 0x4B;
/// Abort the upload: [0x4C][upload_id:2]. No reply.
pub const C2S_FS_UPLOAD_CANCEL: u8 = 0x4C;

/// Read whole files without a sync (docs/design/fs-read.md):
/// [0x4D][nonce:2][flags:1][max_bytes:4][group_count:2] then group_count ×
/// ( [path_count:2] then path_count × [path_len:2][path:N] ).
///
/// The family's one-shot read. `FS_FETCH` needs an established sync — a watched
/// tree, which is the wrong shape for reading a fixed set of files once — so
/// everything that wanted a handful of files had to sync a directory it did not
/// want watched, or shell out to `cat`. Paths are absolute and independent;
/// nothing is watched and no state is kept.
///
/// `max_bytes` is the per-file ceiling, zero meaning [`FS_READ_DEFAULT_BYTES`];
/// a larger file is reported rather than read, so a caller drawing 2em tiles
/// does not have a theme's megabyte SVG pushed at it. `flags` is `FS_READ_FIRST`.
///
/// Paths are grouped, and a group is one question: with `FS_READ_FIRST` each
/// group is answered by its own first readable path, so one message can resolve
/// a whole screenful of icons rather than one message per icon.
pub const C2S_FS_READ: u8 = 0x4D;

/// Sync accepted or rejected: [0x40][nonce:2][sync_id:2][status:1][detail_len:2][detail:N]
/// On success detail is the canonical root (UTF-8); on failure a diagnostic.
pub const S2C_FS_SYNCED: u8 = 0x40;
/// State diff: [0x41][sync_id:2][update_id:4][flags:1][records:LZ4]
pub const S2C_FS_UPDATE: u8 = 0x41;
/// Fetch response: [0x42][nonce:2][status:1][data:LZ4]
pub const S2C_FS_FILE: u8 = 0x42;
/// Sync terminated: [0x43][sync_id:2][reason:1]
pub const S2C_FS_CLOSED: u8 = 0x43;
/// Write/op result: [0x44][nonce:2][status:1][hash:16][mtime_ns:8]
pub const S2C_FS_DONE: u8 = 0x44;
/// Search result: [0x45][nonce:2][status:1][count:2] repeated{ [path_len:2][path:N] }
pub const S2C_FS_SEARCH: u8 = 0x45;
/// Index result: [0x46][nonce:2][status:1][flags:1][count:4][paths:LZ4]
/// where the decompressed payload is repeated{ [path_len:2][path:N] },
/// root-relative, sorted. Status uses the common registry (`FS_DONE_*`).
pub const S2C_FS_INDEX: u8 = 0x46;

/// Grep result: [0x47][nonce:2][status:1][flags:1][detail_len:2][detail:N][records:LZ4]
/// where the decompressed payload is a `[record_len:4][kind:1][..]` stream of
/// `FILE`/`MATCH` records (docs/design/fs-grep.md). Status uses the common
/// registry (`FS_DONE_*`); `detail` carries a regex compile error on `INVALID`.
pub const S2C_FS_GREP: u8 = 0x47;

/// Upload begin result:
/// [0x49][nonce:2][status:1][upload_id:2][hash:16][mtime_ns:8] —
/// `upload_id` is meaningful only when `status` is `FS_DONE_OK`; `hash`
/// carries the current on-disk content hash on `CONFLICT` (the `FS_DONE`
/// convention), zero otherwise.
pub const S2C_FS_UPLOAD_BEGIN: u8 = 0x49;
/// Per-chunk acknowledgement (doubles as progress):
/// [0x4A][upload_id:2][status:1][received:8] — `received` is the cumulative
/// plaintext bytes accepted; on `OFFSET_MISMATCH` it is the resume point.
pub const S2C_FS_UPLOAD_CHUNK: u8 = 0x4A;
/// Upload result: [0x4B][nonce:2][status:1][hash:16][mtime_ns:8] — the
/// `FS_DONE` payload on success (zeroes otherwise), or the current on-disk
/// hash on `CONFLICT` (the precondition re-verified at FINISH failed).
pub const S2C_FS_UPLOAD_FINISH: u8 = 0x4B;

/// Read result: [0x48][nonce:2][status:1][count:2][records:LZ4] where the
/// decompressed payload is repeated{ [status:1][path_len:2][path:N][size:4][data:size] },
/// in the order the request asked for. Request status uses the common registry
/// (`FS_DONE_*`); each record carries its own `FS_FILE_*`, so one unreadable
/// path does not spoil the answer for the rest.
pub const S2C_FS_READ: u8 = 0x48;

/// `S2C_HELLO` feature bit: server supports the `FS_*` message family,
/// reads and writes alike (docs/design/fs-watch.md, docs/design/fs-write.md).
/// A read-only deployment (`BLIT_FS_WRITE=0`) still advertises this bit and
/// answers `FS_WRITE`/`FS_OP` with `FS_DONE_PERMISSION`.
pub const FEATURE_FS: u32 = 1 << 6;

/// `sync_id` reported by a failed `FS_SYNCED`.
pub const FS_SYNC_ID_INVALID: u16 = 0xFFFF;

// C2S_FS_SYNC flags. Two bytes: the exclusion work filled the first one,
// and a 1-byte field with no room left is a field that forces the next
// feature into a worse encoding.
pub const FS_SYNC_RECURSIVE: u16 = 1 << 0;
pub const FS_SYNC_CONTENT: u16 = 1 << 1;
pub const FS_SYNC_CROSS_FILESYSTEM: u16 = 1 << 2;
/// The sync root is a single FILE, not a directory: the mirror holds
/// exactly one entry — the root itself — keyed by the empty relative path
/// "" (the same key a directory sync gives its root). Combining with
/// `RECURSIVE` is invalid, and a directory root answers the invalid-path
/// error (docs/design/fs-watch.md "Single-file sync"). Content,
/// `inline_max`, `FS_FETCH`, and the write family behave as for any other
/// sync, addressing path "".
pub const FS_SYNC_SINGLE: u16 = 1 << 3;
/// Resolve the sync's base directory from a pty's live cwd: a trailing
/// `[src_pty_id:2]` names a pty and the server joins `path` onto its cwd
/// (docs/ide.md Decision 3). It comes last, after any `EXCLUDE` field.
pub const FS_SYNC_FROM_PTY: u16 = 1 << 4;
/// Omit every entry whose final component is exactly `.git` — directory or
/// gitfile — from enumeration, hashing, hints, and records. A pure name
/// filter: no git data is read (docs/design/fs-watch.md "Ignoring").
pub const FS_SYNC_EXCLUDE_GIT: u16 = 1 << 5;
/// Honor `.gitignore` in and above the root, plus the governing
/// repository's `$GIT_DIR/info/exclude`, the user's `core.excludesFile`,
/// and its `core.ignorecase`. Off by default, so a sync only narrows when
/// asked.
pub const FS_SYNC_GITIGNORE: u16 = 1 << 6;
/// A trailing `[exclude_len:2][exclude:M]` carries client patterns —
/// gitignore syntax, one per line, anchored at the sync root and applied
/// above every other rule, so `!keep` re-includes. The flag is what makes
/// the field parseable, and what makes a server too old to filter refuse
/// the sync outright instead of silently mirroring the whole tree.
pub const FS_SYNC_EXCLUDE: u16 = 1 << 7;
/// Honor `.ignore` in and above the root — ripgrep's convention, which a
/// project uses to hide things from tooling without telling git to stop
/// tracking them. Separate from `GITIGNORE` because the two answer
/// different questions, and `.ignore` brings none of git's repository-wide
/// sources with it. `FS_INDEX` and `FS_GREP` apply both together; a sync
/// picks.
pub const FS_SYNC_DOTIGNORE: u16 = 1 << 8;
/// Resolve the sync root to the connection's drag staging dir instead of
/// `path` (which the client sends empty): the dir a browser drag-and-drop
/// pre-uploads its files into before `SURFACE_DRAG_DROP` names them
/// (docs/protocol.md "Drag and drop"). The server creates the dir on first
/// use and removes it on connection close — never on `FS_STOP`, since the
/// staged `file://` URIs must outlive the drop. The staging root belongs to
/// the connection, so combining with `FROM_PTY` is invalid.
pub const FS_SYNC_STAGING: u16 = 1 << 9;

/// Bits a `C2S_FS_SYNC` may set; anything else answers with the
/// unknown-flags refusal.
pub const FS_SYNC_FLAGS_KNOWN: u16 = FS_SYNC_RECURSIVE
    | FS_SYNC_CONTENT
    | FS_SYNC_CROSS_FILESYSTEM
    | FS_SYNC_SINGLE
    | FS_SYNC_FROM_PTY
    | FS_SYNC_EXCLUDE_GIT
    | FS_SYNC_GITIGNORE
    | FS_SYNC_EXCLUDE
    | FS_SYNC_DOTIGNORE
    | FS_SYNC_STAGING;

/// Every exclusion flag, which is also the set `SINGLE` rejects.
pub const FS_SYNC_EXCLUSION_FLAGS: u16 =
    FS_SYNC_EXCLUDE_GIT | FS_SYNC_GITIGNORE | FS_SYNC_EXCLUDE | FS_SYNC_DOTIGNORE;

/// `C2S_FS_SYNC` flag-combination validity: `SINGLE` syncs exactly one
/// file, so `RECURSIVE` contradicts it and the pair is rejected at
/// validation (docs/design/fs-watch.md "Single-file sync"). The exclusion
/// flags apply to enumeration, which `SINGLE` does none of, so they are
/// rejected with it too rather than silently doing nothing. `STAGING`
/// resolves the root from connection state, so `FROM_PTY`'s pty-relative
/// root contradicts it — also rejected here.
pub fn fs_sync_flags_valid(flags: u16) -> bool {
    if flags & FS_SYNC_STAGING != 0 && flags & FS_SYNC_FROM_PTY != 0 {
        return false;
    }
    flags & FS_SYNC_SINGLE == 0 || flags & (FS_SYNC_RECURSIVE | FS_SYNC_EXCLUSION_FLAGS) == 0
}

// S2C_FS_UPDATE flags.
/// Begin a staged snapshot: apply this and subsequent records to an empty
/// staging map instead of the live map.
pub const FS_UPDATE_RESET: u8 = 1 << 0;
/// Atomically replace the live map with the staging map (no-op without one).
pub const FS_UPDATE_SYNC: u8 = 1 << 1;

// S2C_FS_SYNCED status.
pub const FS_STATUS_OK: u8 = 0;
pub const FS_STATUS_NOT_FOUND: u8 = 1;
pub const FS_STATUS_PERMISSION_DENIED: u8 = 2;
pub const FS_STATUS_RESOURCE_LIMIT: u8 = 3;
pub const FS_STATUS_OTHER: u8 = 4;

// S2C_FS_INDEX flags.
/// The walk hit its entry or byte budget; the list is a prefix, not the
/// whole tree. Clients should keep server-side `FS_SEARCH` for this root.
pub const FS_INDEX_TRUNCATED: u8 = 1 << 0;
/// Protocol cap on `S2C_FS_INDEX.count`. The server's entry budget clamps
/// to this, and parsers treat a larger count as malformed — without it, a
/// hostile count of tiny records could force a giant preallocation from a
/// small frame (the decompression guard bounds bytes, not record counts).
pub const FS_INDEX_MAX_COUNT: usize = 1_000_000;

// S2C_FS_FILE status, shared by each `S2C_FS_READ` record.
pub const FS_FILE_OK: u8 = 0;
pub const FS_FILE_NOT_FOUND: u8 = 1;
pub const FS_FILE_UNREADABLE: u8 = 2;
pub const FS_FILE_OTHER: u8 = 3;
/// The file exists and was not read: it is over the request's `max_bytes`, or
/// over what was left of the response budget. Either way the caller has the
/// answer it needs — this path is not going to arrive — and can look elsewhere.
pub const FS_FILE_TOO_LARGE: u8 = 4;

// C2S_FS_READ flags (docs/design/fs-read.md).

/// Answer each group with the first path in it that can be read.
///
/// This is the search-path question — the first of these that exists, in my
/// order of preference — which is otherwise a round trip per candidate. A path
/// that is missing, unreadable or too large is stepped over rather than
/// answered. There is exactly one record per group, in group order: a group that
/// matched nothing carries `FS_FILE_NOT_FOUND` and an empty path, so a caller
/// can align answers with questions by position.
pub const FS_READ_FIRST: u8 = 1 << 0;

/// Answer which path, not what is in it.
///
/// The resolution without the transfer: a caller that only needs to know *where*
/// something is — because it will hand the path to whoever actually wants the
/// bytes — pays a stat instead of a read. Records carry their status and path
/// with an empty body, and `max_bytes` still applies, so "exists but too big to
/// be what I am looking for" is still answered as such.
pub const FS_READ_NO_CONTENT: u8 = 1 << 1;

/// The flags `FS_READ` understands; anything else answers `INVALID`.
pub const FS_READ_FLAGS_KNOWN: u8 = FS_READ_FIRST | FS_READ_NO_CONTENT;

/// Paths one `FS_READ` may name. Generous because the shape it replaces is a
/// batch: a panel asking for a screenful of artwork, a supervisor reading every
/// `.desktop` file in a directory.
pub const FS_READ_MAX_PATHS: usize = 512;
/// Per-file ceiling when the request asks for none.
pub const FS_READ_DEFAULT_BYTES: u32 = 1024 * 1024;
/// Bytes of file content one reply carries. Whatever does not fit is reported
/// `FS_FILE_TOO_LARGE` rather than silently dropped, so a caller can re-ask for
/// the remainder in smaller batches.
pub const FS_READ_MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

// C2S_FS_INDEX flags.

/// List directories instead of files.
///
/// The shape of a tree without its contents, which is what a search path is:
/// an icon theme is fifty directories holding fifty thousand files, and a
/// caller that wants somewhere to look should not be handed all of them.
pub const FS_INDEX_DIRS_ONLY: u8 = 1 << 0;

/// Descend through symbolic links to directories.
///
/// Off by default, because a source tree's links can point anywhere and a walk
/// that follows them is a walk with no bound the caller chose. It exists because
/// some trees are *made* of links: on a Nix system every directory under
/// `/run/current-system/sw/share/icons` is one, so a walk that stops at them
/// reports a theme's name and nothing inside it.
pub const FS_INDEX_FOLLOW_LINKS: u8 = 1 << 1;

/// The flags `FS_INDEX` understands; anything else answers `INVALID`.
pub const FS_INDEX_FLAGS_KNOWN: u8 = FS_INDEX_DIRS_ONLY | FS_INDEX_FOLLOW_LINKS;

// FS_DONE status — the common registry (docs/protocol.md "Common status
// registry"), NOT FS_SYNCED's grandfathered 0-4.
// Same numeric values as `GIT_STATUS_*` where they overlap.
pub const FS_DONE_OK: u8 = 0;
pub const FS_DONE_NOT_FOUND: u8 = 2;
pub const FS_DONE_WRONG_TYPE: u8 = 3;
pub const FS_DONE_PERMISSION: u8 = 4;
pub const FS_DONE_TOO_LARGE: u8 = 5;
pub const FS_DONE_BUDGET: u8 = 6;
pub const FS_DONE_INVALID: u8 = 7;
pub const FS_DONE_OTHER: u8 = 9;

// C2S_FS_GREP flags (docs/design/fs-grep.md).

/// Match case exactly. Unset (the default) is case-insensitive.
pub const FS_GREP_CASE_SENSITIVE: u8 = 1 << 0;
/// `query` is a regex. Unset (the default) treats it as a literal string.
pub const FS_GREP_REGEX: u8 = 1 << 1;
/// Search gitignored files too, ranked after every tracked one. Unset (the
/// default) applies ignore rules and skips them — on a real repo that is
/// the difference between milliseconds and seconds, because the ignored
/// pass is what has to descend into `target/`.
pub const FS_GREP_NO_IGNORE: u8 = 1 << 2;
/// Match only whole words: the pattern is wrapped in `\b(?:...)\b` after
/// any literal escaping, so it composes with either mode. Same semantics
/// as `blit terminal grep --word-regexp`.
pub const FS_GREP_WORD: u8 = 1 << 3;
/// Bits a request may set; anything else answers `INVALID`.
pub const FS_GREP_FLAGS_KNOWN: u8 =
    FS_GREP_CASE_SENSITIVE | FS_GREP_REGEX | FS_GREP_NO_IGNORE | FS_GREP_WORD;

// S2C_FS_GREP flags.

/// A budget clipped the search: matches exist that are not in this response.
/// Exact — set only when something was actually dropped.
pub const FS_GREP_TRUNCATED: u8 = 1 << 0;

// S2C_FS_GREP record kinds.

pub const FS_GREP_RECORD_FILE: u8 = 0x01;
pub const FS_GREP_RECORD_MATCH: u8 = 0x02;

// FILE record flags.

/// The file is gitignored. It still gets searched — ignore rules rank
/// rather than filter here — but sorts after every non-ignored file.
pub const FS_GREP_FILE_IGNORED: u8 = 1 << 0;

/// Longest matched line returned, in bytes; longer lines are truncated on a
/// UTF-8 boundary so a minified bundle costs one line of wire, not one line
/// of megabyte.
pub const FS_GREP_MAX_LINE: usize = 512;
/// A precondition failed (CAS mismatch, create-exclusive on an existing
/// path, conditional remove on a changed file). On `CONFLICT`,
/// `FS_DONE.hash` carries the current on-disk hash so the client rebases
/// without a round trip. Added in lsp's `10 WARMING` extension style.
pub const FS_DONE_CONFLICT: u8 = 11;

// Chunked-upload statuses (FS_UPLOAD_*). Family-local allocations per the
// common status registry (docs/protocol.md), which reserves 128–255 for
// exactly this: a family the registry does not centralize.
/// A chunk's `offset` did not equal the bytes accepted so far. The
/// ack's `received` field carries the resume point.
pub const FS_DONE_OFFSET_MISMATCH: u8 = 128;
/// FINISH arrived with `received != size`; the upload is dropped.
pub const FS_DONE_SIZE_MISMATCH: u8 = 129;
/// The `upload_id` names no live upload on this connection.
pub const FS_DONE_UNKNOWN_UPLOAD: u8 = 130;

/// Human-readable name for an `FS_DONE` status code.
pub fn fs_done_status_text(status: u8) -> &'static str {
    match status {
        FS_DONE_OK => "ok",
        FS_DONE_NOT_FOUND => "not found",
        FS_DONE_WRONG_TYPE => "wrong type",
        FS_DONE_PERMISSION => "permission denied",
        FS_DONE_TOO_LARGE => "too large",
        FS_DONE_BUDGET => "budget exhausted",
        FS_DONE_INVALID => "invalid request",
        FS_DONE_OTHER => "backend error",
        FS_DONE_CONFLICT => "conflict",
        FS_DONE_OFFSET_MISMATCH => "offset mismatch",
        FS_DONE_SIZE_MISMATCH => "size mismatch",
        FS_DONE_UNKNOWN_UPLOAD => "unknown upload",
        _ => "unknown status",
    }
}

// FS_WRITE flags.
/// Ignore `base`; unconditional overwrite/create ("Save As, replace").
pub const FS_WRITE_NO_CAS: u8 = 1 << 0;
/// Create missing parent directories.
pub const FS_WRITE_MKPARENTS: u8 = 1 << 1;
/// fsync the file and its parent (F_FULLFSYNC on macOS) before returning.
pub const FS_WRITE_DURABLE: u8 = 1 << 2;
/// Write through a final-component symlink whose resolved target stays
/// under the root; default refuses one.
pub const FS_WRITE_FOLLOW_SYMLINK: u8 = 1 << 3;

// FS_WRITE content_kind: 0/1 are full bytes; 2 is a delta-against-`base`
// write — the COPY/INSERT instruction stream of `apply_fs_delta`, applied
// server-side against the exact bytes the CAS `base` names, so it
// requires a real base (NO_CAS or a zero base answers INVALID) and a
// stale base answers CONFLICT, never a corrupted apply
// (docs/design/fs-write.md "Wire"). A client may always send full.
pub const FS_WRITE_CONTENT_FULL: u8 = 1;
pub const FS_WRITE_CONTENT_DELTA: u8 = 2;

// FS_UPLOAD_BEGIN flags. Aliases of the FS_WRITE flags — identical bits,
// identical meanings, so a client moving a write between the one-shot and
// the chunked path keeps one flag set.
/// Ignore `base`; unconditional overwrite/create.
pub const FS_UPLOAD_NO_CAS: u8 = FS_WRITE_NO_CAS;
/// Create missing parent directories.
pub const FS_UPLOAD_MKPARENTS: u8 = FS_WRITE_MKPARENTS;
/// fsync the file and its parent before the FINISH rename lands.
pub const FS_UPLOAD_DURABLE: u8 = FS_WRITE_DURABLE;
/// Write through a final-component symlink whose resolved target stays
/// under the root; default refuses one.
pub const FS_UPLOAD_FOLLOW_SYMLINK: u8 = FS_WRITE_FOLLOW_SYMLINK;
/// Bits a `C2S_FS_UPLOAD_BEGIN` may set; anything else answers `INVALID`.
pub const FS_UPLOAD_FLAGS_KNOWN: u8 =
    FS_UPLOAD_NO_CAS | FS_UPLOAD_MKPARENTS | FS_UPLOAD_DURABLE | FS_UPLOAD_FOLLOW_SYMLINK;

// FS_OP op selector.
pub const FS_OP_MKDIR: u8 = 1;
pub const FS_OP_REMOVE: u8 = 2;
pub const FS_OP_RENAME: u8 = 3;
/// Create or retarget a symlink at `b` whose target is the verbatim string
/// `a` (not a wire path; not confined to the root). `base` CASes on the
/// current entry at `b` — a symlink's content hash is BLAKE3-128 of its
/// target bytes (docs/design/fs-write.md "Links").
pub const FS_OP_SYMLINK: u8 = 4;
/// Create a hard link at `b` to the regular file at `a` (both wire paths
/// under the root). `base` CASes on the current entry at `b`.
pub const FS_OP_HARDLINK: u8 = 5;

// FS_OP flags (subset of FS_WRITE's, same bit positions).
pub const FS_OP_NO_CAS: u8 = 1 << 0;
pub const FS_OP_MKPARENTS: u8 = 1 << 1;

// S2C_FS_CLOSED reasons.
pub const FS_CLOSED_CLIENT_REQUEST: u8 = 0;
pub const FS_CLOSED_ROOT_GONE: u8 = 1;
pub const FS_CLOSED_PERMISSION_LOST: u8 = 2;
pub const FS_CLOSED_BACKEND_FAILED: u8 = 3;
pub const FS_CLOSED_RESOURCE_LIMIT: u8 = 4;

// Record kinds inside FS_UPDATE.
pub const FS_RECORD_UPSERT: u8 = 0x01;
pub const FS_RECORD_DELETE: u8 = 0x02;
pub const FS_RECORD_MOVE: u8 = 0x03;

// UPSERT entry_flags: bits 0-1 node type, higher bits flags.
pub const FS_ENTRY_TYPE_MASK: u8 = 0b11;
pub const FS_ENTRY_FILE: u8 = 0;
pub const FS_ENTRY_DIR: u8 = 1;
pub const FS_ENTRY_SYMLINK: u8 = 2;
pub const FS_ENTRY_OTHER: u8 = 3;
/// Entry exists but its content could not be read.
pub const FS_ENTRY_UNREADABLE: u8 = 1 << 2;
/// Content omitted: over `inline_max` or the sync did not request content.
pub const FS_ENTRY_NO_CONTENT: u8 = 1 << 3;
/// File changed repeatedly while being read; content omitted, another
/// upsert follows once it settles.
pub const FS_ENTRY_UNSTABLE: u8 = 1 << 4;
/// Set on an `FS_ENTRY_SYMLINK` whose target is a directory, which the sync
/// enumerates like any other. Clients need it to know the entry is expandable:
/// the type alone cannot distinguish a link to a directory from one to a file,
/// and a non-recursive sync has no children listed yet to infer it from.
pub const FS_ENTRY_LINK_DIR: u8 = 1 << 5;
/// Set on a directory whose enumeration skipped at least one child the
/// sync's exclusion rules cover (docs/design/fs-watch.md "Ignoring").
/// Excluded paths are absent rather than marked, so without this a client
/// cannot tell an empty directory from a filtered one — a file browser
/// needs it to say "some items hidden" instead of showing a folder that
/// looks wrong.
///
/// Prompt when it goes up, lazy when it comes down: the first excluded
/// child costs one re-listing of its directory, while the *last* one
/// disappearing clears the flag only at that directory's next enumeration.
/// Chasing the clear would mean re-listing on every excluded-file event,
/// which is the cost the exclusion exists to avoid — so a client may
/// briefly see "hidden items" on a directory that no longer has any.
pub const FS_ENTRY_FILTERED: u8 = 1 << 6;

// UPSERT content kinds.
pub const FS_CONTENT_NONE: u8 = 0;
pub const FS_CONTENT_FULL: u8 = 1;
pub const FS_CONTENT_DELTA: u8 = 2;

/// One decoded record from an `FS_UPDATE` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsRecord<'a> {
    Upsert {
        path: &'a str,
        entry_flags: u8,
        size: u64,
        mtime_ns: u64,
        mode: u32,
        /// BLAKE3 truncated to 128 bits; zero for non-files or unknown.
        hash: u128,
        content: FsContent<'a>,
    },
    /// Remove `path` and every path under it.
    Delete { path: &'a str },
    /// Rename the `from` subtree to `to`.
    Move { from: &'a str, to: &'a str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsContent<'a> {
    None,
    Full(&'a [u8]),
    /// LEB128 instruction stream against the last content this client
    /// acked for this path: 0x01 COPY [offset][len], 0x02 INSERT [len][bytes].
    Delta(&'a [u8]),
}

/// Append one record to an uncompressed `FS_UPDATE` records buffer.
pub fn append_fs_record(buf: &mut Vec<u8>, record: &FsRecord<'_>) {
    let start = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // record_len placeholder
    match record {
        FsRecord::Upsert {
            path,
            entry_flags,
            size,
            mtime_ns,
            mode,
            hash,
            content,
        } => {
            buf.push(FS_RECORD_UPSERT);
            buf.push(*entry_flags);
            push_str(buf, path);
            buf.extend_from_slice(&size.to_le_bytes());
            buf.extend_from_slice(&mtime_ns.to_le_bytes());
            buf.extend_from_slice(&mode.to_le_bytes());
            buf.extend_from_slice(&hash.to_le_bytes());
            match content {
                FsContent::None => buf.push(FS_CONTENT_NONE),
                FsContent::Full(data) => {
                    buf.push(FS_CONTENT_FULL);
                    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                    buf.extend_from_slice(data);
                }
                FsContent::Delta(ops) => {
                    buf.push(FS_CONTENT_DELTA);
                    buf.extend_from_slice(&(ops.len() as u32).to_le_bytes());
                    buf.extend_from_slice(ops);
                }
            }
        }
        FsRecord::Delete { path } => {
            buf.push(FS_RECORD_DELETE);
            push_str(buf, path);
        }
        FsRecord::Move { from, to } => {
            buf.push(FS_RECORD_MOVE);
            push_str(buf, from);
            push_str(buf, to);
        }
    }
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Iterate records in an uncompressed `FS_UPDATE` payload.
/// Unknown kinds are skipped via `record_len`; a malformed record ends
/// iteration (the update is applied up to that point and the rest dropped —
/// forward-compatible with future record extensions).
pub struct FsRecordIter<'a> {
    data: &'a [u8],
}

pub fn fs_records(data: &[u8]) -> FsRecordIter<'_> {
    FsRecordIter { data }
}

fn take_path<'a>(body: &mut &'a [u8]) -> Option<&'a str> {
    if body.len() < 2 {
        return None;
    }
    let len = u16::from_le_bytes([body[0], body[1]]) as usize;
    if body.len() < 2 + len {
        return None;
    }
    let s = std::str::from_utf8(&body[2..2 + len]).ok()?;
    *body = &body[2 + len..];
    Some(s)
}

impl<'a> Iterator for FsRecordIter<'a> {
    type Item = FsRecord<'a>;

    fn next(&mut self) -> Option<FsRecord<'a>> {
        loop {
            if self.data.len() < 4 {
                return None;
            }
            let rec_len =
                u32::from_le_bytes([self.data[0], self.data[1], self.data[2], self.data[3]])
                    as usize;
            if self.data.len() < 4 + rec_len || rec_len == 0 {
                return None;
            }
            let mut body = &self.data[4..4 + rec_len];
            self.data = &self.data[4 + rec_len..];
            let kind = body[0];
            body = &body[1..];
            match kind {
                FS_RECORD_UPSERT => {
                    if body.is_empty() {
                        return None;
                    }
                    let entry_flags = body[0];
                    body = &body[1..];
                    let path = take_path(&mut body)?;
                    if body.len() < 8 + 8 + 4 + 16 + 1 {
                        return None;
                    }
                    let size = u64::from_le_bytes(body[0..8].try_into().unwrap());
                    let mtime_ns = u64::from_le_bytes(body[8..16].try_into().unwrap());
                    let mode = u32::from_le_bytes(body[16..20].try_into().unwrap());
                    let hash = u128::from_le_bytes(body[20..36].try_into().unwrap());
                    let content_kind = body[36];
                    body = &body[37..];
                    let content = match content_kind {
                        FS_CONTENT_NONE => FsContent::None,
                        FS_CONTENT_FULL | FS_CONTENT_DELTA => {
                            if body.len() < 4 {
                                return None;
                            }
                            let len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
                            if body.len() < 4 + len {
                                return None;
                            }
                            let data = &body[4..4 + len];
                            if content_kind == FS_CONTENT_FULL {
                                FsContent::Full(data)
                            } else {
                                FsContent::Delta(data)
                            }
                        }
                        _ => return None,
                    };
                    return Some(FsRecord::Upsert {
                        path,
                        entry_flags,
                        size,
                        mtime_ns,
                        mode,
                        hash,
                        content,
                    });
                }
                FS_RECORD_DELETE => {
                    let path = take_path(&mut body)?;
                    return Some(FsRecord::Delete { path });
                }
                FS_RECORD_MOVE => {
                    let from = take_path(&mut body)?;
                    let to = take_path(&mut body)?;
                    return Some(FsRecord::Move { from, to });
                }
                _ => continue, // unknown kind: skip via record_len
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Message builders
// ---------------------------------------------------------------------------

pub fn msg_fs_sync(
    nonce: u16,
    flags: u16,
    latency_ms: u16,
    inline_max: u32,
    path: &str,
) -> Vec<u8> {
    msg_fs_sync_full(nonce, flags, latency_ms, inline_max, path, "", None)
}

/// Build a `C2S_FS_SYNC` carrying client exclude patterns: gitignore
/// syntax, one per line, anchored at the sync root
/// (docs/design/fs-watch.md "Ignoring"). Sets `FS_SYNC_EXCLUDE`; an empty
/// `exclude` builds the plain form instead, so a caller need not special-case
/// "no patterns".
pub fn msg_fs_sync_excluding(
    nonce: u16,
    flags: u16,
    latency_ms: u16,
    inline_max: u32,
    path: &str,
    exclude: &str,
) -> Vec<u8> {
    msg_fs_sync_full(nonce, flags, latency_ms, inline_max, path, exclude, None)
}

/// Build a `C2S_FS_SYNC` whose base directory the server resolves from a pty's
/// live cwd: sets `FS_SYNC_FROM_PTY` and appends `[src_pty_id:2]` last
/// (docs/ide.md Decision 3). `path` is joined onto the resolved cwd
/// server-side (empty = the cwd itself).
pub fn msg_fs_sync_from_pty(
    nonce: u16,
    flags: u16,
    latency_ms: u16,
    inline_max: u32,
    path: &str,
    src_pty_id: u16,
) -> Vec<u8> {
    msg_fs_sync_full(
        nonce,
        flags,
        latency_ms,
        inline_max,
        path,
        "",
        Some(src_pty_id),
    )
}

/// Build a `C2S_FS_SYNC` rooted at the connection's drag staging dir: sets
/// `FS_SYNC_STAGING` and sends an empty `path`, which the flag makes the
/// server ignore (docs/protocol.md "Drag and drop").
pub fn msg_fs_sync_staging(nonce: u16, flags: u16, latency_ms: u16, inline_max: u32) -> Vec<u8> {
    msg_fs_sync_full(
        nonce,
        flags | FS_SYNC_STAGING,
        latency_ms,
        inline_max,
        "",
        "",
        None,
    )
}

/// Every `C2S_FS_SYNC` variant, in field order. The optional trailers are
/// self-describing through their flags — `EXCLUDE` first, `FROM_PTY` last —
/// which is what lets a parser skip one to reach the other.
pub fn msg_fs_sync_full(
    nonce: u16,
    flags: u16,
    latency_ms: u16,
    inline_max: u32,
    path: &str,
    exclude: &str,
    src_pty_id: Option<u16>,
) -> Vec<u8> {
    let pb = path.as_bytes();
    let eb = exclude.as_bytes();
    let mut flags = flags;
    if eb.is_empty() {
        flags &= !FS_SYNC_EXCLUDE;
    } else {
        flags |= FS_SYNC_EXCLUDE;
    }
    if src_pty_id.is_some() {
        flags |= FS_SYNC_FROM_PTY;
    }
    let mut msg = Vec::with_capacity(FS_SYNC_HEADER + pb.len() + eb.len() + 4);
    msg.push(C2S_FS_SYNC);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&flags.to_le_bytes());
    msg.extend_from_slice(&latency_ms.to_le_bytes());
    msg.extend_from_slice(&inline_max.to_le_bytes());
    push_str(&mut msg, path);
    if !eb.is_empty() {
        push_str(&mut msg, exclude);
    }
    if let Some(src) = src_pty_id {
        msg.extend_from_slice(&src.to_le_bytes());
    }
    msg
}

/// Fixed part of `C2S_FS_SYNC`, up to and including `path_len`.
pub const FS_SYNC_HEADER: usize = 13;

/// The `flags` field of a `C2S_FS_SYNC`, or `None` if it is truncated.
pub fn fs_sync_flags(msg: &[u8]) -> Option<u16> {
    if msg.first().copied() != Some(C2S_FS_SYNC) || msg.len() < FS_SYNC_HEADER {
        return None;
    }
    Some(u16::from_le_bytes([msg[3], msg[4]]))
}

/// End of the `path` field, i.e. the offset of the first trailer.
fn fs_sync_trailer_start(msg: &[u8]) -> Option<usize> {
    if msg.first().copied() != Some(C2S_FS_SYNC) || msg.len() < FS_SYNC_HEADER {
        return None;
    }
    let path_len = u16::from_le_bytes([msg[11], msg[12]]) as usize;
    let end = FS_SYNC_HEADER.checked_add(path_len)?;
    (end <= msg.len()).then_some(end)
}

/// Byte range of the `exclude` payload in an `EXCLUDE` `C2S_FS_SYNC`, and
/// the offset just past it. `None` when the flag is unset or the field is
/// truncated — the caller refuses the request rather than guessing.
fn fs_sync_exclude_span(msg: &[u8]) -> Option<(std::ops::Range<usize>, usize)> {
    let off = fs_sync_trailer_start(msg)?;
    if fs_sync_flags(msg)? & FS_SYNC_EXCLUDE == 0 {
        return Some((off..off, off));
    }
    let len_bytes = msg.get(off..off + 2)?;
    let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let start = off + 2;
    let end = start.checked_add(len)?;
    (end <= msg.len()).then_some((start..end, end))
}

/// Client exclude patterns from a `C2S_FS_SYNC` — `""` when `EXCLUDE` is
/// unset. `None` means malformed: a truncated field or non-UTF-8 patterns.
pub fn fs_sync_exclude(msg: &[u8]) -> Option<&str> {
    let (span, _) = fs_sync_exclude_span(msg)?;
    std::str::from_utf8(&msg[span]).ok()
}

/// Extract the trailing `src_pty_id` from a `FROM_PTY` `C2S_FS_SYNC`; `None`
/// when the flag is unset or the field is missing.
pub fn fs_sync_src_pty(msg: &[u8]) -> Option<u16> {
    if fs_sync_flags(msg)? & FS_SYNC_FROM_PTY == 0 {
        return None;
    }
    let (_, off) = fs_sync_exclude_span(msg)?;
    let b = msg.get(off..off + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

/// Rebase a `FROM_PTY` `C2S_FS_SYNC` onto a resolved `cwd`: join `cwd`/`path`
/// and clear `FROM_PTY`, producing a plain path-based sync the handler
/// consumes unchanged. A caller that cannot resolve the source pty's cwd must
/// refuse the request rather than forward it — the pty-relative path (the
/// dock's follow-terminal root is `""`) would otherwise be read as absolute.
/// Any exclude field rides along — the filter is the client's, not the
/// pty's, and dropping it here would silently widen the sync.
pub fn fs_sync_rebase(msg: &[u8], cwd: &str) -> Option<Vec<u8>> {
    fs_sync_src_pty(msg)?;
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let flags = fs_sync_flags(msg)? & !FS_SYNC_FROM_PTY;
    let latency_ms = u16::from_le_bytes([msg[5], msg[6]]);
    let inline_max = u32::from_le_bytes([msg[7], msg[8], msg[9], msg[10]]);
    let path_len = u16::from_le_bytes([msg[11], msg[12]]) as usize;
    let path = std::str::from_utf8(msg.get(FS_SYNC_HEADER..FS_SYNC_HEADER + path_len)?).ok()?;
    let exclude = fs_sync_exclude(msg)?;
    let eff = std::path::Path::new(cwd)
        .join(path)
        .to_string_lossy()
        .into_owned();
    Some(msg_fs_sync_full(
        nonce, flags, latency_ms, inline_max, &eff, exclude, None,
    ))
}

/// Rebase a `STAGING` `C2S_FS_SYNC` onto the connection's resolved drag
/// staging dir: `staging` replaces the ignored `path` field and `STAGING`
/// is cleared, producing a plain path-based sync the handler consumes
/// unchanged. `None` when the flag is unset, when `FROM_PTY` rides along
/// (an invalid combination the caller must refuse, not rebase), or when the
/// message is malformed — like `fs_sync_rebase`, a caller that cannot
/// resolve must refuse rather than forward: the empty `path` would
/// otherwise be read as a root. Any exclude field rides along.
pub fn fs_sync_rebase_staging(msg: &[u8], staging: &str) -> Option<Vec<u8>> {
    let flags = fs_sync_flags(msg)?;
    if flags & FS_SYNC_STAGING == 0 || flags & FS_SYNC_FROM_PTY != 0 {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let latency_ms = u16::from_le_bytes([msg[5], msg[6]]);
    let inline_max = u32::from_le_bytes([msg[7], msg[8], msg[9], msg[10]]);
    let exclude = fs_sync_exclude(msg)?;
    Some(msg_fs_sync_full(
        nonce,
        flags & !FS_SYNC_STAGING,
        latency_ms,
        inline_max,
        staging,
        exclude,
        None,
    ))
}

pub fn msg_fs_stop(sync_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_FS_STOP);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg
}

pub fn msg_fs_ack(sync_id: u16, update_id: u32) -> Vec<u8> {
    let mut msg = Vec::with_capacity(7);
    msg.push(C2S_FS_ACK);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg
}

pub fn msg_fs_fetch(nonce: u16, sync_id: u16, path: &str) -> Vec<u8> {
    let pb = path.as_bytes();
    let mut msg = Vec::with_capacity(7 + pb.len());
    msg.push(C2S_FS_FETCH);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&sync_id.to_le_bytes());
    push_str(&mut msg, path);
    msg
}

/// Build a `C2S_FS_SEARCH`.
pub fn msg_fs_search(nonce: u16, limit: u16, root: &str, query: &str) -> Vec<u8> {
    let rb = root.as_bytes();
    let qb = query.as_bytes();
    let mut m = Vec::with_capacity(9 + rb.len() + qb.len());
    m.push(C2S_FS_SEARCH);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.extend_from_slice(&limit.to_le_bytes());
    push_str(&mut m, root);
    push_str(&mut m, query);
    m
}

/// Parse a `C2S_FS_SEARCH` → `(nonce, limit, root, query)`.
pub fn parse_fs_search(data: &[u8]) -> Option<(u16, u16, String, String)> {
    if data.first().copied() != Some(C2S_FS_SEARCH) || data.len() < 9 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let limit = u16::from_le_bytes([data[3], data[4]]);
    let rl = u16::from_le_bytes([data[5], data[6]]) as usize;
    let ro = 7;
    if data.len() < ro + rl + 2 {
        return None;
    }
    let root = String::from_utf8_lossy(&data[ro..ro + rl]).into_owned();
    let qo = ro + rl;
    let ql = u16::from_le_bytes([data[qo], data[qo + 1]]) as usize;
    let qs = qo + 2;
    if data.len() < qs + ql {
        return None;
    }
    let query = String::from_utf8_lossy(&data[qs..qs + ql]).into_owned();
    Some((nonce, limit, root, query))
}

/// Build an `S2C_FS_SEARCH` result.
pub fn msg_fs_search_result(nonce: u16, status: u8, paths: &[String]) -> Vec<u8> {
    let mut m = Vec::with_capacity(6 + paths.iter().map(|p| 2 + p.len()).sum::<usize>());
    m.push(S2C_FS_SEARCH);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(status);
    // Declare only as many results as the u16 count can describe, and emit
    // exactly that many: a wrapped count leaves the reader consuming the
    // wrong number of entries and desyncing on whatever follows.
    let count = paths.len().min(u16::MAX as usize);
    m.extend_from_slice(&(count as u16).to_le_bytes());
    for p in &paths[..count] {
        push_str(&mut m, p);
    }
    m
}

/// Parse an `S2C_FS_SEARCH` → `(nonce, status, paths)`.
pub fn parse_fs_search_result(data: &[u8]) -> Option<(u16, u8, Vec<String>)> {
    if data.first().copied() != Some(S2C_FS_SEARCH) || data.len() < 6 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let status = data[3];
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    let mut paths = Vec::with_capacity(count);
    let mut off = 6;
    for _ in 0..count {
        if off + 2 > data.len() {
            return None;
        }
        let pl = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        if off + pl > data.len() {
            return None;
        }
        paths.push(String::from_utf8_lossy(&data[off..off + pl]).into_owned());
        off += pl;
    }
    Some((nonce, status, paths))
}

/// Build a `C2S_FS_INDEX`.
pub fn msg_fs_index(nonce: u16, root: &str) -> Vec<u8> {
    let rb = root.as_bytes();
    let mut m = Vec::with_capacity(6 + rb.len());
    m.push(C2S_FS_INDEX);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(0); // flags, reserved
    push_str(&mut m, root);
    m
}

/// Parse a `C2S_FS_INDEX` → `(nonce, flags, root)`.
pub fn parse_fs_index(data: &[u8]) -> Option<(u16, u8, String)> {
    // [0x47][nonce:2][flags:1][root_len:2][root:N]
    if data.first().copied() != Some(C2S_FS_INDEX) || data.len() < 6 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let flags = data[3];
    let rl = u16::from_le_bytes([data[4], data[5]]) as usize;
    if data.len() < 6 + rl {
        return None;
    }
    let root = String::from_utf8_lossy(&data[6..6 + rl]).into_owned();
    Some((nonce, flags, root))
}

/// Build an `S2C_FS_INDEX` result. `paths` should be root-relative and
/// sorted — sorted lists share prefixes, which is what makes the LZ4
/// payload small.
pub fn msg_fs_index_result(nonce: u16, status: u8, flags: u8, paths: &[String]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(paths.iter().map(|p| 2 + p.len()).sum::<usize>());
    for p in paths {
        push_str(&mut raw, p);
    }
    let compressed = lz4_flex::compress_prepend_size(&raw);
    let mut m = Vec::with_capacity(9 + compressed.len());
    m.push(S2C_FS_INDEX);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(status);
    m.push(flags);
    m.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    m.extend_from_slice(&compressed);
    m
}

/// Parse an `S2C_FS_INDEX` → `(nonce, status, flags, paths)`. Applies the
/// standard decompression guard; `None` = malformed, over-sized, or a
/// payload that disagrees with `count`.
pub fn parse_fs_index_result(data: &[u8]) -> Option<(u16, u8, u8, Vec<String>)> {
    // [0x46][nonce:2][status:1][flags:1][count:4][paths:LZ4]
    if data.first().copied() != Some(S2C_FS_INDEX) || data.len() < 9 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let status = data[3];
    let flags = data[4];
    let count = u32::from_le_bytes([data[5], data[6], data[7], data[8]]) as usize;
    if count > FS_INDEX_MAX_COUNT {
        return None;
    }
    let raw = decompress_guarded(&data[9..])?;
    // Each record is at least 2 bytes, so `count` bounds the preallocation.
    if count > raw.len() / 2 + 1 {
        return None;
    }
    let mut paths = Vec::with_capacity(count);
    let mut off = 0;
    while off < raw.len() {
        if off + 2 > raw.len() {
            return None;
        }
        let pl = u16::from_le_bytes([raw[off], raw[off + 1]]) as usize;
        off += 2;
        if off + pl > raw.len() {
            return None;
        }
        paths.push(String::from_utf8_lossy(&raw[off..off + pl]).into_owned());
        off += pl;
    }
    if paths.len() != count {
        return None;
    }
    Some((nonce, status, flags, paths))
}

/// Build a `C2S_FS_READ`. `max_bytes` of zero asks for the server default.
///
/// Paths come in groups, and a group is one question. Without `FS_READ_FIRST`
/// the groups are read straight through and the distinction does not matter; with
/// it each group is answered by its own first readable path, which is what lets
/// one message resolve a screenful of icons instead of one message per icon.
pub fn msg_fs_read(nonce: u16, flags: u8, max_bytes: u32, groups: &[&[&str]]) -> Option<Vec<u8>> {
    let total: usize = groups.iter().map(|group| group.len()).sum();
    if groups.is_empty() || total == 0 || total > FS_READ_MAX_PATHS {
        return None;
    }
    let group_count = u16::try_from(groups.len()).ok()?;
    let mut m = Vec::with_capacity(10 + total * 32);
    m.push(C2S_FS_READ);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(flags);
    m.extend_from_slice(&max_bytes.to_le_bytes());
    m.extend_from_slice(&group_count.to_le_bytes());
    for group in groups {
        let count = u16::try_from(group.len()).ok()?;
        m.extend_from_slice(&count.to_le_bytes());
        for path in *group {
            if u16::try_from(path.len()).is_err() {
                return None;
            }
            push_str(&mut m, path);
        }
    }
    Some(m)
}

/// Build a one-group `C2S_FS_READ`, the plain "read these files" case.
pub fn msg_fs_read_paths(nonce: u16, flags: u8, max_bytes: u32, paths: &[&str]) -> Option<Vec<u8>> {
    msg_fs_read(nonce, flags, max_bytes, &[paths])
}

/// A parsed `C2S_FS_READ`: `(nonce, flags, max_bytes, groups)`, where a group is
/// one question and a path that is not UTF-8 is `None`.
pub type FsReadRequest = (u16, u8, u32, Vec<Vec<Option<String>>>);

/// Parse a `C2S_FS_READ` → `(nonce, flags, max_bytes, groups)`.
///
/// A path that is not UTF-8 is `None` rather than a parse failure. This family
/// names paths as text, so it cannot answer about one it cannot name — but
/// `None` for the whole frame would drop a well-formed request with no reply,
/// leaving the caller waiting on a nonce nothing will ever carry. The caller
/// answers such a path per-record, the way it answers one it cannot read.
pub fn parse_fs_read(data: &[u8]) -> Option<FsReadRequest> {
    // [0x4D][nonce:2][flags:1][max_bytes:4][group_count:2]
    // then group_count × ( [path_count:2] then path_count × [len:2][path:N] )
    if data.first().copied() != Some(C2S_FS_READ) || data.len() < 10 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let flags = data[3];
    let max_bytes = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let group_count = u16::from_le_bytes([data[8], data[9]]) as usize;
    if group_count == 0 || group_count > FS_READ_MAX_PATHS {
        return None;
    }
    let mut groups = Vec::with_capacity(group_count);
    let mut total = 0usize;
    let mut off = 10;
    for _ in 0..group_count {
        if off + 2 > data.len() {
            return None;
        }
        let count = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
        off += 2;
        total = total.checked_add(count)?;
        if total > FS_READ_MAX_PATHS {
            return None;
        }
        let mut paths = Vec::with_capacity(count);
        for _ in 0..count {
            if off + 2 > data.len() {
                return None;
            }
            let len = u16::from_le_bytes([data[off], data[off + 1]]) as usize;
            off += 2;
            if off + len > data.len() {
                return None;
            }
            paths.push(String::from_utf8(data[off..off + len].to_vec()).ok());
            off += len;
        }
        groups.push(paths);
    }
    if off != data.len() || total == 0 {
        return None;
    }
    Some((nonce, flags, max_bytes, groups))
}

/// Build an `S2C_FS_READ` from `(status, path, content)` records, in request
/// order. A record whose status is not `FS_FILE_OK` carries no content.
pub fn msg_fs_read_result(nonce: u16, status: u8, records: &[(u8, &str, &[u8])]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(
        records
            .iter()
            .map(|(_, path, data)| 7 + path.len() + data.len())
            .sum::<usize>(),
    );
    for (record_status, path, content) in records {
        raw.push(*record_status);
        push_str(&mut raw, path);
        let content: &[u8] = if *record_status == FS_FILE_OK {
            content
        } else {
            &[]
        };
        raw.extend_from_slice(&(content.len() as u32).to_le_bytes());
        raw.extend_from_slice(content);
    }
    let compressed = lz4_flex::compress_prepend_size(&raw);
    let mut m = Vec::with_capacity(6 + compressed.len());
    m.push(S2C_FS_READ);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(status);
    m.extend_from_slice(&(records.len() as u16).to_le_bytes());
    m.extend_from_slice(&compressed);
    m
}

/// One answered path: its `FS_FILE_*` status, the path as it was asked for, and
/// its content — empty unless the status is `FS_FILE_OK`.
pub type FsReadRecord = (u8, String, Vec<u8>);

/// Parse an `S2C_FS_READ` → `(nonce, status, records)`. Applies the standard
/// decompression guard; `None` = malformed or a payload disagreeing with `count`.
pub fn parse_fs_read_result(data: &[u8]) -> Option<(u16, u8, Vec<FsReadRecord>)> {
    // [0x48][nonce:2][status:1][count:2][records:LZ4]
    if data.first().copied() != Some(S2C_FS_READ) || data.len() < 6 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let status = data[3];
    let count = u16::from_le_bytes([data[4], data[5]]) as usize;
    if count > FS_READ_MAX_PATHS {
        return None;
    }
    let raw = decompress_guarded(&data[6..])?;
    // Each record is at least seven bytes, which bounds the preallocation.
    if count > raw.len() / 7 + 1 {
        return None;
    }
    let mut records = Vec::with_capacity(count);
    let mut off = 0;
    while off < raw.len() {
        if off + 3 > raw.len() {
            return None;
        }
        let record_status = raw[off];
        let path_len = u16::from_le_bytes([raw[off + 1], raw[off + 2]]) as usize;
        off += 3;
        if off + path_len + 4 > raw.len() {
            return None;
        }
        let path = String::from_utf8_lossy(&raw[off..off + path_len]).into_owned();
        off += path_len;
        let size =
            u32::from_le_bytes([raw[off], raw[off + 1], raw[off + 2], raw[off + 3]]) as usize;
        off += 4;
        if off + size > raw.len() {
            return None;
        }
        records.push((record_status, path, raw[off..off + size].to_vec()));
        off += size;
    }
    if records.len() != count {
        return None;
    }
    Some((nonce, status, records))
}

pub fn msg_fs_synced(nonce: u16, sync_id: u16, status: u8, detail: &str) -> Vec<u8> {
    let db = detail.as_bytes();
    let mut msg = Vec::with_capacity(8 + db.len());
    msg.push(S2C_FS_SYNCED);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.push(status);
    push_str(&mut msg, detail);
    msg
}

/// Build an `FS_UPDATE` from an uncompressed records buffer.
pub fn msg_fs_update(sync_id: u16, update_id: u32, flags: u8, records: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut msg = Vec::with_capacity(8 + compressed.len());
    msg.push(S2C_FS_UPDATE);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.extend_from_slice(&update_id.to_le_bytes());
    msg.push(flags);
    msg.extend_from_slice(&compressed);
    msg
}

pub fn msg_fs_file(nonce: u16, status: u8, data: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(data);
    let mut msg = Vec::with_capacity(4 + compressed.len());
    msg.push(S2C_FS_FILE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&compressed);
    msg
}

pub fn msg_fs_closed(sync_id: u16, reason: u8) -> Vec<u8> {
    let mut msg = Vec::with_capacity(4);
    msg.push(S2C_FS_CLOSED);
    msg.extend_from_slice(&sync_id.to_le_bytes());
    msg.push(reason);
    msg
}

// ---------------------------------------------------------------------------
// Client-side reducer
// ---------------------------------------------------------------------------

/// One node in a mirrored tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsNode {
    pub entry_flags: u8,
    pub size: u64,
    pub mtime_ns: u64,
    pub mode: u32,
    pub hash: u128,
    /// Present when the sync requested content and the file fits the
    /// inline limit. `None` does not mean empty — check `entry_flags`.
    pub content: Option<Vec<u8>>,
}

/// Cap on any single LZ4-decompressed fs payload — the protocol-wide
/// [`crate::MAX_DECOMPRESSED`] guard (docs/protocol.md). Checked against
/// the prepended size *before* allocating, so a hostile or corrupt length
/// cannot force a giant allocation (the terminal path has the same guard).
/// Large trees arrive as many bounded updates, never one huge one; content
/// records are bounded by the sync's `inline_max` (16 MiB default).
pub const FS_MAX_DECOMPRESSED: usize = crate::MAX_DECOMPRESSED;

/// Decompress a `compress_prepend_size` payload, refusing declared sizes
/// over [`FS_MAX_DECOMPRESSED`].
fn decompress_guarded(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }
    let declared = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if declared > FS_MAX_DECOMPRESSED {
        return None;
    }
    lz4_flex::decompress_size_prepended(data).ok()
}

/// Decompress an `FS_UPDATE`'s records buffer (for consumers that want the
/// records themselves, e.g. event display), with the standard guard.
pub fn fs_update_records(msg: &[u8]) -> Option<Vec<u8>> {
    if msg.len() < 8 || msg[0] != S2C_FS_UPDATE {
        return None;
    }
    decompress_guarded(&msg[8..])
}

/// Parse an `S2C_FS_FILE` message (starting at the opcode byte) into
/// `(nonce, status, data)`. Applies the same decompression guard as
/// [`FsMirror::apply_update`]; `None` = malformed or over-sized.
pub fn parse_fs_file(msg: &[u8]) -> Option<(u16, u8, Vec<u8>)> {
    if msg.len() < 4 || msg[0] != S2C_FS_FILE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let data = decompress_guarded(&msg[4..])?;
    Some((nonce, status, data))
}

// ---------------------------------------------------------------------------
// Write family (docs/design/fs-write.md): nonce request/response side-band
// operations against disk. The write itself echoes nothing — the existing
// per-client differ re-emits UPSERT/MOVE/DELETE once the reconciler
// re-indexes the landed change.
// ---------------------------------------------------------------------------

/// A content write (`C2S_FS_WRITE`). `base` is the CAS precondition: the
/// current on-disk content hash to match (non-zero), zero for
/// create-exclusive, ignored under `FS_WRITE_NO_CAS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsWrite {
    pub nonce: u16,
    pub sync_id: u16,
    pub flags: u8,
    pub base: u128,
    pub mode: u32,
    pub content_kind: u8,
    pub path: String,
    pub content: Vec<u8>,
}

pub fn msg_fs_write(w: &FsWrite) -> Vec<u8> {
    let pb = w.path.as_bytes();
    let compressed = lz4_flex::compress_prepend_size(&w.content);
    let mut msg = Vec::with_capacity(29 + pb.len() + compressed.len());
    msg.push(C2S_FS_WRITE);
    msg.extend_from_slice(&w.nonce.to_le_bytes());
    msg.extend_from_slice(&w.sync_id.to_le_bytes());
    msg.push(w.flags);
    msg.extend_from_slice(&w.base.to_le_bytes());
    msg.extend_from_slice(&w.mode.to_le_bytes());
    msg.push(w.content_kind);
    push_str(&mut msg, &w.path);
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse a `C2S_FS_WRITE`. `None` = malformed, non-UTF-8 path, or content
/// whose declared decompressed size exceeds the protocol cap.
pub fn parse_fs_write(msg: &[u8]) -> Option<FsWrite> {
    // [nonce:2][sync_id:2][flags:1][base:16][mode:4][content_kind:1][path_len:2][path:N][content:LZ4]
    if msg.len() < 29 || msg[0] != C2S_FS_WRITE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
    let flags = msg[5];
    let base = u128::from_le_bytes(msg[6..22].try_into().unwrap());
    let mode = u32::from_le_bytes(msg[22..26].try_into().unwrap());
    let content_kind = msg[26];
    let path_len = u16::from_le_bytes([msg[27], msg[28]]) as usize;
    let path = std::str::from_utf8(msg.get(29..29 + path_len)?)
        .ok()?
        .to_string();
    let content = decompress_guarded(&msg[29 + path_len..])?;
    Some(FsWrite {
        nonce,
        sync_id,
        flags,
        base,
        mode,
        content_kind,
        path,
        content,
    })
}

/// A metadata op (`C2S_FS_OP`): `op` selects mkdir/remove/rename; `a` is
/// the primary path, `b` the rename destination. `base`/`mode` are used
/// by only some ops (like `LSP_QUERY`'s `line`/`col`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsOp {
    pub nonce: u16,
    pub sync_id: u16,
    pub op: u8,
    pub flags: u8,
    pub base: u128,
    pub mode: u32,
    pub a: String,
    pub b: String,
}

pub fn msg_fs_op(o: &FsOp) -> Vec<u8> {
    let ab = o.a.as_bytes();
    let bb = o.b.as_bytes();
    let mut msg = Vec::with_capacity(29 + ab.len() + bb.len());
    msg.push(C2S_FS_OP);
    msg.extend_from_slice(&o.nonce.to_le_bytes());
    msg.extend_from_slice(&o.sync_id.to_le_bytes());
    msg.push(o.op);
    msg.push(o.flags);
    msg.extend_from_slice(&o.base.to_le_bytes());
    msg.extend_from_slice(&o.mode.to_le_bytes());
    push_str(&mut msg, &o.a);
    push_str(&mut msg, &o.b);
    msg
}

/// Parse a `C2S_FS_OP`. `None` = malformed or a non-UTF-8 path.
pub fn parse_fs_op(msg: &[u8]) -> Option<FsOp> {
    // [nonce:2][sync_id:2][op:1][flags:1][base:16][mode:4][a_len:2][a:N][b_len:2][b:N]
    if msg.len() < 29 || msg[0] != C2S_FS_OP {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
    let op = msg[5];
    let flags = msg[6];
    let base = u128::from_le_bytes(msg[7..23].try_into().unwrap());
    let mode = u32::from_le_bytes(msg[23..27].try_into().unwrap());
    let a_len = u16::from_le_bytes([msg[27], msg[28]]) as usize;
    let a = std::str::from_utf8(msg.get(29..29 + a_len)?)
        .ok()?
        .to_string();
    let b_off = 29 + a_len;
    let b_len = u16::from_le_bytes([*msg.get(b_off)?, *msg.get(b_off + 1)?]) as usize;
    let b = std::str::from_utf8(msg.get(b_off + 2..b_off + 2 + b_len)?)
        .ok()?
        .to_string();
    Some(FsOp {
        nonce,
        sync_id,
        op,
        flags,
        base,
        mode,
        a,
        b,
    })
}

/// Build an `S2C_FS_DONE`. On success `hash`/`mtime_ns` are the post-op
/// stat; on `CONFLICT`, `hash` is the current on-disk hash.
pub fn msg_fs_done(nonce: u16, status: u8, hash: u128, mtime_ns: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(28);
    msg.push(S2C_FS_DONE);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&hash.to_le_bytes());
    msg.extend_from_slice(&mtime_ns.to_le_bytes());
    msg
}

/// Parse an `S2C_FS_DONE` into `(nonce, status, hash, mtime_ns)`.
pub fn parse_fs_done(msg: &[u8]) -> Option<(u16, u8, u128, u64)> {
    // [nonce:2][status:1][hash:16][mtime_ns:8]
    if msg.len() < 28 || msg[0] != S2C_FS_DONE {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let status = msg[3];
    let hash = u128::from_le_bytes(msg[4..20].try_into().unwrap());
    let mtime_ns = u64::from_le_bytes(msg[20..28].try_into().unwrap());
    Some((nonce, status, hash, mtime_ns))
}

// ---------------------------------------------------------------------------
// Chunked upload family (docs/protocol.md "Filesystem sync"): begin →
// sequential chunks → finish, for files too large to fit one FS_WRITE
// frame. Upload ids are per-connection, allocated by the server.
// ---------------------------------------------------------------------------

/// An upload begin (`C2S_FS_UPLOAD_BEGIN`). `path` is the escaped wire path
/// relative to the sync root; `size` the total plaintext bytes. `base` is
/// the CAS precondition, exactly as `FsWrite::base`: the current on-disk
/// content hash to match (non-zero), zero for create-exclusive, ignored
/// under `FS_UPLOAD_NO_CAS`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsUploadBegin {
    pub nonce: u16,
    pub sync_id: u16,
    pub flags: u8,
    pub base: u128,
    pub mode: u32,
    pub size: u64,
    pub path: String,
}

pub fn msg_fs_upload_begin(b: &FsUploadBegin) -> Vec<u8> {
    let pb = b.path.as_bytes();
    let mut msg = Vec::with_capacity(36 + pb.len());
    msg.push(C2S_FS_UPLOAD_BEGIN);
    msg.extend_from_slice(&b.nonce.to_le_bytes());
    msg.extend_from_slice(&b.sync_id.to_le_bytes());
    msg.push(b.flags);
    msg.extend_from_slice(&b.base.to_le_bytes());
    msg.extend_from_slice(&b.mode.to_le_bytes());
    msg.extend_from_slice(&b.size.to_le_bytes());
    push_str(&mut msg, &b.path);
    msg
}

/// Parse a `C2S_FS_UPLOAD_BEGIN`. `None` = malformed or a non-UTF-8 path.
pub fn parse_fs_upload_begin(msg: &[u8]) -> Option<FsUploadBegin> {
    // [nonce:2][sync_id:2][flags:1][base:16][mode:4][size:8][path_len:2][path:N]
    if msg.len() < 36 || msg[0] != C2S_FS_UPLOAD_BEGIN {
        return None;
    }
    let nonce = u16::from_le_bytes([msg[1], msg[2]]);
    let sync_id = u16::from_le_bytes([msg[3], msg[4]]);
    let flags = msg[5];
    let base = u128::from_le_bytes(msg[6..22].try_into().unwrap());
    let mode = u32::from_le_bytes(msg[22..26].try_into().unwrap());
    let size = u64::from_le_bytes(msg[26..34].try_into().unwrap());
    let path_len = u16::from_le_bytes([msg[34], msg[35]]) as usize;
    let path = std::str::from_utf8(msg.get(36..36 + path_len)?)
        .ok()?
        .to_string();
    Some(FsUploadBegin {
        nonce,
        sync_id,
        flags,
        base,
        mode,
        size,
        path,
    })
}

/// Build a `C2S_FS_UPLOAD_CHUNK`; `data` is the plaintext chunk.
pub fn msg_fs_upload_chunk(upload_id: u16, offset: u64, data: &[u8]) -> Vec<u8> {
    let compressed = lz4_flex::compress_prepend_size(data);
    let mut msg = Vec::with_capacity(11 + compressed.len());
    msg.push(C2S_FS_UPLOAD_CHUNK);
    msg.extend_from_slice(&upload_id.to_le_bytes());
    msg.extend_from_slice(&offset.to_le_bytes());
    msg.extend_from_slice(&compressed);
    msg
}

/// Parse a `C2S_FS_UPLOAD_CHUNK` → `(upload_id, offset, data)`. Applies the
/// standard decompression guard; `None` = malformed or over-sized.
pub fn parse_fs_upload_chunk(msg: &[u8]) -> Option<(u16, u64, Vec<u8>)> {
    // [upload_id:2][offset:8][data:LZ4]
    if msg.len() < 11 || msg[0] != C2S_FS_UPLOAD_CHUNK {
        return None;
    }
    let upload_id = u16::from_le_bytes([msg[1], msg[2]]);
    let offset = u64::from_le_bytes(msg[3..11].try_into().unwrap());
    let data = decompress_guarded(&msg[11..])?;
    Some((upload_id, offset, data))
}

pub fn msg_fs_upload_finish(nonce: u16, upload_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(5);
    msg.push(C2S_FS_UPLOAD_FINISH);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.extend_from_slice(&upload_id.to_le_bytes());
    msg
}

/// Parse a `C2S_FS_UPLOAD_FINISH` → `(nonce, upload_id)`.
pub fn parse_fs_upload_finish(msg: &[u8]) -> Option<(u16, u16)> {
    if msg.len() < 5 || msg[0] != C2S_FS_UPLOAD_FINISH {
        return None;
    }
    Some((
        u16::from_le_bytes([msg[1], msg[2]]),
        u16::from_le_bytes([msg[3], msg[4]]),
    ))
}

pub fn msg_fs_upload_cancel(upload_id: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(3);
    msg.push(C2S_FS_UPLOAD_CANCEL);
    msg.extend_from_slice(&upload_id.to_le_bytes());
    msg
}

/// Build an `S2C_FS_UPLOAD_BEGIN` result. On `CONFLICT`, `hash` is the
/// current on-disk content hash (the `FS_DONE` convention); both stat
/// fields are zero otherwise.
pub fn msg_fs_upload_begin_result(
    nonce: u16,
    status: u8,
    upload_id: u16,
    hash: u128,
    mtime_ns: u64,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(30);
    msg.push(S2C_FS_UPLOAD_BEGIN);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&upload_id.to_le_bytes());
    msg.extend_from_slice(&hash.to_le_bytes());
    msg.extend_from_slice(&mtime_ns.to_le_bytes());
    msg
}

/// Parse an `S2C_FS_UPLOAD_BEGIN` → `(nonce, status, upload_id, hash, mtime_ns)`.
pub fn parse_fs_upload_begin_result(msg: &[u8]) -> Option<(u16, u8, u16, u128, u64)> {
    if msg.len() < 30 || msg[0] != S2C_FS_UPLOAD_BEGIN {
        return None;
    }
    Some((
        u16::from_le_bytes([msg[1], msg[2]]),
        msg[3],
        u16::from_le_bytes([msg[4], msg[5]]),
        u128::from_le_bytes(msg[6..22].try_into().unwrap()),
        u64::from_le_bytes(msg[22..30].try_into().unwrap()),
    ))
}

/// Build an `S2C_FS_UPLOAD_CHUNK` acknowledgement.
pub fn msg_fs_upload_chunk_result(upload_id: u16, status: u8, received: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(12);
    msg.push(S2C_FS_UPLOAD_CHUNK);
    msg.extend_from_slice(&upload_id.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&received.to_le_bytes());
    msg
}

/// Parse an `S2C_FS_UPLOAD_CHUNK` → `(upload_id, status, received)`.
pub fn parse_fs_upload_chunk_result(msg: &[u8]) -> Option<(u16, u8, u64)> {
    if msg.len() < 12 || msg[0] != S2C_FS_UPLOAD_CHUNK {
        return None;
    }
    Some((
        u16::from_le_bytes([msg[1], msg[2]]),
        msg[3],
        u64::from_le_bytes(msg[4..12].try_into().unwrap()),
    ))
}

/// Build an `S2C_FS_UPLOAD_FINISH` result. On success `hash`/`mtime_ns` are
/// the post-rename stat, exactly as `FS_DONE` carries for a write; on
/// `CONFLICT` (the FINISH-time precondition re-verification failed),
/// `hash` is the current on-disk hash.
pub fn msg_fs_upload_finish_result(nonce: u16, status: u8, hash: u128, mtime_ns: u64) -> Vec<u8> {
    let mut msg = Vec::with_capacity(28);
    msg.push(S2C_FS_UPLOAD_FINISH);
    msg.extend_from_slice(&nonce.to_le_bytes());
    msg.push(status);
    msg.extend_from_slice(&hash.to_le_bytes());
    msg.extend_from_slice(&mtime_ns.to_le_bytes());
    msg
}

/// Parse an `S2C_FS_UPLOAD_FINISH` → `(nonce, status, hash, mtime_ns)`.
pub fn parse_fs_upload_finish_result(msg: &[u8]) -> Option<(u16, u8, u128, u64)> {
    if msg.len() < 28 || msg[0] != S2C_FS_UPLOAD_FINISH {
        return None;
    }
    Some((
        u16::from_le_bytes([msg[1], msg[2]]),
        msg[3],
        u128::from_le_bytes(msg[4..20].try_into().unwrap()),
        u64::from_le_bytes(msg[20..28].try_into().unwrap()),
    ))
}

/// The complete client obligation: apply updates, read `live`.
///
/// Paths are relative to the sync root, `/`-separated, "" = the root itself.
#[derive(Debug, Default)]
pub struct FsMirror {
    pub live: BTreeMap<String, FsNode>,
    staging: Option<BTreeMap<String, FsNode>>,
}

impl FsMirror {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one `FS_UPDATE` message (starting at the opcode byte).
    /// Returns `Some(update_id)` to acknowledge, `None` if malformed.
    pub fn apply_update(&mut self, msg: &[u8]) -> Option<u32> {
        if msg.len() < 8 || msg[0] != S2C_FS_UPDATE {
            return None;
        }
        let update_id = u32::from_le_bytes([msg[3], msg[4], msg[5], msg[6]]);
        let flags = msg[7];
        let records = decompress_guarded(&msg[8..])?;
        if flags & FS_UPDATE_RESET != 0 {
            self.staging = Some(BTreeMap::new());
        }
        let map = self.staging.as_mut().unwrap_or(&mut self.live);
        for record in fs_records(&records) {
            match record {
                FsRecord::Upsert {
                    path,
                    entry_flags,
                    size,
                    mtime_ns,
                    mode,
                    hash,
                    content,
                } => {
                    let content = match content {
                        FsContent::None => {
                            let entry_type = entry_flags & FS_ENTRY_TYPE_MASK;
                            let content_bearing =
                                entry_type == FS_ENTRY_FILE || entry_type == FS_ENTRY_SYMLINK;
                            if !content_bearing
                                || entry_flags
                                    & (FS_ENTRY_NO_CONTENT
                                        | FS_ENTRY_UNREADABLE
                                        | FS_ENTRY_UNSTABLE)
                                    != 0
                            {
                                None
                            } else {
                                // Metadata-only upsert keeps previous content only
                                // when the entry stays the same content-bearing
                                // type. The node is replaced either way, so move
                                // the bytes out instead of cloning them.
                                map.remove(path)
                                    .filter(|n| n.entry_flags & FS_ENTRY_TYPE_MASK == entry_type)
                                    .and_then(|n| n.content)
                            }
                        }
                        FsContent::Full(data) => Some(data.to_vec()),
                        FsContent::Delta(ops) => {
                            let base = map
                                .get(path)
                                .and_then(|n| n.content.as_deref())
                                .unwrap_or(&[]);
                            Some(apply_fs_delta(base, ops)?)
                        }
                    };
                    map.insert(
                        path.to_string(),
                        FsNode {
                            entry_flags,
                            size,
                            mtime_ns,
                            mode,
                            hash,
                            content,
                        },
                    );
                }
                FsRecord::Delete { path } => {
                    remove_subtree(map, path);
                }
                FsRecord::Move { from, to } => {
                    let moved = take_subtree(map, from);
                    for (suffix, node) in moved {
                        let new_path = join_moved(to, &suffix);
                        map.insert(new_path, node);
                    }
                }
            }
        }
        if flags & FS_UPDATE_SYNC != 0
            && let Some(staged) = self.staging.take()
        {
            self.live = staged;
        }
        Some(update_id)
    }
}

/// Keys at or under `root` in a sorted map: the entry itself plus the
/// contiguous `root/`-prefixed range — O(log n + subtree), never a scan of
/// the whole map.
fn subtree_keys(map: &BTreeMap<String, FsNode>, root: &str) -> Vec<String> {
    if root.is_empty() {
        return map.keys().cloned().collect();
    }
    let mut keys: Vec<String> = Vec::new();
    if map.contains_key(root) {
        keys.push(root.to_string());
    }
    let prefix = format!("{root}/");
    keys.extend(
        map.range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .map(|(k, _)| k.clone()),
    );
    keys
}

fn remove_subtree(map: &mut BTreeMap<String, FsNode>, root: &str) {
    for key in subtree_keys(map, root) {
        map.remove(&key);
    }
}

/// Remove and return `(suffix, node)` pairs for `root` and everything under
/// it. The suffix is "" for the root itself.
fn take_subtree(map: &mut BTreeMap<String, FsNode>, root: &str) -> Vec<(String, FsNode)> {
    subtree_keys(map, root)
        .into_iter()
        .map(|key| {
            let node = map.remove(&key).unwrap();
            let suffix = if key.len() > root.len() {
                key[root.len() + if root.is_empty() { 0 } else { 1 }..].to_string()
            } else {
                String::new()
            };
            (suffix, node)
        })
        .collect()
}

fn join_moved(to: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        to.to_string()
    } else if to.is_empty() {
        suffix.to_string()
    } else {
        format!("{to}/{suffix}")
    }
}

/// Apply a content delta (LEB128 COPY/INSERT instruction stream) to a base.
pub fn apply_fs_delta(base: &[u8], mut ops: &[u8]) -> Option<Vec<u8>> {
    fn leb128(data: &mut &[u8]) -> Option<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let (&byte, rest) = data.split_first()?;
            *data = rest;
            if shift >= 64 {
                return None;
            }
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
            shift += 7;
        }
    }
    let mut out = Vec::new();
    while let Some((&op, rest)) = ops.split_first() {
        ops = rest;
        match op {
            0x01 => {
                let offset = leb128(&mut ops)? as usize;
                let len = leb128(&mut ops)? as usize;
                if out.len().checked_add(len)? > FS_MAX_DECOMPRESSED {
                    return None;
                }
                out.extend_from_slice(base.get(offset..offset.checked_add(len)?)?);
            }
            0x02 => {
                let len = leb128(&mut ops)? as usize;
                if ops.len() < len {
                    return None;
                }
                if out.len().checked_add(len)? > FS_MAX_DECOMPRESSED {
                    return None;
                }
                out.extend_from_slice(&ops[..len]);
                ops = &ops[len..];
            }
            _ => return None,
        }
    }
    Some(out)
}

// ── FS_GREP (docs/design/fs-grep.md) ───────────────────────────────────────

/// One record of an `FS_GREP` response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsGrepRecord {
    /// FILE 0x01: `[kind:1][flags:1][n:2][path_len:2][path:N]` — the next `n`
    /// `Match` records belong to this file. `flags` is `FS_GREP_FILE_IGNORED`.
    File {
        flags: u8,
        n: u16,
        /// Root-relative, lossy UTF-8 of the on-disk name.
        path: String,
    },
    /// MATCH 0x02: `[kind:1][line:4][col:4][end_line:4][end_col:4][text_len:4][text:N]`.
    /// 0-based lines, UTF-8 byte columns — the same shape as an LSP range.
    /// `end_line` differs from `line` when the pattern matched across a
    /// newline; `text` then carries every line the match spans, joined by
    /// `\n`, so a client can show the whole thing rather than a fragment.
    Match {
        line: u32,
        col: u32,
        end_line: u32,
        end_col: u32,
        /// The matched line(s) without a trailing terminator, capped.
        text: String,
    },
}

/// Build a `C2S_FS_GREP`.
pub fn msg_fs_grep(
    nonce: u16,
    flags: u8,
    max_matches: u16,
    max_per_file: u16,
    root: &str,
    query: &str,
) -> Vec<u8> {
    let rb = root.as_bytes();
    let qb = query.as_bytes();
    let mut m = Vec::with_capacity(12 + rb.len() + qb.len());
    m.push(C2S_FS_GREP);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(flags);
    m.extend_from_slice(&max_matches.to_le_bytes());
    m.extend_from_slice(&max_per_file.to_le_bytes());
    push_str(&mut m, root);
    push_str(&mut m, query);
    m
}

/// Parse a `C2S_FS_GREP` → `(nonce, flags, max_matches, max_per_file, root, query)`.
pub fn parse_fs_grep(data: &[u8]) -> Option<(u16, u8, u16, u16, String, String)> {
    if data.first().copied() != Some(C2S_FS_GREP) || data.len() < 12 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let flags = data[3];
    let max_matches = u16::from_le_bytes([data[4], data[5]]);
    let max_per_file = u16::from_le_bytes([data[6], data[7]]);
    let rl = u16::from_le_bytes([data[8], data[9]]) as usize;
    let ro = 10;
    if data.len() < ro + rl + 2 {
        return None;
    }
    let root = String::from_utf8_lossy(&data[ro..ro + rl]).into_owned();
    let qo = ro + rl;
    let ql = u16::from_le_bytes([data[qo], data[qo + 1]]) as usize;
    let qs = qo + 2;
    if data.len() < qs + ql {
        return None;
    }
    let query = String::from_utf8_lossy(&data[qs..qs + ql]).into_owned();
    Some((nonce, flags, max_matches, max_per_file, root, query))
}

/// Append one record to an uncompressed `FS_GREP` records buffer.
pub fn append_fs_grep_record(buf: &mut Vec<u8>, record: &FsGrepRecord) {
    let start = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // record_len placeholder
    match record {
        FsGrepRecord::File { flags, n, path } => {
            buf.push(FS_GREP_RECORD_FILE);
            buf.push(*flags);
            buf.extend_from_slice(&n.to_le_bytes());
            push_str(buf, path);
        }
        FsGrepRecord::Match {
            line,
            col,
            end_line,
            end_col,
            text,
        } => {
            buf.push(FS_GREP_RECORD_MATCH);
            buf.extend_from_slice(&line.to_le_bytes());
            buf.extend_from_slice(&col.to_le_bytes());
            buf.extend_from_slice(&end_line.to_le_bytes());
            buf.extend_from_slice(&end_col.to_le_bytes());
            let tb = text.as_bytes();
            buf.extend_from_slice(&(tb.len() as u32).to_le_bytes());
            buf.extend_from_slice(tb);
        }
    }
    let len = (buf.len() - start - 4) as u32;
    buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Decode an uncompressed `FS_GREP` records buffer. Unknown kinds are skipped
/// via `record_len`; a record whose body overruns ends the stream, matching
/// the TypeScript mirror.
pub fn fs_grep_records(data: &[u8]) -> Vec<FsGrepRecord> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= data.len() {
        let len = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        if len == 0 || off + 4 + len > data.len() {
            return out;
        }
        let body = &data[off + 4..off + 4 + len];
        off += 4 + len;
        match body[0] {
            FS_GREP_RECORD_FILE => {
                if body.len() < 6 {
                    return out;
                }
                let flags = body[1];
                let n = u16::from_le_bytes([body[2], body[3]]);
                let pl = u16::from_le_bytes([body[4], body[5]]) as usize;
                if body.len() < 6 + pl {
                    return out;
                }
                out.push(FsGrepRecord::File {
                    flags,
                    n,
                    path: String::from_utf8_lossy(&body[6..6 + pl]).into_owned(),
                });
            }
            FS_GREP_RECORD_MATCH => {
                if body.len() < 21 {
                    return out;
                }
                let line = u32::from_le_bytes(body[1..5].try_into().unwrap());
                let col = u32::from_le_bytes(body[5..9].try_into().unwrap());
                let end_line = u32::from_le_bytes(body[9..13].try_into().unwrap());
                let end_col = u32::from_le_bytes(body[13..17].try_into().unwrap());
                let tl = u32::from_le_bytes(body[17..21].try_into().unwrap()) as usize;
                if body.len() < 21 + tl {
                    return out;
                }
                out.push(FsGrepRecord::Match {
                    line,
                    col,
                    end_line,
                    end_col,
                    text: String::from_utf8_lossy(&body[21..21 + tl]).into_owned(),
                });
            }
            // Unknown kind: skipped via record_len, as the family requires.
            _ => {}
        }
    }
    out
}

/// Build an `S2C_FS_GREP` from an uncompressed records buffer.
pub fn msg_fs_grep_result(
    nonce: u16,
    status: u8,
    flags: u8,
    detail: &str,
    records: &[u8],
) -> Vec<u8> {
    let db = detail.as_bytes();
    let compressed = lz4_flex::compress_prepend_size(records);
    let mut m = Vec::with_capacity(7 + db.len() + compressed.len());
    m.push(S2C_FS_GREP);
    m.extend_from_slice(&nonce.to_le_bytes());
    m.push(status);
    m.push(flags);
    push_str(&mut m, detail);
    m.extend_from_slice(&compressed);
    m
}

/// Parse an `S2C_FS_GREP` → `(nonce, status, flags, detail, records)` with the
/// records decompressed under the standard guard.
pub fn parse_fs_grep_result(data: &[u8]) -> Option<(u16, u8, u8, String, Vec<u8>)> {
    if data.first().copied() != Some(S2C_FS_GREP) || data.len() < 7 {
        return None;
    }
    let nonce = u16::from_le_bytes([data[1], data[2]]);
    let status = data[3];
    let flags = data[4];
    let dl = u16::from_le_bytes([data[5], data[6]]) as usize;
    let ds = 7;
    if data.len() < ds + dl {
        return None;
    }
    let detail = String::from_utf8_lossy(&data[ds..ds + dl]).into_owned();
    let records = decompress_guarded(&data[ds + dl..])?;
    Some((nonce, status, flags, detail, records))
}

#[cfg(test)]
mod tests {
    /// A path longer than its `u16` prefix must be shortened, not wrapped.
    /// Paths come off a real filesystem and no protocol rule bounds them; a
    /// wrapped `len as u16` declares a length the reader believes and every
    /// field after it in the message is read at the wrong offset.
    #[test]
    fn overlong_paths_are_clipped_not_wrapped() {
        // [0x47][nonce:2][flags:1][root_len:2][root:N]
        let long = "a".repeat(u16::MAX as usize + 1);
        let m = msg_fs_index(1, &long);
        let declared = u16::from_le_bytes([m[4], m[5]]) as usize;
        assert_eq!(declared, u16::MAX as usize);
        assert_eq!(m.len(), 6 + declared, "prefix must match the bytes");
        let (nonce, _, root) = parse_fs_index(&m).expect("still parses");
        assert_eq!(nonce, 1);
        assert_eq!(root.len(), declared);
    }

    /// Clipping lands on a char boundary, so an oversized non-ASCII path
    /// stays decodable as UTF-8 rather than arriving mangled.
    #[test]
    fn clipping_respects_char_boundaries() {
        let wide = "é".repeat(u16::MAX as usize);
        let m = msg_fs_index(1, &wide);
        let declared = u16::from_le_bytes([m[4], m[5]]) as usize;
        assert!(declared <= u16::MAX as usize);
        assert_eq!(m.len(), 6 + declared);
        std::str::from_utf8(&m[6..]).expect("clipped path is still UTF-8");
    }

    /// The `S2C_FS_SEARCH` count is a `u16`. More results than it can
    /// describe must be dropped, not wrapped — a wrapped count leaves the
    /// reader consuming the wrong number of entries and desyncing.
    #[test]
    fn search_result_count_saturates_instead_of_wrapping() {
        let paths: Vec<String> = (0..=u16::MAX as usize + 1).map(|i| i.to_string()).collect();
        let m = msg_fs_search_result(7, FS_STATUS_OK, &paths);
        let declared = u16::from_le_bytes([m[4], m[5]]) as usize;
        assert_eq!(declared, u16::MAX as usize);
        let (nonce, status, out) = parse_fs_search_result(&m).expect("still parses");
        assert_eq!((nonce, status), (7, FS_STATUS_OK));
        assert_eq!(out.len(), declared, "emitted entries match the count");
    }

    #[test]
    fn done_status_text_distinguishes_other_from_unknown() {
        assert_eq!(fs_done_status_text(FS_DONE_OTHER), "backend error");
        assert_eq!(fs_done_status_text(200), "unknown status");
    }

    #[test]
    fn fs_grep_request_roundtrip() {
        // Pinned bytes; the TypeScript mirror asserts the same hex.
        let m = msg_fs_grep(
            0x0102,
            FS_GREP_CASE_SENSITIVE | FS_GREP_REGEX,
            500,
            50,
            "/tmp/root",
            "fn \\w+",
        );
        assert_eq!(
            m.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            // Cross-pinned with js/core/src/__tests__/fs.test.ts (the request
            // is uncompressed, so both sides can pin exact bytes).
            "48020103f401320009002f746d702f726f6f740600666e205c772b"
        );
        assert_eq!(
            parse_fs_grep(&m),
            Some((
                0x0102,
                FS_GREP_CASE_SENSITIVE | FS_GREP_REGEX,
                500,
                50,
                "/tmp/root".to_string(),
                "fn \\w+".to_string()
            ))
        );
        // Truncated frames are malformed, never partially accepted.
        for cut in 0..m.len() {
            assert_eq!(parse_fs_grep(&m[..cut]), None, "cut at {cut}");
        }
    }

    #[test]
    fn fs_grep_result_roundtrip() {
        let recs = vec![
            FsGrepRecord::File {
                flags: 0,
                n: 2,
                path: "src/main.rs".to_string(),
            },
            FsGrepRecord::Match {
                line: 41,
                col: 4,
                end_line: 41,
                end_col: 6,
                text: "    fn main() {".to_string(),
            },
            FsGrepRecord::Match {
                line: 99,
                col: 0,
                end_line: 99,
                end_col: 2,
                text: "fn helper()".to_string(),
            },
            // An ignored file sorts last and carries the flag.
            FsGrepRecord::File {
                flags: FS_GREP_FILE_IGNORED,
                n: 1,
                path: "target/debug/build.rs".to_string(),
            },
            FsGrepRecord::Match {
                line: 0,
                col: 0,
                end_line: 0,
                end_col: 2,
                text: String::new(),
            },
        ];
        let mut buf = Vec::new();
        for r in &recs {
            append_fs_grep_record(&mut buf, r);
        }
        assert_eq!(fs_grep_records(&buf), recs);

        let msg = msg_fs_grep_result(7, FS_DONE_OK, FS_GREP_TRUNCATED, "", &buf);
        let (nonce, status, flags, detail, records) = parse_fs_grep_result(&msg).unwrap();
        assert_eq!(
            (nonce, status, flags, detail.as_str()),
            (7, FS_DONE_OK, FS_GREP_TRUNCATED, "")
        );
        assert_eq!(fs_grep_records(&records), recs);

        // `detail` carries the regex error on INVALID, with no records.
        let bad = msg_fs_grep_result(8, FS_DONE_INVALID, 0, "unclosed character class", &[]);
        let (_, st, _, d, r) = parse_fs_grep_result(&bad).unwrap();
        assert_eq!(st, FS_DONE_INVALID);
        assert_eq!(d, "unclosed character class");
        assert!(fs_grep_records(&r).is_empty());
    }

    #[test]
    fn fs_grep_records_skip_unknown_kinds() {
        let mut buf = Vec::new();
        // An unknown kind between two known records is stepped over via
        // record_len rather than ending the stream.
        append_fs_grep_record(
            &mut buf,
            &FsGrepRecord::File {
                flags: 0,
                n: 0,
                path: "a".to_string(),
            },
        );
        let start = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.push(0x7f); // unknown kind
        buf.extend_from_slice(b"whatever");
        let len = (buf.len() - start - 4) as u32;
        buf[start..start + 4].copy_from_slice(&len.to_le_bytes());
        append_fs_grep_record(
            &mut buf,
            &FsGrepRecord::File {
                flags: 0,
                n: 0,
                path: "b".to_string(),
            },
        );
        let got = fs_grep_records(&buf);
        assert_eq!(got.len(), 2, "unknown kind must not end the stream");

        // A record whose body overruns its own length ends the stream.
        let mut trunc = buf.clone();
        trunc.truncate(trunc.len() - 1);
        assert!(fs_grep_records(&trunc).len() <= 2);
    }

    use super::*;

    #[test]
    fn fs_search_request_roundtrip() {
        let m = msg_fs_search(9, 50, "/a/b:c", "eng.rs");
        let (nonce, limit, root, query) = parse_fs_search(&m).unwrap();
        assert_eq!(nonce, 9);
        assert_eq!(limit, 50);
        assert_eq!(root, "/a/b:c");
        assert_eq!(query, "eng.rs");
    }

    #[test]
    fn fs_search_result_roundtrip() {
        let paths = vec!["src/main.rs".to_string(), "a/b:c/engine.rs".to_string()];
        let m = msg_fs_search_result(3, FS_STATUS_OK, &paths);
        let (nonce, status, out) = parse_fs_search_result(&m).unwrap();
        assert_eq!(nonce, 3);
        assert_eq!(status, FS_STATUS_OK);
        assert_eq!(out, paths);
    }

    #[test]
    fn fs_index_request_roundtrip() {
        let m = msg_fs_index(0x0102, "/tmp/watch me");
        // Cross-pinned with js/core/src/__tests__/fs.test.ts (uncompressed,
        // so both sides can pin exact bytes).
        assert_eq!(
            m.iter().map(|x| format!("{x:02x}")).collect::<String>(),
            "470201000d002f746d702f7761746368206d65"
        );
        let (nonce, flags, root) = parse_fs_index(&m).unwrap();
        assert_eq!(nonce, 0x0102);
        assert_eq!(flags, 0);
        assert_eq!(root, "/tmp/watch me");
        assert_eq!(parse_fs_index(&m[..5]), None);
        assert_eq!(parse_fs_index(&msg_fs_stop(1)), None);
    }

    #[test]
    fn fs_index_result_roundtrip() {
        let paths = vec![
            "a/b:c/engine.rs".to_string(),
            "src/main.rs".to_string(),
            String::new(),
        ];
        let m = msg_fs_index_result(3, FS_DONE_OK, FS_INDEX_TRUNCATED, &paths);
        let (nonce, status, flags, out) = parse_fs_index_result(&m).unwrap();
        assert_eq!(nonce, 3);
        assert_eq!(status, FS_DONE_OK);
        assert_eq!(flags, FS_INDEX_TRUNCATED);
        assert_eq!(out, paths);

        // Empty list (the error-status shape) still carries a valid LZ4 blob.
        let empty = msg_fs_index_result(4, FS_DONE_NOT_FOUND, 0, &[]);
        let (_, status, _, out) = parse_fs_index_result(&empty).unwrap();
        assert_eq!(status, FS_DONE_NOT_FOUND);
        assert!(out.is_empty());

        // A count that disagrees with the payload is malformed.
        let mut lying = msg_fs_index_result(5, FS_DONE_OK, 0, &paths);
        lying[5..9].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(parse_fs_index_result(&lying), None);

        // A count over the protocol cap is rejected before decompression —
        // a hostile 33M-record claim must not reach the preallocation.
        let mut huge = msg_fs_index_result(6, FS_DONE_OK, 0, &[]);
        huge[5..9].copy_from_slice(&((FS_INDEX_MAX_COUNT as u32) + 1).to_le_bytes());
        assert_eq!(parse_fs_index_result(&huge), None);
    }

    #[test]
    fn single_and_recursive_are_mutually_exclusive() {
        assert!(fs_sync_flags_valid(FS_SYNC_SINGLE));
        assert!(fs_sync_flags_valid(FS_SYNC_SINGLE | FS_SYNC_CONTENT));
        assert!(fs_sync_flags_valid(FS_SYNC_RECURSIVE | FS_SYNC_CONTENT));
        assert!(!fs_sync_flags_valid(FS_SYNC_SINGLE | FS_SYNC_RECURSIVE));
        assert!(!fs_sync_flags_valid(
            FS_SYNC_SINGLE | FS_SYNC_RECURSIVE | FS_SYNC_CONTENT
        ));
    }

    #[test]
    fn fs_sync_from_pty_roundtrip_and_rebase() {
        let m = msg_fs_sync_from_pty(7, FS_SYNC_RECURSIVE, 0, 0, "sub", 42);
        assert_eq!(
            m,
            vec![
                0x40, 0x07, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x73,
                0x75, 0x62, 0x2a, 0x00
            ]
        );
        assert_eq!(fs_sync_src_pty(&m), Some(42));
        assert_eq!(
            fs_sync_src_pty(&msg_fs_sync(7, FS_SYNC_RECURSIVE, 0, 0, "sub")),
            None
        );
        // Rebase joins cwd + path and clears FROM_PTY.
        let reb = fs_sync_rebase(&m, "/home/u").unwrap();
        assert_eq!(fs_sync_flags(&reb).unwrap() & FS_SYNC_FROM_PTY, 0);
        let plen = u16::from_le_bytes([reb[11], reb[12]]) as usize;
        assert_eq!(
            std::str::from_utf8(&reb[13..13 + plen]).unwrap(),
            "/home/u/sub"
        );
    }

    /// The two optional trailers coexist in one message, and each is
    /// reachable past the other — the property that makes the field order
    /// (`EXCLUDE` then `FROM_PTY`) load-bearing.
    #[test]
    fn fs_sync_exclude_field_roundtrips_alongside_from_pty() {
        let plain = msg_fs_sync(1, FS_SYNC_RECURSIVE, 0, 0, "sub");
        assert_eq!(fs_sync_flags(&plain).unwrap() & FS_SYNC_EXCLUDE, 0);
        assert_eq!(fs_sync_exclude(&plain), Some(""));

        // Empty patterns build the plain form: no field, no flag.
        assert_eq!(
            msg_fs_sync_excluding(1, FS_SYNC_RECURSIVE, 0, 0, "sub", ""),
            plain
        );

        let ex = msg_fs_sync_excluding(1, FS_SYNC_RECURSIVE, 0, 0, "sub", "target\n!keep");
        assert_ne!(fs_sync_flags(&ex).unwrap() & FS_SYNC_EXCLUDE, 0);
        assert_eq!(fs_sync_exclude(&ex), Some("target\n!keep"));
        assert_eq!(fs_sync_src_pty(&ex), None);

        let both = msg_fs_sync_full(1, FS_SYNC_RECURSIVE, 0, 0, "sub", "target", Some(42));
        assert_eq!(fs_sync_exclude(&both), Some("target"));
        assert_eq!(fs_sync_src_pty(&both), Some(42), "reached past the field");
        let reb = fs_sync_rebase(&both, "/home/u").unwrap();
        assert_eq!(fs_sync_flags(&reb).unwrap() & FS_SYNC_FROM_PTY, 0);
        assert_ne!(fs_sync_flags(&reb).unwrap() & FS_SYNC_EXCLUDE, 0);
        assert_eq!(fs_sync_exclude(&reb), Some("target"), "filter survives");
        let plen = u16::from_le_bytes([reb[11], reb[12]]) as usize;
        assert_eq!(
            std::str::from_utf8(&reb[13..13 + plen]).unwrap(),
            "/home/u/sub"
        );

        // A truncated field is malformed, not an empty pattern list.
        assert_eq!(fs_sync_exclude(&ex[..ex.len() - 1]), None);
        let mut headerless = ex.clone();
        headerless.truncate(17);
        assert_eq!(fs_sync_exclude(&headerless), None);
    }

    /// Exclusion narrows enumeration, and a `SINGLE` sync enumerates
    /// nothing: the combination is a client misunderstanding, refused
    /// rather than silently ignored.
    #[test]
    fn exclusion_flags_are_rejected_with_single() {
        for flag in [FS_SYNC_EXCLUDE_GIT, FS_SYNC_GITIGNORE, FS_SYNC_EXCLUDE] {
            assert!(fs_sync_flags_valid(flag));
            assert!(fs_sync_flags_valid(flag | FS_SYNC_RECURSIVE));
            assert!(!fs_sync_flags_valid(flag | FS_SYNC_SINGLE));
        }
    }

    /// A staging sync roots at the connection's drag staging dir: the flag
    /// is known, the rebase swaps the empty client path for the resolved
    /// dir and clears the flag, and `FROM_PTY` contradicts it at both
    /// levels — flag validation and the rebase itself.
    #[test]
    fn staging_sync_rebases_onto_the_staging_dir() {
        assert_ne!(FS_SYNC_FLAGS_KNOWN & FS_SYNC_STAGING, 0);
        assert!(fs_sync_flags_valid(FS_SYNC_STAGING | FS_SYNC_RECURSIVE));
        assert!(!fs_sync_flags_valid(FS_SYNC_STAGING | FS_SYNC_FROM_PTY));

        let m = msg_fs_sync_staging(9, FS_SYNC_RECURSIVE, 0, 0);
        assert_eq!(
            fs_sync_flags(&m).unwrap(),
            FS_SYNC_STAGING | FS_SYNC_RECURSIVE
        );
        let plen = u16::from_le_bytes([m[11], m[12]]) as usize;
        assert_eq!(plen, 0, "the client sends an empty path");

        let reb = fs_sync_rebase_staging(&m, "/tmp/blit_drag_1_2").expect("staging rebase");
        assert_eq!(fs_sync_flags(&reb).unwrap(), FS_SYNC_RECURSIVE);
        let plen = u16::from_le_bytes([reb[11], reb[12]]) as usize;
        assert_eq!(
            std::str::from_utf8(&reb[FS_SYNC_HEADER..FS_SYNC_HEADER + plen]).unwrap(),
            "/tmp/blit_drag_1_2"
        );
        assert_eq!(
            u16::from_le_bytes([reb[1], reb[2]]),
            9,
            "the nonce survives"
        );

        // No flag, no rebase; FROM_PTY is refused, not rebased.
        assert_eq!(
            fs_sync_rebase_staging(&msg_fs_sync(9, FS_SYNC_RECURSIVE, 0, 0, ""), "/tmp/x"),
            None
        );
        assert_eq!(
            fs_sync_rebase_staging(
                &msg_fs_sync_full(9, FS_SYNC_STAGING, 0, 0, "", "", Some(3)),
                "/tmp/x"
            ),
            None
        );
        // A truncated message is malformed, not a staging sync.
        assert_eq!(
            fs_sync_rebase_staging(&m[..FS_SYNC_HEADER - 1], "/tmp/x"),
            None
        );
    }

    fn upsert(path: &str, content: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        append_fs_record(
            &mut buf,
            &FsRecord::Upsert {
                path,
                entry_flags: FS_ENTRY_FILE,
                size: content.len() as u64,
                mtime_ns: 42,
                mode: 0o644,
                hash: 7,
                content: FsContent::Full(content),
            },
        );
        buf
    }

    #[test]
    fn record_roundtrip() {
        let mut buf = Vec::new();
        append_fs_record(
            &mut buf,
            &FsRecord::Upsert {
                path: "a/b.txt",
                entry_flags: FS_ENTRY_FILE | FS_ENTRY_NO_CONTENT,
                size: 10,
                mtime_ns: 1_700_000_000_000_000_000,
                mode: 0o755,
                hash: 0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF,
                content: FsContent::None,
            },
        );
        append_fs_record(&mut buf, &FsRecord::Delete { path: "old" });
        append_fs_record(
            &mut buf,
            &FsRecord::Move {
                from: "src",
                to: "dst",
            },
        );
        let records: Vec<_> = fs_records(&buf).collect();
        assert_eq!(records.len(), 3);
        match &records[0] {
            FsRecord::Upsert {
                path,
                entry_flags,
                size,
                mtime_ns,
                mode,
                hash,
                content,
            } => {
                assert_eq!(*path, "a/b.txt");
                assert_eq!(*entry_flags, FS_ENTRY_FILE | FS_ENTRY_NO_CONTENT);
                assert_eq!(*size, 10);
                assert_eq!(*mtime_ns, 1_700_000_000_000_000_000);
                assert_eq!(*mode, 0o755);
                assert_eq!(*hash, 0xDEAD_BEEF_DEAD_BEEF_DEAD_BEEF);
                assert_eq!(*content, FsContent::None);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(records[1], FsRecord::Delete { path: "old" });
        assert_eq!(
            records[2],
            FsRecord::Move {
                from: "src",
                to: "dst"
            }
        );
    }

    /// Byte fixtures shared with the TypeScript codecs
    /// (`js/core/src/__tests__/fs.test.ts` pins the same hex), so codec
    /// drift fails on one side or the other. The compressed `FS_UPDATE`
    /// variant is pinned only in TS — LZ4 output may legitimately change
    /// across `lz4_flex` versions, while these buffers never can.
    #[test]
    fn wire_fixtures() {
        fn hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }

        assert_eq!(
            hex(&msg_fs_sync(
                0x0102,
                FS_SYNC_RECURSIVE | FS_SYNC_CONTENT,
                25,
                65536,
                "/tmp/watch me"
            )),
            "40020103001900000001000d002f746d702f7761746368206d65"
        );
        // Both optional trailers, in field order: EXCLUDE then FROM_PTY.
        assert_eq!(
            hex(&msg_fs_sync_full(
                7,
                FS_SYNC_RECURSIVE,
                0,
                0,
                "sub",
                "target",
                Some(42)
            )),
            "4007009100000000000000030073756206007461726765742a00"
        );
        assert_eq!(hex(&msg_fs_stop(0x0102)), "410201");
        assert_eq!(hex(&msg_fs_ack(0x0102, 0x01020304)), "42020104030201");
        assert_eq!(
            hex(&msg_fs_fetch(3, 0x0102, "sub/%FF.bin")),
            "43030002010b007375622f2546462e62696e"
        );
        assert_eq!(
            hex(&msg_fs_synced(0x0102, 3, 0, "/w")),
            "40020103000002002f77"
        );

        let mut records = Vec::new();
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "a.txt",
                entry_flags: FS_ENTRY_FILE,
                size: 5,
                mtime_ns: 1_700_000_000_123_456_789,
                mode: 0o100644,
                hash: 0x0123_4567_89ab_cdef_1122_3344_5566_7788,
                content: FsContent::Full(b"hello"),
            },
        );
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "sub",
                entry_flags: FS_ENTRY_DIR,
                size: 0,
                mtime_ns: 0,
                mode: 0o40755,
                hash: 0,
                content: FsContent::None,
            },
        );
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "sub/%FF.bin", // server-escaped non-UTF-8 name
                entry_flags: FS_ENTRY_FILE | FS_ENTRY_NO_CONTENT,
                size: 1 << 20,
                mtime_ns: 1,
                mode: 0o100600,
                hash: 0xff,
                content: FsContent::None,
            },
        );
        append_fs_record(&mut records, &FsRecord::Delete { path: "old" });
        append_fs_record(
            &mut records,
            &FsRecord::Move {
                from: "src",
                to: "dst",
            },
        );
        assert_eq!(
            hex(&records),
            "3700000001000500612e747874050000000000000015cd853dfe9c9717a48100008877665544332211efcdab8967452301010500000068656c6c6f2c0000000101030073756200000000000000000000000000000000ed41000000000000000000000000000000000000003400000001080b007375622f2546462e62696e0000100000000000010000000000000080810000ff00000000000000000000000000000000060000000203006f6c640b0000000303007372630300647374"
        );

        // Decode direction: the pinned bytes parse back to the same records.
        let decoded: Vec<_> = fs_records(&records).collect();
        assert_eq!(decoded.len(), 5);
        assert!(matches!(
            &decoded[0],
            FsRecord::Upsert {
                path: "a.txt",
                size: 5,
                mtime_ns: 1_700_000_000_123_456_789,
                hash: 0x0123_4567_89ab_cdef_1122_3344_5566_7788,
                content: FsContent::Full(b"hello"),
                ..
            }
        ));
        assert_eq!(decoded[3], FsRecord::Delete { path: "old" });
        assert_eq!(
            decoded[4],
            FsRecord::Move {
                from: "src",
                to: "dst"
            }
        );
    }

    #[test]
    fn oversized_declared_length_is_rejected_before_allocation() {
        // A hand-forged FS_UPDATE whose LZ4 size prefix declares 1 GiB.
        let mut msg = vec![S2C_FS_UPDATE];
        msg.extend_from_slice(&1u16.to_le_bytes()); // sync_id
        msg.extend_from_slice(&1u32.to_le_bytes()); // update_id
        msg.push(0); // flags
        msg.extend_from_slice(&(1u32 << 30).to_le_bytes()); // declared size
        msg.extend_from_slice(&[0u8; 16]); // bogus compressed bytes
        let mut mirror = FsMirror::new();
        assert_eq!(mirror.apply_update(&msg), None);

        let mut file = vec![S2C_FS_FILE];
        file.extend_from_slice(&7u16.to_le_bytes()); // nonce
        file.push(FS_FILE_OK);
        file.extend_from_slice(&(1u32 << 30).to_le_bytes());
        file.extend_from_slice(&[0u8; 16]);
        assert_eq!(parse_fs_file(&file), None);
    }

    #[test]
    fn fs_file_roundtrip() {
        let msg = msg_fs_file(9, FS_FILE_OK, b"contents");
        assert_eq!(
            parse_fs_file(&msg),
            Some((9, FS_FILE_OK, b"contents".to_vec()))
        );
    }

    #[test]
    fn fs_write_roundtrip() {
        let w = FsWrite {
            nonce: 7,
            sync_id: 3,
            flags: FS_WRITE_MKPARENTS | FS_WRITE_DURABLE,
            base: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
            mode: 0o644,
            content_kind: FS_WRITE_CONTENT_FULL,
            path: "dir/50%25.txt".to_string(),
            content: b"hello world".to_vec(),
        };
        assert_eq!(parse_fs_write(&msg_fs_write(&w)), Some(w));
        // Empty content (create-empty) and zero base (create-exclusive).
        let w0 = FsWrite {
            nonce: 1,
            sync_id: 1,
            flags: 0,
            base: 0,
            mode: 0,
            content_kind: FS_WRITE_CONTENT_FULL,
            path: "new.txt".to_string(),
            content: Vec::new(),
        };
        assert_eq!(parse_fs_write(&msg_fs_write(&w0)), Some(w0));
        // Truncated header and wrong opcode are rejected.
        assert_eq!(parse_fs_write(&[C2S_FS_WRITE, 0, 0]), None);
        assert_eq!(parse_fs_write(&msg_fs_file(1, 0, b"x")), None);
    }

    #[test]
    fn fs_op_roundtrip() {
        let rename = FsOp {
            nonce: 42,
            sync_id: 9,
            op: FS_OP_RENAME,
            flags: FS_OP_MKPARENTS,
            base: 0,
            mode: 0,
            a: "old/name".to_string(),
            b: "new/name".to_string(),
        };
        assert_eq!(parse_fs_op(&msg_fs_op(&rename)), Some(rename));
        let mkdir = FsOp {
            nonce: 2,
            sync_id: 1,
            op: FS_OP_MKDIR,
            flags: 0,
            base: 0,
            mode: 0o700,
            a: "sub".to_string(),
            b: String::new(),
        };
        assert_eq!(parse_fs_op(&msg_fs_op(&mkdir)), Some(mkdir));
        assert_eq!(parse_fs_op(&[C2S_FS_OP, 0],), None);
    }

    #[test]
    fn fs_write_family_byte_fixtures() {
        // Pinned bytes, cross-checked with js/core/src/__tests__/fs.test.ts.
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let w = FsWrite {
            nonce: 0x0102,
            sync_id: 0x0304,
            flags: FS_WRITE_MKPARENTS,
            base: 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100,
            mode: 0o644,
            content_kind: FS_WRITE_CONTENT_FULL,
            path: "a/b.txt".into(),
            content: b"hi".to_vec(),
        };
        assert_eq!(
            hex(&msg_fs_write(&w)),
            "440201040302000102030405060708090a0b0c0d0e0fa4010000010700612f622e74787402000000206869"
        );
        let o = FsOp {
            nonce: 0x0102,
            sync_id: 0x0304,
            op: FS_OP_RENAME,
            flags: FS_OP_MKPARENTS,
            base: 0,
            mode: 0,
            a: "x".into(),
            b: "y".into(),
        };
        assert_eq!(
            hex(&msg_fs_op(&o)),
            "450201040303020000000000000000000000000000000000000000010078010079"
        );
        // A symlink target is a verbatim string, never a wire path — "../t"
        // rides the `a` field unescaped and unvalidated.
        let ln = FsOp {
            nonce: 0x0102,
            sync_id: 0x0304,
            op: FS_OP_SYMLINK,
            flags: FS_OP_NO_CAS,
            base: 0,
            mode: 0,
            a: "../t".into(),
            b: "l".into(),
        };
        assert_eq!(
            hex(&msg_fs_op(&ln)),
            "45020104030401000000000000000000000000000000000000000004002e2e2f7401006c"
        );
        assert_eq!(
            hex(&msg_fs_done(
                0x0102,
                FS_DONE_CONFLICT,
                0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100,
                0x1122_3344_5566_7788
            )),
            "4402010b000102030405060708090a0b0c0d0e0f8877665544332211"
        );
    }

    #[test]
    fn fs_done_roundtrip() {
        let hash = 0xdead_beef_dead_beef_dead_beef_dead_beefu128;
        let msg = msg_fs_done(5, FS_DONE_OK, hash, 1_700_000_000_000_000_000);
        assert_eq!(
            parse_fs_done(&msg),
            Some((5, FS_DONE_OK, hash, 1_700_000_000_000_000_000))
        );
        // CONFLICT carries the current disk hash.
        let c = msg_fs_done(6, FS_DONE_CONFLICT, hash, 0);
        assert_eq!(parse_fs_done(&c), Some((6, FS_DONE_CONFLICT, hash, 0)));
    }

    #[test]
    fn fs_upload_roundtrips() {
        let b = FsUploadBegin {
            nonce: 7,
            sync_id: 3,
            flags: FS_UPLOAD_MKPARENTS | FS_UPLOAD_DURABLE,
            base: 0x0123_4567_89ab_cdef_0123_4567_89ab_cdef,
            mode: 0o644,
            size: 1_000_000_001,
            path: "dir/big.bin".to_string(),
        };
        assert_eq!(parse_fs_upload_begin(&msg_fs_upload_begin(&b)), Some(b));
        assert_eq!(parse_fs_upload_begin(&[C2S_FS_UPLOAD_BEGIN, 0, 0]), None);

        assert_eq!(
            parse_fs_upload_chunk(&msg_fs_upload_chunk(9, 4096, b"chunk-data")),
            Some((9, 4096, b"chunk-data".to_vec()))
        );
        // An empty chunk (a zero-length final flush) round-trips too.
        assert_eq!(
            parse_fs_upload_chunk(&msg_fs_upload_chunk(9, 0, b"")),
            Some((9, 0, Vec::new()))
        );
        assert_eq!(parse_fs_upload_chunk(&[C2S_FS_UPLOAD_CHUNK, 0]), None);
        // The decompression guard applies to chunk data.
        let mut forged = vec![C2S_FS_UPLOAD_CHUNK];
        forged.extend_from_slice(&1u16.to_le_bytes());
        forged.extend_from_slice(&0u64.to_le_bytes());
        forged.extend_from_slice(&(1u32 << 30).to_le_bytes());
        forged.extend_from_slice(&[0u8; 8]);
        assert_eq!(parse_fs_upload_chunk(&forged), None);

        assert_eq!(
            parse_fs_upload_finish(&msg_fs_upload_finish(5, 9)),
            Some((5, 9))
        );
        assert_eq!(parse_fs_upload_finish(&[C2S_FS_UPLOAD_FINISH, 0]), None);
        assert_eq!(msg_fs_upload_cancel(9), vec![C2S_FS_UPLOAD_CANCEL, 9, 0]);

        let hash = 0xdead_beef_dead_beef_dead_beef_dead_beefu128;
        assert_eq!(
            parse_fs_upload_begin_result(&msg_fs_upload_begin_result(
                7,
                FS_DONE_CONFLICT,
                9,
                hash,
                0
            )),
            Some((7, FS_DONE_CONFLICT, 9, hash, 0))
        );
        assert_eq!(
            parse_fs_upload_chunk_result(&msg_fs_upload_chunk_result(
                9,
                FS_DONE_OFFSET_MISMATCH,
                4096
            )),
            Some((9, FS_DONE_OFFSET_MISMATCH, 4096))
        );
        assert_eq!(
            parse_fs_upload_finish_result(&msg_fs_upload_finish_result(
                5,
                FS_DONE_OK,
                hash,
                1_700_000_000_000_000_000
            )),
            Some((5, FS_DONE_OK, hash, 1_700_000_000_000_000_000))
        );
    }

    #[test]
    fn fs_upload_byte_fixtures() {
        // Pinned bytes, same discipline as fs_write_family_byte_fixtures.
        // The u64 size/offset fixture value keeps its significant bits under
        // 2^53 so the JavaScript fixtures (js/core/src/__tests__/fs.test.ts,
        // where wire sizes are `number`) pin the identical bytes.
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let b = FsUploadBegin {
            nonce: 0x0102,
            sync_id: 0x0304,
            flags: FS_UPLOAD_MKPARENTS,
            base: 0,
            mode: 0o644,
            size: 0x0102_0304_0506_0000,
            path: "a/b.txt".into(),
        };
        assert_eq!(
            hex(&msg_fs_upload_begin(&b)),
            "49020104030200000000000000000000000000000000a401000000000605040302010700612f622e747874"
        );
        assert_eq!(hex(&msg_fs_upload_finish(0x0102, 0x0506)), "4b02010605");
        assert_eq!(
            hex(&msg_fs_upload_begin_result(
                0x0102, FS_DONE_OK, 0x0506, 0, 0
            )),
            "490201000605000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            hex(&msg_fs_upload_chunk_result(
                0x0506,
                FS_DONE_OK,
                0x0102_0304_0506_0000
            )),
            "4a0605000000060504030201"
        );
    }

    #[test]
    fn unknown_record_kind_is_skipped() {
        let mut buf = Vec::new();
        // A future record kind 0x7F with 3 payload bytes.
        buf.extend_from_slice(&4u32.to_le_bytes());
        buf.push(0x7F);
        buf.extend_from_slice(&[1, 2, 3]);
        append_fs_record(&mut buf, &FsRecord::Delete { path: "x" });
        let records: Vec<_> = fs_records(&buf).collect();
        assert_eq!(records, vec![FsRecord::Delete { path: "x" }]);
    }

    #[test]
    fn mirror_staged_snapshot_and_live() {
        let mut mirror = FsMirror::new();
        // Snapshot: RESET+SYNC with two files.
        let mut records = upsert("a.txt", b"alpha");
        records.extend_from_slice(&upsert("d/b.txt", b"beta"));
        let msg = msg_fs_update(1, 1, FS_UPDATE_RESET | FS_UPDATE_SYNC, &records);
        assert_eq!(mirror.apply_update(&msg), Some(1));
        assert_eq!(mirror.live.len(), 2);
        assert_eq!(mirror.live["a.txt"].content.as_deref(), Some(&b"alpha"[..]));

        // Live delete + move.
        let mut records = Vec::new();
        append_fs_record(&mut records, &FsRecord::Delete { path: "a.txt" });
        append_fs_record(&mut records, &FsRecord::Move { from: "d", to: "e" });
        let msg = msg_fs_update(1, 2, 0, &records);
        assert_eq!(mirror.apply_update(&msg), Some(2));
        assert_eq!(mirror.live.len(), 1);
        assert_eq!(
            mirror.live["e/b.txt"].content.as_deref(),
            Some(&b"beta"[..])
        );

        // Mid-stream RESET without SYNC leaves live untouched…
        let msg = msg_fs_update(1, 3, FS_UPDATE_RESET, &upsert("n.txt", b"new"));
        assert_eq!(mirror.apply_update(&msg), Some(3));
        assert_eq!(mirror.live.len(), 1);
        // …until SYNC swaps atomically.
        let msg = msg_fs_update(1, 4, FS_UPDATE_SYNC, &[]);
        assert_eq!(mirror.apply_update(&msg), Some(4));
        assert_eq!(mirror.live.len(), 1);
        assert!(mirror.live.contains_key("n.txt"));
    }

    #[test]
    fn delta_content() {
        let mut mirror = FsMirror::new();
        let msg = msg_fs_update(
            1,
            1,
            FS_UPDATE_RESET | FS_UPDATE_SYNC,
            &upsert("f", b"hello world"),
        );
        mirror.apply_update(&msg).unwrap();

        // COPY(0,6) + INSERT("blit") == "hello blit"
        let ops: Vec<u8> = vec![0x01, 0, 6, 0x02, 4, b'b', b'l', b'i', b't'];
        let mut records = Vec::new();
        append_fs_record(
            &mut records,
            &FsRecord::Upsert {
                path: "f",
                entry_flags: FS_ENTRY_FILE,
                size: 10,
                mtime_ns: 43,
                mode: 0o644,
                hash: 8,
                content: FsContent::Delta(&ops),
            },
        );
        let msg = msg_fs_update(1, 2, 0, &records);
        mirror.apply_update(&msg).unwrap();
        assert_eq!(
            mirror.live["f"].content.as_deref(),
            Some(&b"hello blit"[..])
        );
    }

    #[test]
    fn subtree_semantics() {
        let mut map = BTreeMap::new();
        for p in ["a", "a/b", "a/b/c", "ab", "z"] {
            map.insert(
                p.to_string(),
                FsNode {
                    entry_flags: FS_ENTRY_FILE,
                    size: 0,
                    mtime_ns: 0,
                    mode: 0,
                    hash: 0,
                    content: None,
                },
            );
        }
        // "ab" must not match subtree "a" — and neither may a taken (moved)
        // subtree, even though "ab" sorts between "a" and "a/b".
        let taken = take_subtree(&mut map.clone(), "a");
        let suffixes: Vec<_> = taken.iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(
            suffixes,
            vec![String::new(), "b".to_string(), "b/c".to_string()]
        );
        remove_subtree(&mut map, "a");
        let left: Vec<_> = map.keys().cloned().collect();
        assert_eq!(left, vec!["ab".to_string(), "z".to_string()]);
    }

    #[test]
    fn read_request_round_trips() {
        let paths = ["/usr/share/applications/vlc.desktop", "/etc/os-release"];
        let wire = msg_fs_read_paths(7, 0, 4096, &paths).expect("builds");
        assert_eq!(
            parse_fs_read(&wire),
            Some((
                7,
                0,
                4096,
                vec![paths.iter().map(|p| Some((*p).to_string())).collect()]
            ))
        );
    }

    /// A path this family cannot name must not cost the frame its reply: the
    /// caller is waiting on that nonce, and nothing else will ever carry it.
    #[test]
    fn a_non_utf8_path_is_a_record_not_a_dropped_frame() {
        // Built by hand: `msg_fs_read` takes `&str`, so the wire is the only
        // place a path like this can come from.
        let bad = b"/tmp/\xff\xfe.png";
        let mut wire = vec![C2S_FS_READ, 5, 0, 0];
        wire.extend_from_slice(&4096u32.to_le_bytes());
        wire.extend_from_slice(&1u16.to_le_bytes()); // one group
        wire.extend_from_slice(&2u16.to_le_bytes()); // two paths in it
        wire.extend_from_slice(&(bad.len() as u16).to_le_bytes());
        wire.extend_from_slice(bad);
        wire.extend_from_slice(&(b"/etc/os-release".len() as u16).to_le_bytes());
        wire.extend_from_slice(b"/etc/os-release");
        let (nonce, _, _, groups) = parse_fs_read(&wire).expect("the frame is well formed");
        assert_eq!(nonce, 5);
        assert_eq!(
            groups,
            vec![vec![None, Some("/etc/os-release".to_string())]],
            "the undecodable path is None, and the readable one beside it survives"
        );
    }

    /// One group per question, which is what makes a screenful of icons one
    /// message: each group is answered by its own first readable path.
    #[test]
    fn a_grouped_request_keeps_its_groups() {
        let first: &[&str] = &["/i/scalable/apps/a.svg", "/i/48x48/apps/a.png"];
        let second: &[&str] = &["/i/scalable/apps/b.svg"];
        let wire = msg_fs_read(9, FS_READ_FIRST, 0, &[first, second]).expect("builds");
        let (nonce, flags, max_bytes, groups) = parse_fs_read(&wire).expect("parses");
        assert_eq!((nonce, flags, max_bytes), (9, FS_READ_FIRST, 0));
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
        assert_eq!(groups[1], vec![Some("/i/scalable/apps/b.svg".to_string())]);
    }

    #[test]
    fn read_result_carries_a_status_per_path() {
        let wire = msg_fs_read_result(
            7,
            FS_DONE_OK,
            &[
                (FS_FILE_OK, "/etc/os-release", b"NAME=NixOS\n"),
                (FS_FILE_NOT_FOUND, "/nope", b""),
                // Oversized paths carry no body even if one is passed.
                (FS_FILE_TOO_LARGE, "/huge.png", b"ignored"),
            ],
        );
        let (nonce, status, records) = parse_fs_read_result(&wire).expect("parses");
        assert_eq!((nonce, status), (7, FS_DONE_OK));
        assert_eq!(
            records,
            vec![
                (
                    FS_FILE_OK,
                    "/etc/os-release".to_string(),
                    b"NAME=NixOS\n".to_vec()
                ),
                (FS_FILE_NOT_FOUND, "/nope".to_string(), Vec::new()),
                (FS_FILE_TOO_LARGE, "/huge.png".to_string(), Vec::new()),
            ]
        );
    }

    #[test]
    fn read_rejects_what_it_cannot_answer() {
        assert!(msg_fs_read(1, 0, 0, &[]).is_none());
        assert!(msg_fs_read_paths(1, 0, 0, &[]).is_none());
        // The cap is on paths in total, however they are grouped.
        let too_many: Vec<&str> = vec!["/x"; FS_READ_MAX_PATHS + 1];
        assert!(msg_fs_read_paths(1, 0, 0, &too_many).is_none());
        // A count that outruns the body, and trailing bytes past the last path.
        assert!(parse_fs_read(&[C2S_FS_READ, 1, 0, 0, 0, 0, 0, 0, 2, 0]).is_none());
        let mut trailing = msg_fs_read_paths(1, 0, 0, &["/x"]).expect("builds");
        trailing.push(0);
        assert!(parse_fs_read(&trailing).is_none());
        // Zero paths is not a request the server should have to interpret.
        assert!(parse_fs_read(&[C2S_FS_READ, 1, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn an_empty_read_result_is_a_valid_answer() {
        // What FIRST reports when nothing matched: no records, status OK.
        let wire = msg_fs_read_result(9, FS_DONE_OK, &[]);
        assert_eq!(
            parse_fs_read_result(&wire),
            Some((9, FS_DONE_OK, Vec::new()))
        );
    }
}
