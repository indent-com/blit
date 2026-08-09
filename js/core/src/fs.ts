/**
 * Filesystem state sync (docs/fs-watch.md): wire constants, message
 * builders, record codecs, and the client-side mirror reducer.
 *
 * The server maintains a canonical replica of a watched tree and streams
 * ordered state diffs (`FS_UPDATE`). The complete client obligation is
 * {@link FsMirror}: apply records to a map, acknowledge. Loss, overflow,
 * and recovery are not wire concepts — the server restages (`RESET … SYNC`)
 * whenever an incremental diff is not possible.
 *
 * All integers little-endian, tightly packed, as everywhere in the protocol.
 */

import type { SessionId } from "./types.js";
import type { ReactiveStore } from "./reactive.js";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

// -- Opcodes ----------------------------------------------------------------

/** Start a sync: [0x40][nonce:2][flags:2][latency_ms:2][inline_max:4][path_len:2][path:N]
 *  then, with `FS_SYNC_EXCLUDE`, [exclude_len:2][exclude:M]; then, with
 *  `FS_SYNC_FROM_PTY`, [src_pty_id:2]. */
export const C2S_FS_SYNC = 0x40;
/** Stop a sync: [0x41][sync_id:2] */
export const C2S_FS_STOP = 0x41;
/** Cumulative acknowledgement: [0x42][sync_id:2][update_id:4] */
export const C2S_FS_ACK = 0x42;
/** Fetch full content of one file: [0x43][nonce:2][sync_id:2][path_len:2][path:N] */
export const C2S_FS_FETCH = 0x43;

/** Sync accepted or rejected: [0x40][nonce:2][sync_id:2][status:1][detail_len:2][detail:N] */
export const S2C_FS_SYNCED = 0x40;
/** State diff: [0x41][sync_id:2][update_id:4][flags:1][records:LZ4] */
export const S2C_FS_UPDATE = 0x41;
/** Fetch response: [0x42][nonce:2][status:1][data:LZ4] */
export const S2C_FS_FILE = 0x42;
/** Sync terminated: [0x43][sync_id:2][reason:1] */
export const S2C_FS_CLOSED = 0x43;

/** `S2C_HELLO` feature bit: server supports the `FS_*` message family,
 * reads and writes alike. A read-only deployment (`BLIT_FS_WRITE=0` on
 * the server) still advertises this bit and answers writes with
 * `FS_DONE_PERMISSION`. */
export const FEATURE_FS = 1 << 6;

/** `sync_id` reported by a failed `FS_SYNCED`. */
export const FS_SYNC_ID_INVALID = 0xffff;

// C2S_FS_SYNC flags.
export const FS_SYNC_RECURSIVE = 1 << 0;
export const FS_SYNC_CONTENT = 1 << 1;
export const FS_SYNC_CROSS_FILESYSTEM = 1 << 2;
/** The root is a single FILE (docs/design/fs-watch.md "Single-file sync"):
 *  the mirror holds exactly one entry keyed `""`. Mutually exclusive with
 *  `RECURSIVE` — the combination is rejected server-side. */
export const FS_SYNC_SINGLE = 1 << 3;
/** Resolve the sync's base directory from a pty's live cwd: a trailing
 *  `[src_pty_id:2]` names a pty and the server joins `path` onto its cwd
 *  (docs/ide.md Decision 3). It comes last, after any `EXCLUDE` field. */
export const FS_SYNC_FROM_PTY = 1 << 4;
/** Omit every entry whose final component is exactly `.git` — directory or
 *  gitfile — from enumeration, hashing, hints, and records. A pure name
 *  filter: no git data is read (docs/design/fs-watch.md "Ignoring"). */
export const FS_SYNC_EXCLUDE_GIT = 1 << 5;
/** Honor `.gitignore` in and above the root, plus the governing
 *  repository's `$GIT_DIR/info/exclude`, the user's `core.excludesFile`,
 *  and its `core.ignorecase`. */
export const FS_SYNC_GITIGNORE = 1 << 6;
/** A trailing `[exclude_len:2][exclude:M]` carries client patterns —
 *  gitignore syntax, one per line, anchored at the sync root and applied
 *  above every other rule, so `!keep` re-includes. The flag is what makes
 *  the field parseable, and what makes a server too old to filter refuse
 *  the sync instead of silently mirroring the whole tree. */
export const FS_SYNC_EXCLUDE = 1 << 7;
/** Honor `.ignore` in and above the root — ripgrep's convention, which a
 *  project uses to hide things from tooling without telling git to stop
 *  tracking them. Separate from `GITIGNORE` because the two answer
 *  different questions, and `.ignore` brings none of git's
 *  repository-wide sources with it. */
export const FS_SYNC_DOTIGNORE = 1 << 8;
/** Root the sync at this connection's drag staging dir instead of a server
 *  path: the path is ignored (sent empty), the dir is auto-created, and it
 *  lives until the connection closes. Browser drag-and-drop stages files
 *  here so `C2S_SURFACE_DRAG_DROP` can name them without inlining their
 *  bytes. Carries no trailer; invalid with `FS_SYNC_FROM_PTY`. */
export const FS_SYNC_STAGING = 1 << 9;

// S2C_FS_UPDATE flags.
/** Begin a staged snapshot: apply this and subsequent records to an empty
 *  staging map instead of the live map. */
export const FS_UPDATE_RESET = 1 << 0;
/** Atomically replace the live map with the staging map (no-op without one). */
export const FS_UPDATE_SYNC = 1 << 1;

// S2C_FS_SYNCED status.
export const FS_STATUS_OK = 0;
export const FS_STATUS_NOT_FOUND = 1;
export const FS_STATUS_PERMISSION_DENIED = 2;
export const FS_STATUS_RESOURCE_LIMIT = 3;
export const FS_STATUS_OTHER = 4;

// S2C_FS_FILE status.
export const FS_FILE_OK = 0;
export const FS_FILE_NOT_FOUND = 1;
export const FS_FILE_UNREADABLE = 2;
export const FS_FILE_OTHER = 3;

// S2C_FS_CLOSED reasons.
export const FS_CLOSED_CLIENT_REQUEST = 0;
export const FS_CLOSED_ROOT_GONE = 1;
export const FS_CLOSED_PERMISSION_LOST = 2;
export const FS_CLOSED_BACKEND_FAILED = 3;
export const FS_CLOSED_RESOURCE_LIMIT = 4;
/** Client-side pseudo-reason: the connection dropped or was re-established.
 *  Sync state does not survive reconnects — re-`syncFs`. */
export const FS_CLOSED_CONNECTION_LOST = -1;

/** Human-readable `S2C_FS_SYNCED` failure status. */
export function fsStatusText(status: number, detail: string): string {
  const name =
    status === FS_STATUS_NOT_FOUND
      ? "not found"
      : status === FS_STATUS_PERMISSION_DENIED
        ? "permission denied"
        : status === FS_STATUS_RESOURCE_LIMIT
          ? "resource limit"
          : "error";
  return detail.length > 0 ? `${name}: ${detail}` : name;
}

/** Rejection from a refused `FS_SYNC` open, carrying the wire status and
 *  detail so callers can pick a fallback without parsing the message —
 *  e.g. a `single` open refused by a pre-`FS_SYNC_SINGLE` server (any
 *  status other than not-found/permission) falls back to a directory
 *  sync. The message stays `Sync failed: ${fsStatusText(...)}`. */
export class FsOpenError extends Error {
  readonly status: number;
  readonly detail: string;
  constructor(status: number, detail: string) {
    super(`Sync failed: ${fsStatusText(status, detail)}`);
    this.name = "FsOpenError";
    this.status = status;
    this.detail = detail;
  }
}

/** Human-readable `S2C_FS_FILE` failure status. */
export function fsFileStatusText(status: number): string {
  return status === FS_FILE_NOT_FOUND
    ? "not found"
    : status === FS_FILE_UNREADABLE
      ? "unreadable"
      : "error";
}

// Record kinds inside FS_UPDATE.
export const FS_RECORD_UPSERT = 0x01;
export const FS_RECORD_DELETE = 0x02;
export const FS_RECORD_MOVE = 0x03;

// UPSERT entry_flags: bits 0-1 node type, higher bits flags.
export const FS_ENTRY_TYPE_MASK = 0b11;
export const FS_ENTRY_FILE = 0;
export const FS_ENTRY_DIR = 1;
export const FS_ENTRY_SYMLINK = 2;
export const FS_ENTRY_OTHER = 3;
/** Entry exists but its content could not be read. */
export const FS_ENTRY_UNREADABLE = 1 << 2;
/** Content omitted: over `inline_max` or the sync did not request content. */
export const FS_ENTRY_NO_CONTENT = 1 << 3;
/** File changed repeatedly while being read; content omitted, another
 *  upsert follows once it settles. */
export const FS_ENTRY_UNSTABLE = 1 << 4;
/** Set on an `FS_ENTRY_SYMLINK` whose target is a directory, which the sync
 *  enumerates like any other. The type alone cannot distinguish a link to a
 *  directory from one to a file, so this is what tells a tree the entry is
 *  expandable. */
export const FS_ENTRY_LINK_DIR = 1 << 5;
/** Set on a directory whose enumeration skipped at least one child the
 *  sync's exclusion rules cover. Excluded paths are absent rather than
 *  marked, so without this a client cannot tell an empty directory from a
 *  filtered one — a file tree needs it to say "some items hidden".
 *
 *  Prompt when it goes up, lazy when it comes down: the first excluded
 *  child costs one re-listing of its directory, while the last one
 *  disappearing clears the flag only at that directory's next
 *  enumeration. So a tree may briefly show "hidden items" on a directory
 *  that no longer has any. */
export const FS_ENTRY_FILTERED = 1 << 6;

// UPSERT content kinds.
export const FS_CONTENT_NONE = 0;
export const FS_CONTENT_FULL = 1;
export const FS_CONTENT_DELTA = 2;

// -- Message builders (client to server) ------------------------------------

/** Fixed part of `C2S_FS_SYNC`, up to and including `path_len`. */
export const FS_SYNC_HEADER = 13;

export function buildFsSyncMessage(
  nonce: number,
  flags: number,
  latencyMs: number,
  inlineMax: number,
  path: string,
  srcPtyId?: number,
  /** Gitignore-syntax patterns, one per line. Empty omits the field. */
  exclude?: string,
): Uint8Array {
  const pathBytes = textEncoder.encode(path);
  const excludeBytes = textEncoder.encode(exclude ?? "");
  const hasExclude = excludeBytes.length > 0;
  const hasSrc = srcPtyId != null;
  const excludeLen = hasExclude ? 2 + excludeBytes.length : 0;
  const msg = new Uint8Array(
    FS_SYNC_HEADER + pathBytes.length + excludeLen + (hasSrc ? 2 : 0),
  );
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_SYNC;
  v.setUint16(1, nonce, true);
  v.setUint16(
    3,
    (hasSrc ? flags | FS_SYNC_FROM_PTY : flags) |
      (hasExclude ? FS_SYNC_EXCLUDE : 0),
    true,
  );
  v.setUint16(5, latencyMs, true);
  v.setUint32(7, inlineMax, true);
  v.setUint16(11, pathBytes.length, true);
  msg.set(pathBytes, FS_SYNC_HEADER);
  let off = FS_SYNC_HEADER + pathBytes.length;
  if (hasExclude) {
    v.setUint16(off, excludeBytes.length, true);
    msg.set(excludeBytes, off + 2);
    off += excludeLen;
  }
  if (hasSrc) v.setUint16(off, srcPtyId, true);
  return msg;
}

export function buildFsStopMessage(syncId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_FS_STOP;
  msg[1] = syncId & 0xff;
  msg[2] = (syncId >> 8) & 0xff;
  return msg;
}

export function buildFsAckMessage(
  syncId: number,
  updateId: number,
): Uint8Array {
  const msg = new Uint8Array(7);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_ACK;
  v.setUint16(1, syncId, true);
  v.setUint32(3, updateId, true);
  return msg;
}

export function buildFsFetchMessage(
  nonce: number,
  syncId: number,
  path: string,
): Uint8Array {
  const pathBytes = textEncoder.encode(path);
  const msg = new Uint8Array(7 + pathBytes.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_FETCH;
  v.setUint16(1, nonce, true);
  v.setUint16(3, syncId, true);
  v.setUint16(5, pathBytes.length, true);
  msg.set(pathBytes, 7);
  return msg;
}

// -- File search (no sync) --------------------------------------------------

export const C2S_FS_SEARCH = 0x46;
export const S2C_FS_SEARCH = 0x45;

/** [0x46][nonce:2][limit:2][root_len:2][root:N][query_len:2][query:N] */
export function buildFsSearchMessage(
  nonce: number,
  limit: number,
  root: string,
  query: string,
): Uint8Array {
  const rb = textEncoder.encode(root);
  const qb = textEncoder.encode(query);
  const msg = new Uint8Array(9 + rb.length + qb.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_SEARCH;
  v.setUint16(1, nonce, true);
  v.setUint16(3, limit, true);
  v.setUint16(5, rb.length, true);
  msg.set(rb, 7);
  v.setUint16(7 + rb.length, qb.length, true);
  msg.set(qb, 9 + rb.length);
  return msg;
}

/** [0x45][nonce:2][status:1][count:2] repeated{ [path_len:2][path:N] } */
export function parseFsSearchResult(
  data: Uint8Array,
): { nonce: number; status: number; paths: string[] } | null {
  if (data.length < 6 || data[0] !== S2C_FS_SEARCH) return null;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const nonce = v.getUint16(1, true);
  const status = data[3];
  const count = v.getUint16(4, true);
  const paths: string[] = [];
  let off = 6;
  for (let i = 0; i < count; i++) {
    if (off + 2 > data.length) return null;
    const pl = v.getUint16(off, true);
    off += 2;
    if (off + pl > data.length) return null;
    paths.push(textDecoder.decode(data.subarray(off, off + pl)));
    off += pl;
  }
  return { nonce, status, paths };
}

// -- File index (no sync, docs/design/fs-search.md) -------------------------

export const C2S_FS_INDEX = 0x47;
export const S2C_FS_INDEX = 0x46;

/** The walk hit a budget; the list is a prefix of the tree, so callers
 *  should keep server-side search for this root. */
export const FS_INDEX_TRUNCATED = 1 << 0;

/** Protocol cap on `count` — a larger claim is malformed. Without it, a
 *  hostile count of tiny records forces millions of decode calls from a
 *  small frame (the decompression guard bounds bytes, not record counts). */
export const FS_INDEX_MAX_COUNT = 1_000_000;

/** A fetched candidate list: root-relative paths, sorted,
 *  gitignore-filtered server-side. */
export type FsFileIndex = { paths: string[]; truncated: boolean };

/** [0x47][nonce:2][flags:1][root_len:2][root:N] — flags reserved (0). */
export function buildFsIndexMessage(nonce: number, root: string): Uint8Array {
  const rb = textEncoder.encode(root);
  const msg = new Uint8Array(6 + rb.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_INDEX;
  v.setUint16(1, nonce, true);
  msg[3] = 0;
  v.setUint16(4, rb.length, true);
  msg.set(rb, 6);
  return msg;
}

/** [0x46][nonce:2][status:1][flags:1][count:4][paths:LZ4] where the
 *  decompressed payload is repeated{ [path_len:2][path:N] }. Applies the
 *  standard decompression guard; null = malformed, over-sized, or a
 *  payload that disagrees with `count`. */
export function parseFsIndexResult(
  data: Uint8Array,
): { nonce: number; status: number; flags: number; paths: string[] } | null {
  if (data.length < 9 || data[0] !== S2C_FS_INDEX) return null;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const nonce = v.getUint16(1, true);
  const status = data[3];
  const flags = data[4];
  const count = v.getUint32(5, true);
  if (count > FS_INDEX_MAX_COUNT) return null;
  const raw = fsDecompress(data.subarray(9));
  if (!raw) return null;
  const rv = new DataView(raw.buffer, raw.byteOffset, raw.byteLength);
  const paths: string[] = [];
  let off = 0;
  while (off < raw.length) {
    if (off + 2 > raw.length) return null;
    const pl = rv.getUint16(off, true);
    off += 2;
    if (off + pl > raw.length) return null;
    paths.push(textDecoder.decode(raw.subarray(off, off + pl)));
    off += pl;
  }
  if (paths.length !== count) return null;
  return { nonce, status, flags, paths };
}

// -- Content search (no sync, docs/design/fs-grep.md) -----------------------

export const C2S_FS_GREP = 0x48;
export const S2C_FS_GREP = 0x47;

// C2S_FS_GREP flags.

/** Match case exactly. Unset (the default) is case-insensitive. */
export const FS_GREP_CASE_SENSITIVE = 1 << 0;
/** `query` is a regex. Unset (the default) treats it as a literal string. */
export const FS_GREP_REGEX = 1 << 1;
/** Search gitignored files too, ranked after every tracked one. Unset (the
 *  default) skips them — on a real repo that is the difference between
 *  milliseconds and seconds. */
export const FS_GREP_NO_IGNORE = 1 << 2;
/** Match only whole words — the pattern is wrapped in `\b(?:…)\b` after
 *  literal escaping, so it composes with either mode. */
export const FS_GREP_WORD = 1 << 3;

// S2C_FS_GREP flags.

/** A budget clipped the search: matches exist that are not in this
 *  response. Exact — set only when something was actually dropped. */
export const FS_GREP_TRUNCATED = 1 << 0;

// S2C_FS_GREP record kinds.

export const FS_GREP_RECORD_FILE = 0x01;
export const FS_GREP_RECORD_MATCH = 0x02;

// FILE record flags.

/** The file is gitignored. It is still searched — ignore rules rank rather
 *  than filter here — but sorts after every non-ignored file. */
export const FS_GREP_FILE_IGNORED = 1 << 0;

export type FsGrepRecord =
  /** FILE 0x01: [kind:1][flags:1][n:2][path_len:2][path:N] — the next `n`
   *  match records belong to this file. */
  | { kind: "file"; flags: number; n: number; path: string }
  /** MATCH 0x02: [kind:1][line:4][col:4][end_line:4][end_col:4][text_len:4][text:N].
   *  0-based lines, UTF-8 byte columns — an LSP-shaped range. `endLine`
   *  differs from `line` when the pattern matched across a newline, and
   *  `text` then holds every line the match spans, joined by `\n`. */
  | {
      kind: "match";
      line: number;
      col: number;
      endLine: number;
      endCol: number;
      text: string;
    };

/** One file's hits, as the UI consumes them. */
export interface FsGrepFile {
  /** Root-relative path. */
  path: string;
  /** Gitignored — ranked last, and a client may dim it. */
  ignored: boolean;
  matches: {
    line: number;
    col: number;
    endLine: number;
    endCol: number;
    text: string;
  }[];
}

export interface FsGrepResult {
  files: FsGrepFile[];
  /** A budget clipped the search. */
  truncated: boolean;
}

export interface FsGrepOptions {
  /** Match case exactly; default is case-insensitive. */
  caseSensitive?: boolean;
  /** Treat the query as a regex; default is a literal string. */
  regex?: boolean;
  /** Include gitignored files, ranked last. Default respects ignore rules,
   *  which is what keeps a search of a repo with build output fast. */
  noIgnore?: boolean;
  /** Match whole words only. */
  word?: boolean;
  /** Cap on total matches; 0/omitted means the server default. */
  maxMatches?: number;
  /** Cap on matches from any one file; 0/omitted means the server default. */
  maxPerFile?: number;
}

/** Build a `C2S_FS_GREP`:
 *  [0x48][nonce:2][flags:1][max_matches:2][max_per_file:2][root_len:2][root:N][query_len:2][query:N] */
export function buildFsGrepMessage(
  nonce: number,
  root: string,
  query: string,
  opts: FsGrepOptions = {},
): Uint8Array {
  const rb = textEncoder.encode(root);
  const qb = textEncoder.encode(query);
  const flags =
    (opts.caseSensitive ? FS_GREP_CASE_SENSITIVE : 0) |
    (opts.regex ? FS_GREP_REGEX : 0) |
    (opts.noIgnore ? FS_GREP_NO_IGNORE : 0) |
    (opts.word ? FS_GREP_WORD : 0);
  const msg = new Uint8Array(12 + rb.length + qb.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_GREP;
  v.setUint16(1, nonce, true);
  msg[3] = flags;
  v.setUint16(4, Math.min(opts.maxMatches ?? 0, 0xffff), true);
  v.setUint16(6, Math.min(opts.maxPerFile ?? 0, 0xffff), true);
  v.setUint16(8, rb.length, true);
  msg.set(rb, 10);
  v.setUint16(10 + rb.length, qb.length, true);
  msg.set(qb, 12 + rb.length);
  return msg;
}

/**
 * Decode an uncompressed `FS_GREP` records payload. Unknown kinds are
 * skipped via `record_len`; a record whose body overruns ends iteration,
 * matching the Rust codec.
 */
export function* fsGrepRecords(data: Uint8Array): Generator<FsGrepRecord> {
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  let offset = 0;
  while (offset + 4 <= data.length) {
    const recLen = view.getUint32(offset, true);
    if (recLen === 0 || offset + 4 + recLen > data.length) return;
    const body = offset + 4;
    const end = body + recLen;
    offset = end;
    const kind = data[body];
    if (kind === FS_GREP_RECORD_FILE) {
      if (end - body < 6) return;
      const pl = view.getUint16(body + 4, true);
      if (body + 6 + pl > end) return;
      yield {
        kind: "file",
        flags: data[body + 1],
        n: view.getUint16(body + 2, true),
        path: textDecoder.decode(data.subarray(body + 6, body + 6 + pl)),
      };
    } else if (kind === FS_GREP_RECORD_MATCH) {
      if (end - body < 21) return;
      const tl = view.getUint32(body + 17, true);
      if (body + 21 + tl > end) return;
      yield {
        kind: "match",
        line: view.getUint32(body + 1, true),
        col: view.getUint32(body + 5, true),
        endLine: view.getUint32(body + 9, true),
        endCol: view.getUint32(body + 13, true),
        text: textDecoder.decode(data.subarray(body + 21, body + 21 + tl)),
      };
    }
    // Unknown kind: skipped via record_len.
  }
}

/** Parse an `S2C_FS_GREP`:
 *  [0x47][nonce:2][status:1][flags:1][detail_len:2][detail:N][records:LZ4].
 *  Applies the standard decompression guard; null = malformed. */
export function parseFsGrepResult(data: Uint8Array): {
  nonce: number;
  status: number;
  flags: number;
  detail: string;
  files: FsGrepFile[];
} | null {
  if (data.length < 7 || data[0] !== S2C_FS_GREP) return null;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const nonce = v.getUint16(1, true);
  const status = data[3];
  const flags = data[4];
  const dl = v.getUint16(5, true);
  if (7 + dl > data.length) return null;
  let detail: string;
  try {
    detail = textDecoder.decode(data.subarray(7, 7 + dl));
  } catch {
    return null;
  }
  const raw = fsDecompress(data.subarray(7 + dl));
  if (!raw) return null;
  // Match records attach to the most recent file record, as in LSP_DIAG.
  const files: FsGrepFile[] = [];
  let current: FsGrepFile | null = null;
  try {
    for (const rec of fsGrepRecords(raw)) {
      if (rec.kind === "file") {
        current = {
          path: rec.path,
          ignored: (rec.flags & FS_GREP_FILE_IGNORED) !== 0,
          matches: [],
        };
        files.push(current);
      } else if (current) {
        current.matches.push({
          line: rec.line,
          col: rec.col,
          endLine: rec.endLine,
          endCol: rec.endCol,
          text: rec.text,
        });
      }
    }
  } catch {
    return null; // invalid UTF-8 in a record poisons the payload
  }
  return { nonce, status, flags, detail, files };
}

// -- Write family (docs/design/fs-write.md) ---------------------------------

export const C2S_FS_WRITE = 0x44;
export const C2S_FS_OP = 0x45;
export const S2C_FS_DONE = 0x44;

// FS_DONE status — the unified git/lsp table plus CONFLICT.
export const FS_DONE_OK = 0;
export const FS_DONE_NOT_FOUND = 2;
export const FS_DONE_WRONG_TYPE = 3;
export const FS_DONE_PERMISSION = 4;
export const FS_DONE_TOO_LARGE = 5;
export const FS_DONE_BUDGET = 6;
export const FS_DONE_INVALID = 7;
export const FS_DONE_OTHER = 9;
/** A precondition failed; `FsDone.hash` carries the current on-disk hash. */
export const FS_DONE_CONFLICT = 11;
/* Family-local statuses live at 128+ per docs/protocol.md's common status
 * registry (0–127 is the centralized range); keep in sync with
 * crates/remote/src/fs.rs. */
/** Chunked upload: the chunk's offset is not the server's resume point;
 * the reply's `received` field names where to resend from. */
export const FS_DONE_OFFSET_MISMATCH = 128;
/** Chunked upload: the assembled size does not match the declared size. */
export const FS_DONE_SIZE_MISMATCH = 129;
/** Chunked upload: the `upload_id` is unknown (never began, finished, or
 * cancelled already). */
export const FS_DONE_UNKNOWN_UPLOAD = 130;

/** Human-readable `FS_DONE` status. */
export function fsDoneStatusText(status: number): string {
  switch (status) {
    case FS_DONE_OK:
      return "ok";
    case FS_DONE_NOT_FOUND:
      return "not found";
    case FS_DONE_WRONG_TYPE:
      return "wrong type";
    case FS_DONE_PERMISSION:
      return "permission denied";
    case FS_DONE_TOO_LARGE:
      return "too large";
    case FS_DONE_BUDGET:
      return "budget exhausted";
    case FS_DONE_INVALID:
      return "invalid request";
    case FS_DONE_CONFLICT:
      return "conflict";
    case FS_DONE_OFFSET_MISMATCH:
      return "offset mismatch";
    case FS_DONE_SIZE_MISMATCH:
      return "size mismatch";
    case FS_DONE_UNKNOWN_UPLOAD:
      return "unknown upload";
    default:
      return "error";
  }
}

// FS_WRITE flags.
export const FS_WRITE_NO_CAS = 1 << 0;
export const FS_WRITE_MKPARENTS = 1 << 1;
export const FS_WRITE_DURABLE = 1 << 2;
export const FS_WRITE_FOLLOW_SYMLINK = 1 << 3;
export const FS_WRITE_CONTENT_FULL = 1;
export const FS_WRITE_CONTENT_DELTA = 2;

// FS_OP op selector + flags.
export const FS_OP_MKDIR = 1;
export const FS_OP_REMOVE = 2;
export const FS_OP_RENAME = 3;
/** Create or retarget a symlink at `b` targeting the verbatim string `a`;
 * a symlink's content hash is BLAKE3-128 of its target bytes. */
export const FS_OP_SYMLINK = 4;
/** Create a hard link at `b` to the regular file at `a`. */
export const FS_OP_HARDLINK = 5;
export const FS_OP_NO_CAS = 1 << 0;
export const FS_OP_MKPARENTS = 1 << 1;

const U64_MASK = 0xffffffffffffffffn;

/** Write a 128-bit value as two little-endian u64 (low word first). */
function setU128(v: DataView, off: number, value: bigint): void {
  v.setBigUint64(off, value & U64_MASK, true);
  v.setBigUint64(off + 8, (value >> 64n) & U64_MASK, true);
}

function getU128(v: DataView, off: number): bigint {
  return v.getBigUint64(off, true) | (v.getBigUint64(off + 8, true) << 64n);
}

export interface FsWriteArgs {
  nonce: number;
  syncId: number;
  flags: number;
  /** CAS precondition hash (0n = create-exclusive; ignored under NO_CAS). */
  base: bigint;
  mode: number;
  contentKind: number;
  path: string;
  content: Uint8Array;
}

export function buildFsWriteMessage(a: FsWriteArgs): Uint8Array {
  const pathBytes = textEncoder.encode(a.path);
  const compressed = fsCompress(a.content);
  const msg = new Uint8Array(29 + pathBytes.length + compressed.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_WRITE;
  v.setUint16(1, a.nonce, true);
  v.setUint16(3, a.syncId, true);
  msg[5] = a.flags;
  setU128(v, 6, a.base);
  v.setUint32(22, a.mode, true);
  msg[26] = a.contentKind;
  v.setUint16(27, pathBytes.length, true);
  msg.set(pathBytes, 29);
  msg.set(compressed, 29 + pathBytes.length);
  return msg;
}

export interface FsOpArgs {
  nonce: number;
  syncId: number;
  op: number;
  flags: number;
  base: bigint;
  mode: number;
  a: string;
  b: string;
}

export function buildFsOpMessage(o: FsOpArgs): Uint8Array {
  const ab = textEncoder.encode(o.a);
  const bb = textEncoder.encode(o.b);
  // Fixed part is 31 bytes: opcode + nonce + sync + op + flags + base(16) +
  // mode(4) + a_len(2) + b_len(2).
  const msg = new Uint8Array(31 + ab.length + bb.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_OP;
  v.setUint16(1, o.nonce, true);
  v.setUint16(3, o.syncId, true);
  msg[5] = o.op;
  msg[6] = o.flags;
  setU128(v, 7, o.base);
  v.setUint32(23, o.mode, true);
  v.setUint16(27, ab.length, true);
  msg.set(ab, 29);
  const bLenOff = 29 + ab.length;
  v.setUint16(bLenOff, bb.length, true);
  msg.set(bb, bLenOff + 2);
  return msg;
}

export interface FsDone {
  nonce: number;
  status: number;
  /** Post-op content hash on success; current on-disk hash on CONFLICT. */
  hash: bigint;
  mtimeNs: bigint;
}

/** Parse an `S2C_FS_DONE`; null = malformed or wrong opcode. */
export function parseFsDoneMessage(msg: Uint8Array): FsDone | null {
  if (msg.length < 28 || msg[0] !== S2C_FS_DONE) {
    return null;
  }
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  return {
    nonce: v.getUint16(1, true),
    status: msg[3],
    hash: getU128(v, 4),
    mtimeNs: v.getBigUint64(20, true),
  };
}

/** Build an `FS_DONE` (tests and mock servers). */
export function buildFsDoneMessage(
  nonce: number,
  status: number,
  hash: bigint,
  mtimeNs: bigint,
): Uint8Array {
  const msg = new Uint8Array(28);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_FS_DONE;
  v.setUint16(1, nonce, true);
  msg[3] = status;
  setU128(v, 4, hash);
  v.setBigUint64(20, mtimeNs, true);
  return msg;
}

// -- Upload family (chunked writes) ------------------------------------------
//
// Large files that would not fit one `FS_WRITE` frame go up as a BEGIN, an
// ordered run of CHUNKs (each individually LZ4-framed like `FS_WRITE`
// content), and a FINISH; CANCEL abandons. Chunks are pipelined: each is
// acked with the cumulative plaintext bytes accepted, and a mismatch asks
// the client to resend from the server's resume point. Same feature bit as
// the rest of the FS family (`FEATURE_FS`).

/** Begin a chunked upload: [0x49][nonce:2][sync_id:2][flags:1][mode:4][size:8][path_len:2][path:N] */
export const C2S_FS_UPLOAD_BEGIN = 0x49;
/** One chunk: [0x4a][upload_id:2][offset:8][data:LZ4] */
export const C2S_FS_UPLOAD_CHUNK = 0x4a;
/** Commit an upload: [0x4b][nonce:2][upload_id:2] */
export const C2S_FS_UPLOAD_FINISH = 0x4b;
/** Abandon an upload (no reply): [0x4c][upload_id:2] */
export const C2S_FS_UPLOAD_CANCEL = 0x4c;

/** Begin accepted or rejected: [0x49][nonce:2][status:1][upload_id:2] */
export const S2C_FS_UPLOAD_BEGIN = 0x49;
/** Chunk ack: [0x4a][upload_id:2][status:1][received:8] */
export const S2C_FS_UPLOAD_CHUNK = 0x4a;
/** Commit result: [0x4b][nonce:2][status:1][hash:16][mtime_ns:8] — the
 *  `FS_DONE` payload shape on success. */
export const S2C_FS_UPLOAD_FINISH = 0x4b;

// C2S_FS_UPLOAD_BEGIN flags — deliberately the same bit values as the
// FS_WRITE flags above, the semantics are identical.
export const FS_UPLOAD_NO_CAS = FS_WRITE_NO_CAS;
export const FS_UPLOAD_MKPARENTS = FS_WRITE_MKPARENTS;
export const FS_UPLOAD_DURABLE = FS_WRITE_DURABLE;
export const FS_UPLOAD_FOLLOW_SYMLINK = FS_WRITE_FOLLOW_SYMLINK;

export interface FsUploadBeginArgs {
  nonce: number;
  syncId: number;
  flags: number;
  /** Precondition, exactly as FS_WRITE's base: ignored under NO_CAS;
   *  0 without NO_CAS = create-exclusive; otherwise CAS against the
   *  current content hash. Checked at BEGIN (fail fast) and re-verified
   *  at FINISH before the rename. */
  base: bigint;
  mode: number;
  /** Total plaintext bytes to be uploaded. */
  size: number;
  path: string;
}

export function buildFsUploadBeginMessage(a: FsUploadBeginArgs): Uint8Array {
  const pathBytes = textEncoder.encode(a.path);
  const msg = new Uint8Array(36 + pathBytes.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_UPLOAD_BEGIN;
  v.setUint16(1, a.nonce, true);
  v.setUint16(3, a.syncId, true);
  msg[5] = a.flags;
  setU128(v, 6, a.base);
  v.setUint32(22, a.mode, true);
  v.setBigUint64(26, BigInt(a.size), true);
  v.setUint16(34, pathBytes.length, true);
  msg.set(pathBytes, 36);
  return msg;
}

/** Build a `C2S_FS_UPLOAD_CHUNK`; `data` is the plaintext chunk, compressed
 *  with the same lz4-prepend-size framing as `FS_WRITE` content. */
export function buildFsUploadChunkMessage(
  uploadId: number,
  offset: number,
  data: Uint8Array,
): Uint8Array {
  const compressed = fsCompress(data);
  const msg = new Uint8Array(11 + compressed.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_UPLOAD_CHUNK;
  v.setUint16(1, uploadId, true);
  v.setBigUint64(3, BigInt(offset), true);
  msg.set(compressed, 11);
  return msg;
}

export function buildFsUploadFinishMessage(
  nonce: number,
  uploadId: number,
): Uint8Array {
  const msg = new Uint8Array(5);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_UPLOAD_FINISH;
  v.setUint16(1, nonce, true);
  v.setUint16(3, uploadId, true);
  return msg;
}

export function buildFsUploadCancelMessage(uploadId: number): Uint8Array {
  const msg = new Uint8Array(3);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_UPLOAD_CANCEL;
  v.setUint16(1, uploadId, true);
  return msg;
}

export interface FsUploadBeginReply {
  nonce: number;
  status: number;
  uploadId: number;
  /** Current on-disk content hash when `status` is CONFLICT, 0 otherwise
   *  (same convention as `FsDone.hash`). */
  hash: bigint;
  mtimeNs: bigint;
}

/** Parse an `S2C_FS_UPLOAD_BEGIN`; null = malformed or wrong opcode. */
export function parseFsUploadBeginReply(
  msg: Uint8Array,
): FsUploadBeginReply | null {
  if (msg.length < 30 || msg[0] !== S2C_FS_UPLOAD_BEGIN) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  return {
    nonce: v.getUint16(1, true),
    status: msg[3],
    uploadId: v.getUint16(4, true),
    hash: getU128(v, 6),
    mtimeNs: v.getBigUint64(22, true),
  };
}

export interface FsUploadChunkAck {
  uploadId: number;
  status: number;
  /** Cumulative plaintext bytes accepted; on `FS_DONE_OFFSET_MISMATCH`,
   * the resume point to resend from. */
  received: number;
}

/** Parse an `S2C_FS_UPLOAD_CHUNK` ack; null = malformed or wrong opcode. */
export function parseFsUploadChunkAck(
  msg: Uint8Array,
): FsUploadChunkAck | null {
  if (msg.length < 12 || msg[0] !== S2C_FS_UPLOAD_CHUNK) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  return {
    uploadId: v.getUint16(1, true),
    status: msg[3],
    received: Number(v.getBigUint64(4, true)),
  };
}

export interface FsUploadFinishReply {
  nonce: number;
  status: number;
  /** Post-write content hash on success (same slot as `FsDone.hash`). */
  hash: bigint;
  /** The hash's raw 16 wire bytes (little-endian u128), for callers that
   *  want bytes rather than a bigint. */
  hashBytes: Uint8Array;
  mtimeNs: bigint;
}

/** Parse an `S2C_FS_UPLOAD_FINISH`; null = malformed or wrong opcode. */
export function parseFsUploadFinishReply(
  msg: Uint8Array,
): FsUploadFinishReply | null {
  if (msg.length < 28 || msg[0] !== S2C_FS_UPLOAD_FINISH) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  return {
    nonce: v.getUint16(1, true),
    status: msg[3],
    hash: getU128(v, 4),
    hashBytes: msg.slice(4, 20),
    mtimeNs: v.getBigUint64(20, true),
  };
}

/** Build an `S2C_FS_UPLOAD_BEGIN` (tests and mock servers). */
export function buildFsUploadBeginReply(
  nonce: number,
  status: number,
  uploadId: number,
  hash = 0n,
  mtimeNs = 0n,
): Uint8Array {
  const msg = new Uint8Array(30);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_FS_UPLOAD_BEGIN;
  v.setUint16(1, nonce, true);
  msg[3] = status;
  v.setUint16(4, uploadId, true);
  setU128(v, 6, hash);
  v.setBigUint64(22, mtimeNs, true);
  return msg;
}

/** Build an `S2C_FS_UPLOAD_CHUNK` ack (tests and mock servers). */
export function buildFsUploadChunkAck(
  uploadId: number,
  status: number,
  received: number,
): Uint8Array {
  const msg = new Uint8Array(12);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_FS_UPLOAD_CHUNK;
  v.setUint16(1, uploadId, true);
  msg[3] = status;
  v.setBigUint64(4, BigInt(received), true);
  return msg;
}

/** Build an `S2C_FS_UPLOAD_FINISH` (tests and mock servers). */
export function buildFsUploadFinishReply(
  nonce: number,
  status: number,
  hash: bigint,
  mtimeNs: bigint,
): Uint8Array {
  const msg = new Uint8Array(28);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_FS_UPLOAD_FINISH;
  v.setUint16(1, nonce, true);
  msg[3] = status;
  setU128(v, 4, hash);
  v.setBigUint64(20, mtimeNs, true);
  return msg;
}

// -- LZ4 --------------------------------------------------------------------

/**
 * Cap on any single LZ4-decompressed fs payload, mirroring the Rust guard:
 * the declared size is checked *before* allocating, so a hostile or corrupt
 * length cannot force a giant allocation. Large trees arrive as many
 * bounded updates, never one huge one.
 */
export const FS_MAX_DECOMPRESSED = 64 * 1024 * 1024;

/**
 * Decompress an lz4_flex `compress_prepend_size` payload
 * (`[uncompressed_len:4][lz4 block]`), refusing declared sizes over
 * {@link FS_MAX_DECOMPRESSED}. Returns null on any malformation.
 */
export function fsDecompress(data: Uint8Array): Uint8Array | null {
  if (data.length < 4) return null;
  const declared =
    (data[0] | (data[1] << 8) | (data[2] << 16) | (data[3] << 24)) >>> 0;
  if (declared > FS_MAX_DECOMPRESSED) return null;
  return lz4DecompressBlock(data.subarray(4), declared);
}

/** Decode one raw LZ4 block into exactly `outLen` bytes, or null. */
function lz4DecompressBlock(
  src: Uint8Array,
  outLen: number,
): Uint8Array | null {
  const out = new Uint8Array(outLen);
  let si = 0;
  let di = 0;
  if (outLen === 0) return src.length === 0 || src.length === 1 ? out : null;
  while (si < src.length) {
    const token = src[si++];
    let litLen = token >> 4;
    if (litLen === 15) {
      let b: number;
      do {
        if (si >= src.length) return null;
        b = src[si++];
        litLen += b;
      } while (b === 255);
    }
    if (si + litLen > src.length || di + litLen > outLen) return null;
    out.set(src.subarray(si, si + litLen), di);
    si += litLen;
    di += litLen;
    if (si >= src.length) break; // final sequence carries no match
    if (si + 2 > src.length) return null;
    const offset = src[si] | (src[si + 1] << 8);
    si += 2;
    if (offset === 0 || offset > di) return null;
    let matchLen = (token & 0x0f) + 4;
    if ((token & 0x0f) === 15) {
      let b: number;
      do {
        if (si >= src.length) return null;
        b = src[si++];
        matchLen += b;
      } while (b === 255);
    }
    if (di + matchLen > outLen) return null;
    let mi = di - offset;
    if (offset >= matchLen) {
      out.copyWithin(di, mi, mi + matchLen);
      di += matchLen;
    } else {
      // Overlapping copies are the point of LZ4 — byte-by-byte is required.
      for (let i = 0; i < matchLen; i++) out[di++] = out[mi++];
    }
  }
  return di === outLen ? out : null;
}

/**
 * Compress with a literal-only LZ4 block (always valid, never smaller than
 * the input) in `compress_prepend_size` framing. Enough to build
 * `FS_UPDATE`/`FS_FILE` messages in tests and mock servers; real servers
 * use a full encoder.
 */
export function fsCompressLiteral(data: Uint8Array): Uint8Array {
  const header: number[] = [
    data.length & 0xff,
    (data.length >> 8) & 0xff,
    (data.length >> 16) & 0xff,
    (data.length >> 24) & 0xff,
  ];
  if (data.length === 0) {
    // An LZ4 block is never empty: even zero output needs one 0x00 token
    // (lz4_flex's own empty encoding; without it decompression fails with
    // ExpectedAnotherByte and the whole message is dropped as malformed).
    return new Uint8Array([...header, 0]);
  }
  let rest = data.length;
  if (rest < 15) {
    header.push(rest << 4);
  } else {
    header.push(15 << 4);
    rest -= 15;
    while (rest >= 255) {
      header.push(255);
      rest -= 255;
    }
    header.push(rest);
  }
  const out = new Uint8Array(header.length + data.length);
  out.set(header, 0);
  out.set(data, header.length);
  return out;
}

/**
 * Compress into a standard LZ4 block (greedy hash-table matcher) in
 * `compress_prepend_size` framing — the C2S counterpart of
 * {@link fsDecompress}, also decodable by the server's lz4_flex. Honors
 * the block end rules (the last 5 bytes stay literals, no match starts
 * within the last 12), so tiny inputs and inputs that never match fall
 * back to the literal-only encoding.
 */
export function fsCompress(data: Uint8Array): Uint8Array {
  const n = data.length;
  if (n < 13) return fsCompressLiteral(data);
  // Worst case is one literal-only run: token + length extensions + data.
  const out = new Uint8Array(5 + n + Math.ceil(n / 255) + 16);
  out[0] = n & 0xff;
  out[1] = (n >> 8) & 0xff;
  out[2] = (n >> 16) & 0xff;
  out[3] = (n >> 24) & 0xff;
  let o = 4;
  const hashShift = n < 1 << 16 ? 20 : 16; // 4K entries small, 64K large
  const table = new Int32Array(1 << (32 - hashShift)).fill(-1);
  const read32 = (i: number): number =>
    data[i] | (data[i + 1] << 8) | (data[i + 2] << 16) | (data[i + 3] << 24);
  const matchStartLimit = n - 12;
  const matchEndLimit = n - 5;
  let anchor = 0;
  let si = 0;
  while (si < matchStartLimit) {
    const h = Math.imul(read32(si), 2654435761) >>> hashShift;
    const ref = table[h];
    table[h] = si;
    if (ref < 0 || si - ref > 0xffff || read32(ref) !== read32(si)) {
      si++;
      continue;
    }
    let matchLen = 4;
    while (
      si + matchLen < matchEndLimit &&
      data[ref + matchLen] === data[si + matchLen]
    ) {
      matchLen++;
    }
    // Sequence: token, literal length extension, literals, offset,
    // match length extension.
    const litLen = si - anchor;
    const tokenAt = o++;
    if (litLen >= 15) {
      let rest = litLen - 15;
      for (; rest >= 255; rest -= 255) out[o++] = 255;
      out[o++] = rest;
    }
    out.set(data.subarray(anchor, si), o);
    o += litLen;
    const offset = si - ref;
    out[o++] = offset & 0xff;
    out[o++] = (offset >> 8) & 0xff;
    if (matchLen - 4 >= 15) {
      let rest = matchLen - 4 - 15;
      for (; rest >= 255; rest -= 255) out[o++] = 255;
      out[o++] = rest;
    }
    out[tokenAt] =
      ((litLen < 15 ? litLen : 15) << 4) |
      (matchLen - 4 < 15 ? matchLen - 4 : 15);
    si += matchLen;
    anchor = si;
  }
  // Final sequence: literals only.
  const litLen = n - anchor;
  const tokenAt = o++;
  if (litLen >= 15) {
    let rest = litLen - 15;
    for (; rest >= 255; rest -= 255) out[o++] = 255;
    out[o++] = rest;
  }
  out.set(data.subarray(anchor), o);
  o += litLen;
  out[tokenAt] = (litLen < 15 ? litLen : 15) << 4;
  // Incompressible input: the literal-only encoding is never larger.
  const literalOnly = 5 + n + (n < 15 ? 0 : Math.floor((n - 15) / 255) + 1);
  if (o >= literalOnly) return fsCompressLiteral(data);
  return out.subarray(0, o);
}

// -- Records ----------------------------------------------------------------

/** One decoded record from an `FS_UPDATE` payload. */
export type FsRecord =
  | {
      kind: "upsert";
      path: string;
      entryFlags: number;
      size: number;
      /** Nanoseconds since the epoch; exceeds 2^53, hence bigint. */
      mtimeNs: bigint;
      mode: number;
      /** BLAKE3 truncated to 128 bits; 0n for non-files or unknown. */
      hash: bigint;
      content: FsContent;
    }
  /** Remove `path` and every path under it. */
  | { kind: "delete"; path: string }
  /** Rename the `from` subtree to `to`. */
  | { kind: "move"; from: string; to: string };

export type FsContent =
  | { kind: "none" }
  | { kind: "full"; data: Uint8Array }
  /** LEB128 instruction stream against the last content this client acked
   *  for this path: 0x01 COPY [offset][len], 0x02 INSERT [len][bytes]. */
  | { kind: "delta"; ops: Uint8Array };

/** Append one record to an uncompressed `FS_UPDATE` records buffer. */
export function appendFsRecord(buf: number[], record: FsRecord): void {
  const start = buf.length;
  buf.push(0, 0, 0, 0); // record_len placeholder
  switch (record.kind) {
    case "upsert": {
      buf.push(FS_RECORD_UPSERT, record.entryFlags);
      pushString(buf, record.path);
      pushU64(buf, BigInt(record.size));
      pushU64(buf, record.mtimeNs);
      pushU32(buf, record.mode);
      pushU64(buf, record.hash & 0xffffffffffffffffn);
      pushU64(buf, record.hash >> 64n);
      const content = record.content;
      if (content.kind === "none") {
        buf.push(FS_CONTENT_NONE);
      } else {
        buf.push(content.kind === "full" ? FS_CONTENT_FULL : FS_CONTENT_DELTA);
        const bytes = content.kind === "full" ? content.data : content.ops;
        pushU32(buf, bytes.length);
        for (const b of bytes) buf.push(b);
      }
      break;
    }
    case "delete":
      buf.push(FS_RECORD_DELETE);
      pushString(buf, record.path);
      break;
    case "move":
      buf.push(FS_RECORD_MOVE);
      pushString(buf, record.from);
      pushString(buf, record.to);
      break;
  }
  const len = buf.length - start - 4;
  buf[start] = len & 0xff;
  buf[start + 1] = (len >> 8) & 0xff;
  buf[start + 2] = (len >> 16) & 0xff;
  buf[start + 3] = (len >> 24) & 0xff;
}

function pushString(buf: number[], s: string): void {
  const bytes = textEncoder.encode(s);
  buf.push(bytes.length & 0xff, (bytes.length >> 8) & 0xff);
  for (const b of bytes) buf.push(b);
}

function pushU32(buf: number[], value: number): void {
  buf.push(
    value & 0xff,
    (value >> 8) & 0xff,
    (value >> 16) & 0xff,
    (value >> 24) & 0xff,
  );
}

function pushU64(buf: number[], value: bigint): void {
  for (let i = 0n; i < 8n; i++) {
    buf.push(Number((value >> (8n * i)) & 0xffn));
  }
}

/**
 * Decode records from an uncompressed `FS_UPDATE` payload. Unknown kinds
 * are skipped via `record_len`; a malformed record ends iteration (the
 * update is applied up to that point and the rest dropped —
 * forward-compatible with future record extensions).
 */
export function* fsRecords(data: Uint8Array): Generator<FsRecord> {
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  let offset = 0;
  while (offset + 4 <= data.length) {
    const recLen = view.getUint32(offset, true);
    if (recLen === 0 || offset + 4 + recLen > data.length) return;
    const bodyStart = offset + 4;
    const bodyEnd = bodyStart + recLen;
    offset = bodyEnd;
    const kind = data[bodyStart];
    let pos = bodyStart + 1;

    const takeString = (): string | null => {
      if (pos + 2 > bodyEnd) return null;
      const len = view.getUint16(pos, true);
      if (pos + 2 + len > bodyEnd) return null;
      let s: string;
      try {
        s = textDecoder.decode(data.subarray(pos + 2, pos + 2 + len));
      } catch {
        // Invalid UTF-8 ends iteration, matching Rust's fatal from_utf8.
        return null;
      }
      pos += 2 + len;
      return s;
    };

    switch (kind) {
      case FS_RECORD_UPSERT: {
        if (pos >= bodyEnd) return;
        const entryFlags = data[pos++];
        const path = takeString();
        if (path === null) return;
        if (pos + 8 + 8 + 4 + 16 + 1 > bodyEnd) return;
        const size = Number(view.getBigUint64(pos, true));
        const mtimeNs = view.getBigUint64(pos + 8, true);
        const mode = view.getUint32(pos + 16, true);
        const hash =
          view.getBigUint64(pos + 20, true) |
          (view.getBigUint64(pos + 28, true) << 64n);
        const contentKind = data[pos + 36];
        pos += 37;
        let content: FsContent;
        if (contentKind === FS_CONTENT_NONE) {
          content = { kind: "none" };
        } else if (
          contentKind === FS_CONTENT_FULL ||
          contentKind === FS_CONTENT_DELTA
        ) {
          if (pos + 4 > bodyEnd) return;
          const len = view.getUint32(pos, true);
          if (pos + 4 + len > bodyEnd) return;
          const bytes = data.subarray(pos + 4, pos + 4 + len);
          content =
            contentKind === FS_CONTENT_FULL
              ? { kind: "full", data: bytes }
              : { kind: "delta", ops: bytes };
        } else {
          return;
        }
        yield {
          kind: "upsert",
          path,
          entryFlags,
          size,
          mtimeNs,
          mode,
          hash,
          content,
        };
        break;
      }
      case FS_RECORD_DELETE: {
        const path = takeString();
        if (path === null) return;
        yield { kind: "delete", path };
        break;
      }
      case FS_RECORD_MOVE: {
        const from = takeString();
        if (from === null) return;
        const to = takeString();
        if (to === null) return;
        yield { kind: "move", from, to };
        break;
      }
      default:
        break; // unknown kind: skip via record_len
    }
  }
}

/** Build an `FS_UPDATE` from an uncompressed records buffer (tests/mocks). */
export function buildFsUpdateMessage(
  syncId: number,
  updateId: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  const compressed = fsCompressLiteral(records);
  const msg = new Uint8Array(8 + compressed.length);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_FS_UPDATE;
  v.setUint16(1, syncId, true);
  v.setUint32(3, updateId, true);
  msg[7] = flags;
  msg.set(compressed, 8);
  return msg;
}

/**
 * Parse an `S2C_FS_FILE` message (starting at the opcode byte). Applies the
 * standard decompression guard; null = malformed or over-sized.
 */
export function parseFsFileMessage(
  msg: Uint8Array,
): { nonce: number; status: number; data: Uint8Array } | null {
  if (msg.length < 4 || msg[0] !== S2C_FS_FILE) return null;
  const nonce = msg[1] | (msg[2] << 8);
  const status = msg[3];
  const data = fsDecompress(msg.subarray(4));
  if (data === null) return null;
  return { nonce, status, data };
}

// -- Client API -------------------------------------------------------------

export interface FsSyncOptions {
  /** Watch the whole subtree (default) or only the root's immediate children. */
  recursive?: boolean;
  /** The root is a single FILE (docs/design/fs-watch.md "Single-file
   *  sync"): the mirror holds exactly one entry keyed `""` (the file
   *  itself). Delete/rename-away arrives as `DELETE ""` with the sync
   *  staying open, recreate as `UPSERT ""`; fetches and writes address
   *  path `""`. Mutually exclusive with `recursive` (`syncFs` throws on
   *  the combination); a server predating the flag refuses the open —
   *  the rejection is an {@link FsOpenError} so callers can fall back. */
  single?: boolean;
  /** Attach file bytes to upserts (hashes always sync). */
  content?: boolean;
  /** Descend into mount points. */
  crossFilesystem?: boolean;
  /** Shorthand for `gitignore`, `dotIgnore` and `excludeGit` together —
   *  what "ignore what the repo ignores" usually means. Off by default,
   *  so a sync only narrows when asked; on a checkout it is the
   *  difference between mirroring the work tree and mirroring
   *  `node_modules` and `.git` too. */
  ignore?: boolean;
  /** Honor `.gitignore` in and above the root, plus the governing
   *  repository's `$GIT_DIR/info/exclude`, the user's `core.excludesFile`,
   *  and its `core.ignorecase`. */
  gitignore?: boolean;
  /** Honor `.ignore` files (ripgrep's convention), which bring none of
   *  git's repository-wide sources with them. */
  dotIgnore?: boolean;
  /** Omit `.git` directories and gitfiles. A pure name filter — no git
   *  data is read — and usually what you want alongside `ignore`, since
   *  `.git` is not in anyone's `.gitignore`. */
  excludeGit?: boolean;
  /** Extra gitignore-syntax patterns, anchored at the sync root and
   *  applied above every other rule, so `"!keep"` re-includes something
   *  the ignore files hide. Excluded paths are never enumerated, hashed,
   *  or counted against the server's entry budget. */
  exclude?: string[];
  /** Batching/settle window in ms; 0 = server default (20). */
  latencyMs?: number;
  /** Per-file inline content cap in bytes; 0 = server default (16 MiB). */
  inlineMax?: number;
  /** Called for each applied record (the mirror already reflects it). */
  onRecord?: (record: FsRecord) => void;
  /** A staged snapshot began (`RESET`): the server is restaging instead of
   *  diffing. Only consumers replaying records into their own map care. */
  onReset?: () => void;
  /** The live map is coherent: initial snapshot done, or a restage swapped in. */
  onSync?: () => void;
  /** The live map changed. Updates that only accumulate in the staging
   *  map during a `RESET`…`SYNC` restage don't fire this (or the reactive
   *  notifier) — the `SYNC` swap does, once. */
  onUpdate?: () => void;
  /** The sync ended: an `FS_CLOSED` reason, or
   *  {@link FS_CLOSED_CONNECTION_LOST} when the connection dropped. */
  onClosed?: (reason: number) => void;
  /** Resolve the sync's base directory from this session's live cwd: `path`
   *  is joined onto the source pty's server-side cwd, so the tree follows
   *  `cd` (docs/ide.md Decision 3). The session must be on the same
   *  connection as the sync. */
  fromSessionId?: SessionId;
  /** Root the sync at this connection's drag staging dir
   *  ({@link FS_SYNC_STAGING}): `path` is ignored and sent empty, the dir
   *  is auto-created server-side, and it lives until the connection
   *  closes. Browser drag-and-drop stages dropped files here so a DROP
   *  message names them instead of inlining their bytes. Invalid with
   *  `fromSessionId` — `syncFs` throws on the combination. */
  staging?: boolean;
}

/** A live sync established by `BlitConnection.syncFs`. */
/** Options for {@link FsSyncHandle.writeFile}. */
export interface FsWriteOptions {
  /** CAS: write only if the current content hash equals this (from
   *  `live.get(path)?.hash`). Mutually exclusive with `create`/`force`. */
  ifHash?: bigint;
  /** The exact bytes this client believes are on disk — the content the
   *  nonzero `ifHash` hashes. When set, the write is encoded as a
   *  single-span delta against them when clearly smaller
   *  (docs/design/fs-write.md content_kind 2); otherwise it goes out
   *  full, unchanged. The ops apply against the bytes the CAS
   *  precondition names, so `force`, `create`, or a missing/zero
   *  `ifHash` rejects client-side. A pre-delta server answers INVALID
   *  and the write retries once automatically as a full write with the
   *  same precondition; only the retry's outcome surfaces. */
  deltaBase?: Uint8Array;
  /** Create-exclusive: fail with a conflict if the path already exists. */
  create?: boolean;
  /** Overwrite unconditionally, ignoring any precondition. */
  force?: boolean;
  /** File mode (e.g. 0o644); omitted/0 preserves the existing mode. */
  mode?: number;
  /** Create missing parent directories. */
  createParents?: boolean;
  /** fsync the file and its parent before resolving. */
  durable?: boolean;
}

/** Options for {@link FsSyncHandle.upload}. */
export interface FsUploadOptions {
  /** Unix mode for a created file; omitted/0 preserves the default. */
  mode?: number;
  /** Create missing parent directories. */
  createParents?: boolean;
  /** fsync the file and its parent before resolving. */
  durable?: boolean;
  /** CAS: upload only if the current content hash equals this. Checked
   *  at BEGIN (fail fast, before bytes flow) and re-verified at FINISH;
   *  a mismatch rejects with an {@link FsConflictError} carrying the
   *  current on-disk hash. Mutually exclusive with `create`/`force`. */
  ifHash?: bigint;
  /** Create-exclusive: fail with CONFLICT when the target exists. */
  create?: boolean;
  /** Overwrite unconditionally (the default when neither `ifHash` nor
   *  `create` is given). */
  force?: boolean;
  /** Plaintext bytes per chunk; default 256 KiB. Each chunk rides its own
   *  transport frame, LZ4-compressed. Chunks are small and at most 512 KiB
   *  is left unacked on the wire, so interactive input sharing the
   *  connection is not stuck behind a large upload backlog. */
  chunkSize?: number;
  /** Progress in cumulative plaintext bytes accepted by the server. */
  onProgress?: (uploaded: number, total: number) => void;
  /** Aborting sends `FS_UPLOAD_CANCEL` and rejects the promise. */
  signal?: AbortSignal;
}

/** Result of a successful chunked upload. */
export interface FsUploadResult {
  /** Post-write content hash: the raw 16 wire bytes (little-endian u128). */
  hash: Uint8Array;
  /** The same hash as a bigint (matches `FsWriteResult.hash`). */
  hashU128: bigint;
  /** Modification time in nanoseconds since the epoch (number; loses
   *  sub-microsecond precision — use `mtimeNs` when exactness matters). */
  mtime: number;
  /** Modification time in nanoseconds since the epoch, full precision. */
  mtimeNs: bigint;
}

/** Options for {@link FsSyncHandle.symlink} / {@link FsSyncHandle.hardlink}. */
export interface FsLinkOptions {
  /** Replace only if the current entry's content hash equals this (a
   *  symlink's hash covers its target bytes: `live.get(path)?.hash`). */
  ifHash?: bigint;
  /** Replace unconditionally. Without `ifHash`/`force`, creation is
   *  exclusive: an existing entry rejects with {@link FsConflictError}. */
  force?: boolean;
  /** Create missing parent directories. */
  createParents?: boolean;
}

/** Result of a successful write/mkdir. */
export interface FsWriteResult {
  /** Post-op content hash (0n for a directory). */
  hash: bigint;
  mtimeNs: bigint;
}

export interface FsSyncHandle extends ReactiveStore {
  readonly syncId: number;
  /** Canonical root path on the server. */
  readonly root: string;
  /** The mirrored tree: wire path → node, "" = the root itself.
   *  Replaced wholesale when a staged snapshot swaps in — re-read after
   *  `onSync`, don't retain across callbacks. */
  readonly live: ReadonlyMap<string, FsNode>;
  /** Pull one file's full content (for `FS_ENTRY_NO_CONTENT` entries). */
  fetch(path: string): Promise<Uint8Array>;
  /** Write a file (docs/design/fs-write.md). `path` is the wire/mirror-key
   *  form (as in `live`). Rejects with an {@link FsConflictError} carrying
   *  the current on-disk hash when a precondition fails. On success the
   *  returned hash is also recorded as {@link lastWrittenHash} so the
   *  matching echo can be recognized. Every full-content write goes out as
   *  a chunked {@link upload} — paced so it can't stall interactive input
   *  sharing the connection; only delta writes (`deltaBase`) still use a
   *  single FS_WRITE frame, small by construction. */
  writeFile(
    path: string,
    data: Uint8Array,
    options?: FsWriteOptions,
  ): Promise<FsWriteResult>;
  /** Upload a file as an ordered run of chunks (the `FS_UPLOAD_*` family)
   *  for content too large — or too inconvenient — for a single
   *  `writeFile` frame. Accepts a `Blob` (e.g. a dropped `File`) and reads
   *  it slice by slice, so the whole file is never held in memory at once.
   *  Chunks are pipelined a few frames ahead; `onProgress` reports the
   *  server's cumulative ack. Resolves from the FINISH reply; rejects on an
   *  error status or abort (which also sends `FS_UPLOAD_CANCEL`). The
   *  returned hash is recorded as {@link lastWrittenHash}, like a write. */
  upload(
    path: string,
    data: Uint8Array | Blob,
    opts?: FsUploadOptions,
  ): Promise<FsUploadResult>;
  /** Create a directory. */
  mkdir(
    path: string,
    options?: { mode?: number; createParents?: boolean },
  ): Promise<FsWriteResult>;
  /** Remove a file or subtree; `ifHash` makes it conditional on a file. */
  remove(path: string, options?: { ifHash?: bigint }): Promise<void>;
  /** Rename/move a file or subtree. */
  rename(
    from: string,
    to: string,
    options?: { createParents?: boolean },
  ): Promise<void>;
  /** Create — or, with `ifHash`/`force`, atomically retarget — a symlink
   *  at `path` pointing at the verbatim string `target` (relative,
   *  absolute, or dangling; never resolved by the sync). The returned
   *  hash covers the target bytes and is recorded as
   *  {@link lastWrittenHash} for self-echo suppression. */
  symlink(
    target: string,
    path: string,
    options?: FsLinkOptions,
  ): Promise<FsWriteResult>;
  /** Create a hard link at `path` to the regular file at `source` (both
   *  wire paths under the root). */
  hardlink(
    source: string,
    path: string,
    options?: FsLinkOptions,
  ): Promise<FsWriteResult>;
  /** The hash of this handle's most recent successful `writeFile` at
   *  `path`, for self-echo suppression: when an incoming UPSERT's `hash`
   *  equals this, the change is this handle's own write and the editor
   *  model already holds it (never `setValue` your own echo). Scoped to
   *  the handle, not the shared sync — another handle's write on the same
   *  file is an external change to this one. The entry is dropped once
   *  that echo has been delivered to every callback, so check it inside
   *  `onRecord` (or a subscriber), not later. */
  lastWrittenHash(path: string): bigint | undefined;
  /** Release this handle. Wire-identical opens share one server sync, so
   *  the wire stop goes out with the last handle; `onClosed` fires with
   *  client-request either way. */
  stop(): void;
}

/** Rejection from a write/op whose precondition failed. `hash` is the
 *  current on-disk content hash — rebase against it and retry. */
export class FsConflictError extends Error {
  readonly hash: bigint;
  constructor(hash: bigint) {
    super("filesystem write conflict");
    this.name = "FsConflictError";
    this.hash = hash;
  }
}

// -- Client-side reducer ----------------------------------------------------

/** One node in a mirrored tree. */
export interface FsNode {
  entryFlags: number;
  size: number;
  mtimeNs: bigint;
  mode: number;
  hash: bigint;
  /** Present when the sync requested content and the file fits the inline
   *  limit. `null` does not mean empty — check `entryFlags`. */
  content: Uint8Array | null;
}

function isUnder(path: string, root: string): boolean {
  return (
    root.length === 0 ||
    path === root ||
    (path.length > root.length &&
      path.startsWith(root) &&
      path.charCodeAt(root.length) === 0x2f) // '/'
  );
}

/** Outcome of one applied `FS_UPDATE`. */
export interface FsApplyResult {
  /** The update_id to acknowledge. */
  updateId: number;
  /** Whether `live` changed: false while records only accumulate in the
   *  staging map during a `RESET`…`SYNC` restage, true on the `SYNC` swap
   *  or a direct (non-staged) mutation. */
  liveChanged: boolean;
}

/**
 * The complete client obligation: apply updates, read `live`.
 *
 * Paths are relative to the sync root, `/`-separated, "" = the root itself.
 */
export class FsMirror {
  live = new Map<string, FsNode>();
  private staging: Map<string, FsNode> | null = null;

  /** The staging map while a `RESET`…`SYNC` restage is in flight, else
   *  null. Record consumers joining a shared sync mid-restage replay it
   *  to synthesize a coherent join point; everyone else reads `live`. */
  get staged(): ReadonlyMap<string, FsNode> | null {
    return this.staging;
  }

  /**
   * Apply one `FS_UPDATE` message (starting at the opcode byte).
   * Returns the update_id to acknowledge, or null if malformed.
   */
  applyUpdate(msg: Uint8Array): number | null {
    return this.apply(msg)?.updateId ?? null;
  }

  /**
   * Like {@link applyUpdate}, but also reports whether `live` changed and
   * optionally collects each decoded record into `records` — one
   * decompress + decode shared by the mirror and per-record callbacks.
   */
  apply(msg: Uint8Array, records?: FsRecord[]): FsApplyResult | null {
    if (msg.length < 8 || msg[0] !== S2C_FS_UPDATE) return null;
    const view = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    const updateId = view.getUint32(3, true);
    const flags = msg[7];
    const raw = fsDecompress(msg.subarray(8));
    if (raw === null) return null;
    if (flags & FS_UPDATE_RESET) {
      this.staging = new Map();
    }
    const map = this.staging ?? this.live;
    let mutated = false;
    for (const record of fsRecords(raw)) {
      records?.push(record);
      switch (record.kind) {
        case "upsert": {
          const prev = map.get(record.path);
          let content: Uint8Array | null;
          const c = record.content;
          if (c.kind === "none") {
            const entryType = record.entryFlags & FS_ENTRY_TYPE_MASK;
            const contentBearing =
              entryType === FS_ENTRY_FILE || entryType === FS_ENTRY_SYMLINK;
            const sameType =
              prev !== undefined &&
              (prev.entryFlags & FS_ENTRY_TYPE_MASK) === entryType;
            content =
              !contentBearing ||
              (record.entryFlags &
                (FS_ENTRY_NO_CONTENT |
                  FS_ENTRY_UNREADABLE |
                  FS_ENTRY_UNSTABLE)) !==
                0
                ? null
                : // Metadata-only upsert keeps previous content only when the
                  // entry stays the same content-bearing type.
                  sameType
                  ? (prev.content ?? null)
                  : null;
          } else if (c.kind === "full") {
            content = c.data.slice();
          } else {
            const base = prev?.content ?? new Uint8Array(0);
            content = applyFsDelta(base, c.ops);
            if (content === null) return null;
          }
          map.set(record.path, {
            entryFlags: record.entryFlags,
            size: record.size,
            mtimeNs: record.mtimeNs,
            mode: record.mode,
            hash: record.hash,
            content,
          });
          mutated = true;
          break;
        }
        case "delete": {
          if (record.path.length === 0) {
            mutated = mutated || map.size > 0;
            map.clear();
            break;
          }
          // Deleting the current key during Map iteration is well-defined,
          // so no key-array copy is needed.
          for (const path of map.keys()) {
            if (isUnder(path, record.path)) {
              map.delete(path);
              mutated = true;
            }
          }
          break;
        }
        case "move": {
          const moved: Array<[string, FsNode]> = [];
          for (const [path, node] of map) {
            if (isUnder(path, record.from)) moved.push([path, node]);
          }
          for (const [path] of moved) map.delete(path);
          for (const [path, node] of moved) {
            const suffix =
              path.length > record.from.length
                ? path.slice(
                    record.from.length + (record.from.length === 0 ? 0 : 1),
                  )
                : "";
            map.set(joinMoved(record.to, suffix), node);
            mutated = true;
          }
          break;
        }
      }
    }
    let liveChanged = mutated && this.staging === null;
    if (flags & FS_UPDATE_SYNC && this.staging !== null) {
      this.live = this.staging;
      this.staging = null;
      liveChanged = true;
    }
    return { updateId, liveChanged };
  }
}

function joinMoved(to: string, suffix: string): string {
  if (suffix.length === 0) return to;
  if (to.length === 0) return suffix;
  return `${to}/${suffix}`;
}

/** Append one LEB128-encoded value (u64 range within Number safety). */
function pushLeb128(out: number[], value: number): void {
  for (;;) {
    const byte = value & 0x7f;
    value = Math.floor(value / 128);
    if (value === 0) {
      out.push(byte);
      return;
    }
    out.push(byte | 0x80);
  }
}

/**
 * Single-span delta, the client mirror of the server encoder
 * (crates/fssync/src/lib.rs `encode_delta`): the longest common prefix
 * and suffix become `COPY`s, the middle an `INSERT` — an instruction
 * stream {@link applyFsDelta} decodes back to `next`. Covers appends,
 * prepends, truncations, and one contiguous in-place edit; scattered
 * edits degrade to a large `INSERT`, so callers only send the delta when
 * it is clearly smaller than the full content.
 */
export function encodeFsDelta(base: Uint8Array, next: Uint8Array): Uint8Array {
  const bound = Math.min(base.length, next.length);
  let prefix = 0;
  while (prefix < bound && base[prefix] === next[prefix]) prefix++;
  let suffix = 0;
  const suffixBound = bound - prefix;
  while (
    suffix < suffixBound &&
    base[base.length - 1 - suffix] === next[next.length - 1 - suffix]
  ) {
    suffix++;
  }
  const ops: number[] = [];
  if (prefix > 0) {
    ops.push(0x01);
    pushLeb128(ops, 0);
    pushLeb128(ops, prefix);
  }
  const middleEnd = next.length - suffix;
  if (middleEnd > prefix) {
    ops.push(0x02);
    pushLeb128(ops, middleEnd - prefix);
    for (let i = prefix; i < middleEnd; i++) ops.push(next[i]);
  }
  if (suffix > 0) {
    ops.push(0x01);
    pushLeb128(ops, base.length - suffix);
    pushLeb128(ops, suffix);
  }
  return new Uint8Array(ops);
}

/** Apply a content delta (LEB128 COPY/INSERT instruction stream) to a base. */
export function applyFsDelta(
  base: Uint8Array,
  ops: Uint8Array,
): Uint8Array | null {
  let pos = 0;
  const leb128 = (): number | null => {
    let value = 0;
    let shift = 0;
    for (;;) {
      if (pos >= ops.length || shift >= 53) return null;
      const byte = ops[pos++];
      value += (byte & 0x7f) * 2 ** shift;
      if ((byte & 0x80) === 0) return value;
      shift += 7;
    }
  };
  const chunks: Uint8Array[] = [];
  let total = 0;
  while (pos < ops.length) {
    const op = ops[pos++];
    if (op === 0x01) {
      const offset = leb128();
      const len = leb128();
      if (offset === null || len === null || offset + len > base.length)
        return null;
      if (total + len > FS_MAX_DECOMPRESSED) return null;
      chunks.push(base.subarray(offset, offset + len));
      total += len;
    } else if (op === 0x02) {
      const len = leb128();
      if (len === null || pos + len > ops.length) return null;
      if (total + len > FS_MAX_DECOMPRESSED) return null;
      chunks.push(ops.subarray(pos, pos + len));
      pos += len;
      total += len;
    } else {
      return null;
    }
  }
  const out = new Uint8Array(total);
  let di = 0;
  for (const chunk of chunks) {
    out.set(chunk, di);
    di += chunk.length;
  }
  return out;
}
