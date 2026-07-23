/**
 * Git introspection (docs/git.md): wire constants, message builders,
 * record codecs, and the client-side state mirror.
 *
 * The server splits repository data along its grain: mutable-and-small
 * state (HEAD, refs, in-progress operation, upstream tracking, stash,
 * status) is pushed as whole snapshots the client applies with
 * {@link GitStateMirror} — replace the map, acknowledge — while
 * immutable-and-large content (commits, trees, blobs, patches) is pulled
 * by oid and cacheable forever.
 *
 * All integers little-endian, tightly packed, as everywhere in the protocol.
 */

import { fsCompressLiteral, fsDecompress } from "./fs.js";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { ignoreBOM: true });

// -- Opcodes ----------------------------------------------------------------

/** Open a repo: [0x50][nonce:2][flags:1][refs_latency_ms:2][status_latency_ms:2][path_len:2][path:N] */
export const C2S_GIT_OPEN = 0x50;
/** Close a repo: [0x51][repo_id:2] */
export const C2S_GIT_CLOSE = 0x51;
/** Acknowledge a state snapshot: [0x52][repo_id:2][state_id:4] */
export const C2S_GIT_ACK = 0x52;
/** Walk commits: [0x53][nonce:2][repo_id:2][flags:1][limit:2][path_len:2][path:N][n_tips:2][tips][n_hides:2][hides] */
export const C2S_GIT_LOG = 0x53;
/** List one tree level: [0x54][nonce:2][repo_id:2][oid:32][path_len:2][path:N] */
export const C2S_GIT_TREE = 0x54;
/** Fetch object bytes: [0x55][nonce:2][repo_id:2][oid:32][path_len:2][path:N][max_len:4] */
export const C2S_GIT_BLOB = 0x55;
/** File-level diff: [0x56][nonce:2][repo_id:2][flags:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N] */
export const C2S_GIT_DIFF = 0x56;
/** Patch rows or text: [0x57][nonce:2][repo_id:2][flags:1][context:1][old_kind:1][old:32][new_kind:1][new:32][path_len:2][path:N][max_len:4] */
export const C2S_GIT_PATCH = 0x57;
/** Enumerate the index: [0x58][nonce:2][repo_id:2][path_len:2][path:N] */
export const C2S_GIT_INDEX = 0x58;
/** Cancel an in-flight request: [0x59][nonce:2] */
export const C2S_GIT_CANCEL = 0x59;
/** Merge base: [0x5A][nonce:2][repo_id:2][n_oids:1][oids:32·N] */
export const C2S_GIT_BASE = 0x5a;
/** Resolve a revision spec to commit oids: [0x5B][nonce:2][repo_id:2][spec_len:2][spec:N].
 *  `spec` is any git revision expression — a ref, (short) oid, `HEAD~3`, or
 *  a range `A..B` / `A...B`. The reply gives `tips`/`hides` for {@link msgGitLog}. */
export const C2S_GIT_RESOLVE = 0x5b;
/** Subscribe to a live log: [0x5C][log_id:2][repo_id:2][flags:1][limit:2][spec_len:2][spec:N].
 *  The server resolves `spec` and pushes a `GIT_LOG_PAGE`, re-emitting when
 *  the resolved endpoints move. `log_id` is client-assigned (unique per
 *  connection); `flags` are the `GIT_LOG_*` bits. */
export const C2S_GIT_LOG_WATCH = 0x5c;
/** End a log subscription: [0x5D][log_id:2][repo_id:2] */
export const C2S_GIT_LOG_UNWATCH = 0x5d;
/** Acknowledge a log page (coalescing pacing): [0x5E][log_id:2][repo_id:2][update_id:4] */
export const C2S_GIT_LOG_ACK = 0x5e;

/** Open reply: [0x50][nonce:2][repo_id:2][status:1][oid_format:1][flags:1][workdir_len:2][workdir][gitdir_len:2][gitdir] */
export const S2C_GIT_REPO = 0x50;
/** Whole-state snapshot: [0x51][repo_id:2][state_id:4][flags:1][records:LZ4] */
export const S2C_GIT_STATE = 0x51;
/** Repo terminated: [0x52][repo_id:2][reason:1] */
export const S2C_GIT_CLOSED = 0x52;
/** Log page: [0x53][nonce:2][status:1][flags:1][n_frontier:2][frontier][records:LZ4] */
export const S2C_GIT_COMMITS = 0x53;
/** Tree listing: [0x54][nonce:2][status:1][flags:1][records:LZ4] */
export const S2C_GIT_TREE = 0x54;
/** Blob bytes: [0x55][nonce:2][status:1][size:8][data:LZ4] */
export const S2C_GIT_BLOB = 0x55;
/** Diff entries: [0x56][nonce:2][status:1][flags:1][records:LZ4] */
export const S2C_GIT_DIFF = 0x56;
/** Patch rows/text: [0x57][nonce:2][status:1][flags:1][data:LZ4] */
export const S2C_GIT_PATCH = 0x57;
/** Index entries: [0x58][nonce:2][status:1][flags:1][records:LZ4] */
export const S2C_GIT_INDEX = 0x58;
/** Merge bases: [0x5A][nonce:2][status:1][n_bases:1][bases:32·N] */
export const S2C_GIT_BASE = 0x5a;
/** Resolve reply: [0x5B][nonce:2][status:1][n_tips:2][tips:32·N][n_hides:2][hides:32·N] */
export const S2C_GIT_RESOLVE = 0x5b;
/** Live log page: [0x5C][log_id:2][update_id:4][status:1][flags:1][n_frontier:2][frontier:32·N][records:LZ4].
 *  Same records as `GIT_COMMITS`; re-sent (coalesced, acked) when the
 *  subscription's resolved endpoints move. `flags` bit 0 `MORE` marks a
 *  truncated head page — pull older history with `GIT_LOG` from `frontier`. */
export const S2C_GIT_LOG_PAGE = 0x5c;

/** `S2C_HELLO` feature bit: server supports the `GIT_*` message family. */
export const FEATURE_GIT = 1 << 7;

// One status table for every `status` byte in the family (docs/git.md).
export const GIT_STATUS_OK = 0;
export const GIT_STATUS_UNKNOWN_ID = 1;
export const GIT_STATUS_NOT_FOUND = 2;
export const GIT_STATUS_WRONG_TYPE = 3;
export const GIT_STATUS_PERMISSION = 4;
export const GIT_STATUS_TOO_LARGE = 5;
export const GIT_STATUS_BUDGET = 6;
export const GIT_STATUS_INVALID = 7;
export const GIT_STATUS_CANCELLED = 8;
export const GIT_STATUS_OTHER = 9;

// C2S_GIT_OPEN flags. STATUS and TRACKING imply WATCH; UNTRACKED implies
// STATUS; IGNORED implies UNTRACKED.
export const GIT_OPEN_WATCH = 1 << 0;
export const GIT_OPEN_STATUS = 1 << 1;
export const GIT_OPEN_UNTRACKED = 1 << 2;
export const GIT_OPEN_IGNORED = 1 << 3;
export const GIT_OPEN_TRACKING = 1 << 4;

/** `repo_id` reported by a failed `GIT_REPO`. */
export const GIT_REPO_ID_INVALID = 0xffff;

export const GIT_OID_FORMAT_SHA1 = 0;
export const GIT_OID_FORMAT_SHA256 = 1;

// S2C_GIT_REPO flags.
export const GIT_REPO_BARE = 1 << 0;
export const GIT_REPO_SHALLOW = 1 << 1;
export const GIT_REPO_SPARSE = 1 << 2;
export const GIT_REPO_LINKED = 1 << 3;

// S2C_GIT_CLOSED reasons.
export const GIT_CLOSED_CLIENT_REQUEST = 0;
export const GIT_CLOSED_REPO_GONE = 1;
export const GIT_CLOSED_PERMISSION_LOST = 2;
export const GIT_CLOSED_BACKEND_FAILED = 3;
export const GIT_CLOSED_RESOURCE_LIMIT = 4;
/** Client-side pseudo-reason: the connection dropped or was re-established.
 *  Repos do not survive reconnects — re-`openRepo`. */
export const GIT_CLOSED_CONNECTION_LOST = -1;

// S2C_GIT_STATE flags.
export const GIT_STATE_REFS_TRUNCATED = 1 << 0;
export const GIT_STATE_STATUS_TRUNCATED = 1 << 1;

// C2S_GIT_LOG flags.
export const GIT_LOG_FIRST_PARENT = 1 << 0;
export const GIT_LOG_TOPO = 1 << 1;
export const GIT_LOG_FULL_MESSAGE = 1 << 2;
export const GIT_LOG_FOLLOW = 1 << 3;
export const GIT_LOG_PATH_OIDS = 1 << 4;

// S2C_GIT_COMMITS flags.
export const GIT_COMMITS_MORE = 1 << 0;

// C2S_GIT_DIFF / C2S_GIT_PATCH request flags (shared bits 0-4).
export const GIT_DIFF_RENAMES = 1 << 0;
export const GIT_DIFF_UNTRACKED = 1 << 1;
export const GIT_DIFF_IGNORED = 1 << 2;
export const GIT_DIFF_IGNORE_SPACE_CHANGE = 1 << 3;
export const GIT_DIFF_IGNORE_ALL_SPACE = 1 << 4;
// C2S_GIT_PATCH-only request flags.
export const GIT_PATCH_TEXT = 1 << 5;
export const GIT_PATCH_CHAR_SPANS = 1 << 6;
export const GIT_PATCH_NO_SPANS = 1 << 7;

// Response flags.
export const GIT_TREE_TRUNCATED = 1 << 0;
export const GIT_DIFF_TRUNCATED = 1 << 0;
export const GIT_INDEX_TRUNCATED = 1 << 0;
export const GIT_PATCH_STRUCTURED = 1 << 0;
export const GIT_PATCH_TRUNCATED = 1 << 1;

// Diff endpoints.
export const GIT_ENDPOINT_EMPTY = 0;
export const GIT_ENDPOINT_COMMIT = 1;
export const GIT_ENDPOINT_TREE = 2;
export const GIT_ENDPOINT_INDEX = 3;
export const GIT_ENDPOINT_WORKTREE = 4;
/** Old side only: the server substitutes `merge-base(oid, new)`. */
export const GIT_ENDPOINT_MERGE_BASE = 5;

// GIT_STATE record kinds.
export const GIT_STATE_RECORD_HEAD = 0x01;
export const GIT_STATE_RECORD_REF = 0x02;
export const GIT_STATE_RECORD_OP = 0x03;
export const GIT_STATE_RECORD_STATUS = 0x04;
export const GIT_STATE_RECORD_UPSTREAM = 0x05;
export const GIT_STATE_RECORD_STASH = 0x06;

export const GIT_HEAD_DETACHED = 1 << 0;
export const GIT_HEAD_UNBORN = 1 << 1;
export const GIT_REF_PEELED_VALID = 1 << 0;
export const GIT_REF_SYMBOLIC = 1 << 1;
export const GIT_OP_MERGE = 1;
export const GIT_OP_REBASE = 2;
export const GIT_OP_CHERRY_PICK = 3;
export const GIT_OP_REVERT = 4;
export const GIT_OP_BISECT = 5;
export const GIT_STATUS_ENTRY_CONFLICTED = 1 << 0;
export const GIT_UPSTREAM_GONE = 1 << 0;
export const GIT_UPSTREAM_COUNTS_VALID = 1 << 1;

// GIT_COMMITS record kinds.
export const GIT_COMMIT_RECORD_COMMIT = 0x01;
export const GIT_COMMIT_RECORD_PATH_AT = 0x02;
export const GIT_COMMIT_LOSSY_ENCODING = 1 << 0;

// GIT_TREE record kind and object types.
export const GIT_TREE_RECORD_ENTRY = 0x02;
export const GIT_OTYPE_COMMIT = 1;
export const GIT_OTYPE_TREE = 2;
export const GIT_OTYPE_BLOB = 3;

// GIT_DIFF record kinds.
export const GIT_DIFF_RECORD_ENTRY = 0x03;
export const GIT_DIFF_RECORD_BASE = 0x04;
export const GIT_DIFF_ENTRY_BINARY = 1 << 0;
export const GIT_DIFF_ENTRY_SUBMODULE = 1 << 1;

// GIT_PATCH record kinds.
export const GIT_PATCH_RECORD_FILE = 0x01;
export const GIT_PATCH_RECORD_ROW = 0x02;
export const GIT_PATCH_RECORD_GAP = 0x03;
export const GIT_PATCH_RECORD_BASE = 0x04;
export const GIT_PATCH_FILE_BINARY = 1 << 0;

// GIT_INDEX record kind.
export const GIT_INDEX_RECORD_ENTRY = 0x04;
export const GIT_INDEX_INTENT_TO_ADD = 1 << 0;
export const GIT_INDEX_SKIP_WORKTREE = 1 << 1;

// -- Oids -------------------------------------------------------------------

/** Always 32 bytes on the wire, zero-padded past the repo's hash width. */
export type GitOid = Uint8Array;

export const GIT_OID_NONE: GitOid = new Uint8Array(32);

export function gitOidEqual(a: GitOid, b: GitOid): boolean {
  if (a.length !== 32 || b.length !== 32) return false;
  for (let i = 0; i < 32; i++) if (a[i] !== b[i]) return false;
  return true;
}

export function gitOidIsZero(oid: GitOid): boolean {
  return gitOidEqual(oid, GIT_OID_NONE);
}

/** Lowercase hex of the oid's meaningful width (40 for SHA-1, 64 for SHA-256). */
export function gitOidHex(
  oid: GitOid,
  oidFormat: number = GIT_OID_FORMAT_SHA1,
): string {
  const width = oidFormat === GIT_OID_FORMAT_SHA1 ? 20 : 32;
  return [...oid.subarray(0, width)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Parse a hex oid of either width into wire form; null on malformation. */
export function gitOidFromHex(hex: string): GitOid | null {
  if (hex.length !== 40 && hex.length !== 64) return null;
  if (!/^[0-9a-fA-F]+$/.test(hex)) return null;
  const oid = new Uint8Array(32);
  for (let i = 0; i < hex.length / 2; i++) {
    const byte = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    if (Number.isNaN(byte)) return null;
    oid[i] = byte;
  }
  return oid;
}

// -- Builder helpers --------------------------------------------------------

function pushU16(buf: number[], value: number): void {
  buf.push(value & 0xff, (value >> 8) & 0xff);
}

function pushU32(buf: number[], value: number): void {
  buf.push(
    value & 0xff,
    (value >> 8) & 0xff,
    (value >> 16) & 0xff,
    (value >> 24) & 0xff,
  );
}

function pushI64(buf: number[], value: bigint): void {
  let v = BigInt.asUintN(64, value);
  for (let i = 0; i < 8; i++) {
    buf.push(Number(v & 0xffn));
    v >>= 8n;
  }
}

function pushStr(buf: number[], text: string): void {
  const bytes = textEncoder.encode(text);
  pushU16(buf, bytes.length);
  buf.push(...bytes);
}

function pushOid(buf: number[], oid: GitOid): void {
  for (let i = 0; i < 32; i++) buf.push(oid[i] ?? 0);
}

/** Bounds-checked little-endian reader. */
class Cursor {
  pos = 0;
  constructor(readonly data: Uint8Array) {}
  get ok(): boolean {
    return this.pos >= 0;
  }
  private take(n: number): number {
    if (this.pos < 0 || this.pos + n > this.data.length) {
      this.pos = -1;
      return -1;
    }
    const at = this.pos;
    this.pos += n;
    return at;
  }
  u8(): number {
    const at = this.take(1);
    return at < 0 ? 0 : this.data[at];
  }
  u16(): number {
    const at = this.take(2);
    return at < 0 ? 0 : this.data[at] | (this.data[at + 1] << 8);
  }
  u32(): number {
    const at = this.take(4);
    if (at < 0) return 0;
    return (
      (this.data[at] | (this.data[at + 1] << 8) | (this.data[at + 2] << 16)) +
      this.data[at + 3] * 0x1000000
    );
  }
  i64(): bigint {
    const at = this.take(8);
    if (at < 0) return 0n;
    let v = 0n;
    for (let i = 7; i >= 0; i--) v = (v << 8n) | BigInt(this.data[at + i]);
    return BigInt.asIntN(64, v);
  }
  u64(): bigint {
    const at = this.take(8);
    if (at < 0) return 0n;
    let v = 0n;
    for (let i = 7; i >= 0; i--) v = (v << 8n) | BigInt(this.data[at + i]);
    return v;
  }
  bytes(n: number): Uint8Array {
    const at = this.take(n);
    return at < 0 ? new Uint8Array(0) : this.data.subarray(at, at + n);
  }
  oid(): GitOid {
    return new Uint8Array(this.bytes(32));
  }
  str(): string {
    return textDecoder.decode(this.bytes(this.u16()));
  }
  rest(): Uint8Array {
    return this.pos < 0 ? new Uint8Array(0) : this.data.subarray(this.pos);
  }
}

// -- Message builders and parsers -------------------------------------------

export interface GitEndpoint {
  kind: number;
  oid: GitOid;
}

export function msgGitOpen(
  nonce: number,
  flags: number,
  refsLatencyMs: number,
  statusLatencyMs: number,
  path: string,
): Uint8Array {
  const buf: number[] = [C2S_GIT_OPEN];
  pushU16(buf, nonce);
  buf.push(flags);
  pushU16(buf, refsLatencyMs);
  pushU16(buf, statusLatencyMs);
  pushStr(buf, path);
  return new Uint8Array(buf);
}

export function msgGitClose(repoId: number): Uint8Array {
  const buf: number[] = [C2S_GIT_CLOSE];
  pushU16(buf, repoId);
  return new Uint8Array(buf);
}

export function msgGitAck(repoId: number, stateId: number): Uint8Array {
  const buf: number[] = [C2S_GIT_ACK];
  pushU16(buf, repoId);
  pushU32(buf, stateId);
  return new Uint8Array(buf);
}

export interface GitLogRequest {
  nonce: number;
  repoId: number;
  flags: number;
  /** 0 = server default; clamped to the server maximum. */
  limit: number;
  /** Subtree filter (escaped wire path); "" = everything. */
  path: string;
  tips: GitOid[];
  hides: GitOid[];
}

export function msgGitLog(req: GitLogRequest): Uint8Array {
  const buf: number[] = [C2S_GIT_LOG];
  pushU16(buf, req.nonce);
  pushU16(buf, req.repoId);
  buf.push(req.flags);
  pushU16(buf, req.limit);
  pushStr(buf, req.path);
  pushU16(buf, req.tips.length);
  for (const oid of req.tips) pushOid(buf, oid);
  pushU16(buf, req.hides.length);
  for (const oid of req.hides) pushOid(buf, oid);
  return new Uint8Array(buf);
}

export function msgGitTree(
  nonce: number,
  repoId: number,
  oid: GitOid,
  path: string,
): Uint8Array {
  const buf: number[] = [C2S_GIT_TREE];
  pushU16(buf, nonce);
  pushU16(buf, repoId);
  pushOid(buf, oid);
  pushStr(buf, path);
  return new Uint8Array(buf);
}

export function msgGitBlob(
  nonce: number,
  repoId: number,
  oid: GitOid,
  path: string,
  maxLen: number,
): Uint8Array {
  const buf: number[] = [C2S_GIT_BLOB];
  pushU16(buf, nonce);
  pushU16(buf, repoId);
  pushOid(buf, oid);
  pushStr(buf, path);
  pushU32(buf, maxLen);
  return new Uint8Array(buf);
}

export interface GitDiffRequest {
  nonce: number;
  repoId: number;
  flags: number;
  old: GitEndpoint;
  new: GitEndpoint;
  /** Subtree filter (escaped wire path); "" = everything. */
  path: string;
}

export function msgGitDiff(req: GitDiffRequest): Uint8Array {
  const buf: number[] = [C2S_GIT_DIFF];
  pushU16(buf, req.nonce);
  pushU16(buf, req.repoId);
  buf.push(req.flags);
  buf.push(req.old.kind);
  pushOid(buf, req.old.oid);
  buf.push(req.new.kind);
  pushOid(buf, req.new.oid);
  pushStr(buf, req.path);
  return new Uint8Array(buf);
}

export interface GitPatchRequest extends GitDiffRequest {
  /** Context lines; 0 = server default (3). */
  context: number;
  /** Response size cap; 0 = server default. */
  maxLen: number;
}

export function msgGitPatch(req: GitPatchRequest): Uint8Array {
  const buf: number[] = [C2S_GIT_PATCH];
  pushU16(buf, req.nonce);
  pushU16(buf, req.repoId);
  buf.push(req.flags);
  buf.push(req.context);
  buf.push(req.old.kind);
  pushOid(buf, req.old.oid);
  buf.push(req.new.kind);
  pushOid(buf, req.new.oid);
  pushStr(buf, req.path);
  pushU32(buf, req.maxLen);
  return new Uint8Array(buf);
}

export function msgGitIndex(
  nonce: number,
  repoId: number,
  path: string,
): Uint8Array {
  const buf: number[] = [C2S_GIT_INDEX];
  pushU16(buf, nonce);
  pushU16(buf, repoId);
  pushStr(buf, path);
  return new Uint8Array(buf);
}

export function msgGitCancel(nonce: number): Uint8Array {
  const buf: number[] = [C2S_GIT_CANCEL];
  pushU16(buf, nonce);
  return new Uint8Array(buf);
}

export function msgGitBase(
  nonce: number,
  repoId: number,
  oids: GitOid[],
): Uint8Array {
  const buf: number[] = [C2S_GIT_BASE];
  pushU16(buf, nonce);
  pushU16(buf, repoId);
  buf.push(oids.length);
  for (const oid of oids) pushOid(buf, oid);
  return new Uint8Array(buf);
}

export function msgGitResolve(
  nonce: number,
  repoId: number,
  spec: string,
): Uint8Array {
  const buf: number[] = [C2S_GIT_RESOLVE];
  pushU16(buf, nonce);
  pushU16(buf, repoId);
  pushStr(buf, spec);
  return new Uint8Array(buf);
}

export function msgGitLogWatch(
  logId: number,
  repoId: number,
  flags: number,
  limit: number,
  spec: string,
): Uint8Array {
  const buf: number[] = [C2S_GIT_LOG_WATCH];
  pushU16(buf, logId);
  pushU16(buf, repoId);
  buf.push(flags);
  pushU16(buf, limit);
  pushStr(buf, spec);
  return new Uint8Array(buf);
}

export function msgGitLogUnwatch(logId: number, repoId: number): Uint8Array {
  const buf: number[] = [C2S_GIT_LOG_UNWATCH];
  pushU16(buf, logId);
  pushU16(buf, repoId);
  return new Uint8Array(buf);
}

export function msgGitLogAck(
  logId: number,
  repoId: number,
  updateId: number,
): Uint8Array {
  const buf: number[] = [C2S_GIT_LOG_ACK];
  pushU16(buf, logId);
  pushU16(buf, repoId);
  pushU32(buf, updateId);
  return new Uint8Array(buf);
}

export interface GitRepoInfo {
  nonce: number;
  repoId: number;
  status: number;
  oidFormat: number;
  flags: number;
  /** Escaped canonical worktree root; empty for bare. On failure, a
   *  diagnostic message. */
  workdir: string;
  /** Escaped canonical git directory. */
  gitdir: string;
}

export function parseGitRepo(msg: Uint8Array): GitRepoInfo | null {
  if (msg.length < 12 || msg[0] !== S2C_GIT_REPO) return null;
  const c = new Cursor(msg.subarray(1));
  const info: GitRepoInfo = {
    nonce: c.u16(),
    repoId: c.u16(),
    status: c.u8(),
    oidFormat: c.u8(),
    flags: c.u8(),
    workdir: c.str(),
    gitdir: c.str(),
  };
  return c.ok ? info : null;
}

/** Parse `S2C_GIT_STATE` into `[repoId, stateId, flags, records]`. */
export function parseGitState(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  if (msg.length < 8 || msg[0] !== S2C_GIT_STATE) return null;
  const c = new Cursor(msg.subarray(1));
  const repoId = c.u16();
  const stateId = c.u32();
  const flags = c.u8();
  const records = fsDecompress(c.rest());
  if (!c.ok || records === null) return null;
  return [repoId, stateId, flags, records];
}

export function parseGitClosed(msg: Uint8Array): [number, number] | null {
  if (msg.length < 4 || msg[0] !== S2C_GIT_CLOSED) return null;
  return [msg[1] | (msg[2] << 8), msg[3]];
}

export interface GitCommitsPage {
  nonce: number;
  status: number;
  flags: number;
  /** Pass as `tips` with the same `hides` to continue the walk. */
  frontier: GitOid[];
  /** Uncompressed records; decode with {@link gitCommitRecords}. */
  records: Uint8Array;
}

export function parseGitCommits(msg: Uint8Array): GitCommitsPage | null {
  if (msg.length < 7 || msg[0] !== S2C_GIT_COMMITS) return null;
  const c = new Cursor(msg.subarray(1));
  const nonce = c.u16();
  const status = c.u8();
  const flags = c.u8();
  const count = c.u16();
  const frontier: GitOid[] = [];
  for (let i = 0; i < count; i++) frontier.push(c.oid());
  const records = fsDecompress(c.rest());
  if (!c.ok || records === null) return null;
  return { nonce, status, flags, frontier, records };
}

/** Parse tree/diff/index responses: `[nonce, status, flags, records]`. */
function parseRecordsResp(
  msg: Uint8Array,
  opcode: number,
): [number, number, number, Uint8Array] | null {
  if (msg.length < 5 || msg[0] !== opcode) return null;
  const c = new Cursor(msg.subarray(1));
  const nonce = c.u16();
  const status = c.u8();
  const flags = c.u8();
  const records = fsDecompress(c.rest());
  if (!c.ok || records === null) return null;
  return [nonce, status, flags, records];
}

export function parseGitTreeResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  return parseRecordsResp(msg, S2C_GIT_TREE);
}

export function parseGitDiffResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  return parseRecordsResp(msg, S2C_GIT_DIFF);
}

export function parseGitPatchResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  return parseRecordsResp(msg, S2C_GIT_PATCH);
}

export function parseGitIndexResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  return parseRecordsResp(msg, S2C_GIT_INDEX);
}

/** Parse `S2C_GIT_BLOB` into `[nonce, status, size, data]`. `size` is the
 *  true object size even when `TOO_LARGE` leaves `data` empty. */
export function parseGitBlobResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  if (msg.length < 12 || msg[0] !== S2C_GIT_BLOB) return null;
  const c = new Cursor(msg.subarray(1));
  const nonce = c.u16();
  const status = c.u8();
  const size = Number(c.u64());
  const data = fsDecompress(c.rest());
  if (!c.ok || data === null) return null;
  return [nonce, status, size, data];
}

export function parseGitBaseResp(
  msg: Uint8Array,
): [number, number, GitOid[]] | null {
  if (msg.length < 5 || msg[0] !== S2C_GIT_BASE) return null;
  const c = new Cursor(msg.subarray(1));
  const nonce = c.u16();
  const status = c.u8();
  const count = c.u8();
  const bases: GitOid[] = [];
  for (let i = 0; i < count; i++) bases.push(c.oid());
  return c.ok ? [nonce, status, bases] : null;
}

export interface GitResolveResult {
  nonce: number;
  status: number;
  /** Feed as `tips` to {@link msgGitLog} (or a live watch). */
  tips: GitOid[];
  /** Feed as `hides` to {@link msgGitLog}. */
  hides: GitOid[];
}

export function parseGitResolveResp(msg: Uint8Array): GitResolveResult | null {
  if (msg.length < 4 || msg[0] !== S2C_GIT_RESOLVE) return null;
  const c = new Cursor(msg.subarray(1));
  const nonce = c.u16();
  const status = c.u8();
  const tips: GitOid[] = [];
  for (let n = c.u16(), i = 0; i < n; i++) tips.push(c.oid());
  const hides: GitOid[] = [];
  for (let n = c.u16(), i = 0; i < n; i++) hides.push(c.oid());
  return c.ok ? { nonce, status, tips, hides } : null;
}

export interface GitLogPage {
  logId: number;
  /** Acknowledge with {@link msgGitLogAck} to receive later updates. */
  updateId: number;
  status: number;
  /** `GIT_COMMITS_*` flags; bit 0 `MORE` marks a truncated head page. */
  flags: number;
  /** Pass as `tips` with the same `hides` to continue the walk statelessly. */
  frontier: GitOid[];
  /** Uncompressed records; decode with {@link gitCommitRecords}. */
  records: Uint8Array;
}

export function parseGitLogPage(msg: Uint8Array): GitLogPage | null {
  if (msg.length < 11 || msg[0] !== S2C_GIT_LOG_PAGE) return null;
  const c = new Cursor(msg.subarray(1));
  const logId = c.u16();
  const updateId = c.u32();
  const status = c.u8();
  const flags = c.u8();
  const count = c.u16();
  const frontier: GitOid[] = [];
  for (let i = 0; i < count; i++) frontier.push(c.oid());
  const records = fsDecompress(c.rest());
  if (!c.ok || records === null) return null;
  return { logId, updateId, status, flags, frontier, records };
}

// Server-side builders (tests and mock servers).

export function msgGitState(
  repoId: number,
  stateId: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  const buf: number[] = [S2C_GIT_STATE];
  pushU16(buf, repoId);
  pushU32(buf, stateId);
  buf.push(flags);
  const compressed = fsCompressLiteral(records);
  const msg = new Uint8Array(buf.length + compressed.length);
  msg.set(buf, 0);
  msg.set(compressed, buf.length);
  return msg;
}

export function msgGitClosed(repoId: number, reason: number): Uint8Array {
  const buf: number[] = [S2C_GIT_CLOSED];
  pushU16(buf, repoId);
  buf.push(reason);
  return new Uint8Array(buf);
}

export function msgGitResolveResp(
  nonce: number,
  status: number,
  tips: GitOid[],
  hides: GitOid[],
): Uint8Array {
  const buf: number[] = [S2C_GIT_RESOLVE];
  pushU16(buf, nonce);
  buf.push(status);
  pushU16(buf, tips.length);
  for (const oid of tips) pushOid(buf, oid);
  pushU16(buf, hides.length);
  for (const oid of hides) pushOid(buf, oid);
  return new Uint8Array(buf);
}

export function msgGitLogPage(
  logId: number,
  updateId: number,
  status: number,
  flags: number,
  frontier: GitOid[],
  records: Uint8Array,
): Uint8Array {
  const buf: number[] = [S2C_GIT_LOG_PAGE];
  pushU16(buf, logId);
  pushU32(buf, updateId);
  buf.push(status);
  buf.push(flags);
  pushU16(buf, frontier.length);
  for (const oid of frontier) pushOid(buf, oid);
  const compressed = fsCompressLiteral(records);
  const msg = new Uint8Array(buf.length + compressed.length);
  msg.set(buf, 0);
  msg.set(compressed, buf.length);
  return msg;
}

// -- Records ----------------------------------------------------------------

export type GitStateRecord =
  | { kind: "head"; flags: number; oid: GitOid; name: string }
  | { kind: "ref"; flags: number; oid: GitOid; peeled: GitOid; name: string }
  | { kind: "op"; op: number; oid: GitOid; detail: string }
  | {
      kind: "status";
      staged: number;
      unstaged: number;
      flags: number;
      oldPath: string;
      path: string;
    }
  | {
      kind: "upstream";
      flags: number;
      ahead: number;
      behind: number;
      name: string;
      upstream: string;
    }
  | {
      kind: "stash";
      index: number;
      oid: GitOid;
      time: bigint;
      tz: number;
      msg: string;
    };

export type GitCommitRecord =
  | {
      kind: "commit";
      flags: number;
      oid: GitOid;
      tree: GitOid;
      parents: GitOid[];
      authorTime: bigint;
      authorTz: number;
      committerTime: bigint;
      committerTz: number;
      authorName: string;
      authorEmail: string;
      committerName: string;
      committerEmail: string;
      message: string;
    }
  | { kind: "pathAt"; otype: number; mode: number; oid: GitOid; path: string };

export type GitTreeRecord = {
  kind: "entry";
  otype: number;
  mode: number;
  oid: GitOid;
  name: string;
};

export type GitDiffRecord =
  | {
      kind: "entry";
      /** ASCII porcelain letter (A M D R C T U) as a char code. */
      st: number;
      similarity: number;
      dflags: number;
      oldMode: number;
      newMode: number;
      oldOid: GitOid;
      newOid: GitOid;
      oldPath: string;
      newPath: string;
    }
  | { kind: "base"; oid: GitOid };

export type GitPatchRecord =
  | { kind: "file"; flags: number; oldPath: string; newPath: string }
  | {
      kind: "row";
      /** 1-based; 0 = side absent (pure addition/deletion). */
      oldLine: number;
      newLine: number;
      oldText: Uint8Array;
      newText: Uint8Array;
      /** Changed byte ranges `[start, len]` within each side's text. */
      oldSpans: Array<[number, number]>;
      newSpans: Array<[number, number]>;
    }
  | { kind: "gap"; oldLine: number; newLine: number }
  | { kind: "base"; oid: GitOid };

export type GitIndexRecord = {
  kind: "entry";
  stage: number;
  iflags: number;
  mode: number;
  size: number;
  mtimeNs: bigint;
  oid: GitOid;
  path: string;
};

/**
 * Iterate `[record_len:4][kind:1][…]` records. Unknown kinds are skipped
 * (their decoder returns null without touching the cursor); a malformed
 * record — one whose decoder overran the body — ends the payload, matching
 * the Rust codec (docs/git.md).
 */
function* records<T>(
  data: Uint8Array,
  decode: (kind: number, c: Cursor) => T | null,
): Generator<T> {
  let rest = data;
  while (rest.length >= 4) {
    const len = rest[0] | (rest[1] << 8) | (rest[2] << 16) | (rest[3] << 24);
    if (len === 0 || rest.length < 4 + len) return;
    const body = rest.subarray(4, 4 + len);
    rest = rest.subarray(4 + len);
    const c = new Cursor(body);
    const kind = c.u8();
    const decoded = decode(kind, c);
    if (!c.ok) return; // malformed record ends the payload
    if (decoded !== null) yield decoded;
  }
}

export function gitStateRecords(data: Uint8Array): Generator<GitStateRecord> {
  return records(data, (kind, c) => {
    switch (kind) {
      case GIT_STATE_RECORD_HEAD:
        return {
          kind: "head",
          flags: c.u8(),
          oid: c.oid(),
          name: c.str(),
        } as const;
      case GIT_STATE_RECORD_REF:
        return {
          kind: "ref",
          flags: c.u8(),
          oid: c.oid(),
          peeled: c.oid(),
          name: c.str(),
        } as const;
      case GIT_STATE_RECORD_OP:
        return {
          kind: "op",
          op: c.u8(),
          oid: c.oid(),
          detail: c.str(),
        } as const;
      case GIT_STATE_RECORD_STATUS:
        return {
          kind: "status",
          staged: c.u8(),
          unstaged: c.u8(),
          flags: c.u8(),
          oldPath: c.str(),
          path: c.str(),
        } as const;
      case GIT_STATE_RECORD_UPSTREAM:
        return {
          kind: "upstream",
          flags: c.u8(),
          ahead: c.u32(),
          behind: c.u32(),
          name: c.str(),
          upstream: c.str(),
        } as const;
      case GIT_STATE_RECORD_STASH:
        return {
          kind: "stash",
          index: c.u16(),
          oid: c.oid(),
          time: c.i64(),
          tz: (c.u16() << 16) >> 16,
          msg: c.str(),
        } as const;
      default:
        return null;
    }
  });
}

export function gitCommitRecords(data: Uint8Array): Generator<GitCommitRecord> {
  return records(data, (kind, c) => {
    switch (kind) {
      case GIT_COMMIT_RECORD_COMMIT: {
        const flags = c.u8();
        const oid = c.oid();
        const tree = c.oid();
        const parentCount = c.u8();
        const parents: GitOid[] = [];
        for (let i = 0; i < parentCount; i++) parents.push(c.oid());
        const authorTime = c.i64();
        const authorTz = (c.u16() << 16) >> 16;
        const committerTime = c.i64();
        const committerTz = (c.u16() << 16) >> 16;
        const authorName = c.str();
        const authorEmail = c.str();
        const committerName = c.str();
        const committerEmail = c.str();
        const message = textDecoder.decode(c.bytes(c.u32()));
        return {
          kind: "commit",
          flags,
          oid,
          tree,
          parents,
          authorTime,
          authorTz,
          committerTime,
          committerTz,
          authorName,
          authorEmail,
          committerName,
          committerEmail,
          message,
        } as const;
      }
      case GIT_COMMIT_RECORD_PATH_AT:
        return {
          kind: "pathAt",
          otype: c.u8(),
          mode: c.u32(),
          oid: c.oid(),
          path: c.str(),
        } as const;
      default:
        return null;
    }
  });
}

export function gitTreeRecords(data: Uint8Array): Generator<GitTreeRecord> {
  return records(data, (kind, c) => {
    if (kind !== GIT_TREE_RECORD_ENTRY) return null;
    return {
      kind: "entry",
      otype: c.u8(),
      mode: c.u32(),
      oid: c.oid(),
      name: c.str(),
    } as const;
  });
}

export function gitDiffRecords(data: Uint8Array): Generator<GitDiffRecord> {
  return records(data, (kind, c) => {
    switch (kind) {
      case GIT_DIFF_RECORD_ENTRY:
        return {
          kind: "entry",
          st: c.u8(),
          similarity: c.u8(),
          dflags: c.u8(),
          oldMode: c.u32(),
          newMode: c.u32(),
          oldOid: c.oid(),
          newOid: c.oid(),
          oldPath: c.str(),
          newPath: c.str(),
        } as const;
      case GIT_DIFF_RECORD_BASE:
        return { kind: "base", oid: c.oid() } as const;
      default:
        return null;
    }
  });
}

export function gitPatchRecords(data: Uint8Array): Generator<GitPatchRecord> {
  return records(data, (kind, c) => {
    switch (kind) {
      case GIT_PATCH_RECORD_FILE:
        return {
          kind: "file",
          flags: c.u8(),
          oldPath: c.str(),
          newPath: c.str(),
        } as const;
      case GIT_PATCH_RECORD_ROW: {
        const oldLine = c.u32();
        const newLine = c.u32();
        const oldText = new Uint8Array(c.bytes(c.u32()));
        const newText = new Uint8Array(c.bytes(c.u32()));
        const takeSpans = (): Array<[number, number]> => {
          const count = c.u16();
          const spans: Array<[number, number]> = [];
          for (let i = 0; i < count; i++) spans.push([c.u32(), c.u32()]);
          return spans;
        };
        return {
          kind: "row",
          oldLine,
          newLine,
          oldText,
          newText,
          oldSpans: takeSpans(),
          newSpans: takeSpans(),
        } as const;
      }
      case GIT_PATCH_RECORD_GAP:
        return { kind: "gap", oldLine: c.u32(), newLine: c.u32() } as const;
      case GIT_PATCH_RECORD_BASE:
        return { kind: "base", oid: c.oid() } as const;
      default:
        return null;
    }
  });
}

export function gitIndexRecords(data: Uint8Array): Generator<GitIndexRecord> {
  return records(data, (kind, c) => {
    if (kind !== GIT_INDEX_RECORD_ENTRY) return null;
    return {
      kind: "entry",
      stage: c.u8(),
      iflags: c.u8(),
      mode: c.u32(),
      size: Number(c.u64()),
      mtimeNs: c.u64(),
      oid: c.oid(),
      path: c.str(),
    } as const;
  });
}

/** Append one record to an uncompressed records buffer (tests and mocks). */
export function appendGitStateRecord(
  buf: number[],
  record: GitStateRecord,
): void {
  const start = buf.length;
  buf.push(0, 0, 0, 0);
  switch (record.kind) {
    case "head":
      buf.push(GIT_STATE_RECORD_HEAD, record.flags);
      pushOid(buf, record.oid);
      pushStr(buf, record.name);
      break;
    case "ref":
      buf.push(GIT_STATE_RECORD_REF, record.flags);
      pushOid(buf, record.oid);
      pushOid(buf, record.peeled);
      pushStr(buf, record.name);
      break;
    case "op":
      buf.push(GIT_STATE_RECORD_OP, record.op);
      pushOid(buf, record.oid);
      pushStr(buf, record.detail);
      break;
    case "status":
      buf.push(
        GIT_STATE_RECORD_STATUS,
        record.staged,
        record.unstaged,
        record.flags,
      );
      pushStr(buf, record.oldPath);
      pushStr(buf, record.path);
      break;
    case "upstream":
      buf.push(GIT_STATE_RECORD_UPSTREAM, record.flags);
      pushU32(buf, record.ahead);
      pushU32(buf, record.behind);
      pushStr(buf, record.name);
      pushStr(buf, record.upstream);
      break;
    case "stash":
      buf.push(GIT_STATE_RECORD_STASH);
      pushU16(buf, record.index);
      pushOid(buf, record.oid);
      pushI64(buf, record.time);
      pushU16(buf, record.tz & 0xffff);
      pushStr(buf, record.msg);
      break;
  }
  const len = buf.length - start - 4;
  buf[start] = len & 0xff;
  buf[start + 1] = (len >> 8) & 0xff;
  buf[start + 2] = (len >> 16) & 0xff;
  buf[start + 3] = (len >> 24) & 0xff;
}

// -- Connection-level API shapes --------------------------------------------

export interface GitOpenOptions {
  /** Stream `GIT_STATE` snapshots (implied by status/tracking). */
  watch?: boolean;
  /** Include index/worktree status entries in state. */
  status?: boolean;
  /** Status includes untracked files (implies status). */
  untracked?: boolean;
  /** Status includes ignored files (implies untracked). */
  ignored?: boolean;
  /** Include per-branch upstream ahead/behind records. */
  tracking?: boolean;
  /** Ref settle window in ms; 0 = server default (50). */
  refsLatencyMs?: number;
  /** Status settle window in ms; 0 = server default (500). */
  statusLatencyMs?: number;
  /** A state snapshot was applied and acknowledged. */
  onState?: (mirror: GitStateMirror, stateId: number) => void;
  /** The repo ended: a `GIT_CLOSED` reason, or
   *  {@link GIT_CLOSED_CONNECTION_LOST} when the connection dropped. */
  onClosed?: (reason: number) => void;
}

/** A repository opened by `BlitConnection.openRepo`. */
export interface GitRepoHandle {
  readonly repoId: number;
  readonly oidFormat: number;
  /** `GIT_REPO_*` flags (bare/shallow/sparse/linked). */
  readonly repoFlags: number;
  /** Escaped canonical worktree root; empty for bare. */
  readonly workdir: string;
  /** Escaped canonical git directory. */
  readonly gitdir: string;
  /** Live state; populated when watching. Replaced wholesale per snapshot. */
  readonly state: GitStateMirror;
  /** One page of `hides..tips`; continue with `frontier` as `tips`. */
  log(
    req?: Partial<Omit<GitLogRequest, "nonce" | "repoId">>,
  ): Promise<GitCommitsPage>;
  /** One tree level; oid may be a commit/tag (peeled server-side). */
  tree(oid: GitOid, path?: string): Promise<GitTreeRecord[]>;
  /** Raw object bytes, cached by oid (immutable, cache-forever). */
  blob(oid: GitOid, path?: string, maxLen?: number): Promise<Uint8Array>;
  /** File-level diff records between two endpoints. */
  diff(
    old: GitEndpoint,
    newEndpoint: GitEndpoint,
    opts?: { flags?: number; path?: string },
  ): Promise<GitDiffRecord[]>;
  /** Patch rows (default) or unified text (`GIT_PATCH_TEXT`). */
  patch(
    old: GitEndpoint,
    newEndpoint: GitEndpoint,
    opts?: { flags?: number; context?: number; path?: string; maxLen?: number },
  ): Promise<{ flags: number; records: GitPatchRecord[]; text: Uint8Array }>;
  /** Index entries under a path prefix. */
  index(path?: string): Promise<GitIndexRecord[]>;
  /** Merge base of two or more commits; empty = disjoint histories. */
  mergeBase(oids: GitOid[]): Promise<GitOid[]>;
  /** Resolve a revision spec (ref, oid, `HEAD~3`, `A..B`, `A...B`) to the
   *  `tips`/`hides` a {@link log} or {@link watchLog} walks between. */
  resolve(spec: string): Promise<{ tips: GitOid[]; hides: GitOid[] }>;
  /** Subscribe to a server-pushed log of `spec`. `onUpdate` fires with the
   *  first page and again whenever the resolved endpoints move (a named ref
   *  changes). Pages are acknowledged automatically. `close()` unsubscribes. */
  watchLog(
    spec: string,
    opts: GitLogWatchOptions,
    onUpdate: (page: GitLogPage) => void,
  ): GitLogSubscription;
  /** Close the repo; `onClosed` fires when the server confirms. */
  close(): void;
}

export interface GitLogWatchOptions {
  /** `GIT_LOG_*` bits (first-parent, topo, full-message, follow, path-oids). */
  flags?: number;
  /** Page size; 0 = server default, clamped to the server maximum. */
  limit?: number;
}

/** A live log subscription created by {@link GitRepoHandle.watchLog}. */
export interface GitLogSubscription {
  /** Client-assigned subscription id (unique per connection). */
  readonly logId: number;
  /** Unsubscribe; sends `GIT_LOG_UNWATCH` and stops delivering pages. */
  close(): void;
}

/** Human-readable unified-status text. */
export function gitStatusText(status: number): string {
  switch (status) {
    case GIT_STATUS_UNKNOWN_ID:
      return "unknown repo";
    case GIT_STATUS_NOT_FOUND:
      return "not found";
    case GIT_STATUS_WRONG_TYPE:
      return "wrong object type";
    case GIT_STATUS_PERMISSION:
      return "permission denied";
    case GIT_STATUS_TOO_LARGE:
      return "too large";
    case GIT_STATUS_BUDGET:
      return "budget exhausted";
    case GIT_STATUS_INVALID:
      return "invalid request";
    case GIT_STATUS_CANCELLED:
      return "cancelled";
    default:
      return "error";
  }
}

// -- State mirror -----------------------------------------------------------

export interface GitHead {
  flags: number;
  oid: GitOid;
  /** Symbolic target; empty when detached. */
  name: string;
}

export interface GitRefState {
  flags: number;
  oid: GitOid;
  peeled: GitOid;
}

export interface GitUpstreamState {
  flags: number;
  ahead: number;
  behind: number;
  upstream: string;
}

export interface GitStatusEntry {
  staged: number;
  unstaged: number;
  flags: number;
  oldPath: string;
  path: string;
}

export interface GitStashEntry {
  index: number;
  oid: GitOid;
  time: bigint;
  tz: number;
  message: string;
}

export interface GitOpState {
  op: number;
  oid: GitOid;
  detail: string;
}

/**
 * The complete client obligation for the state stream: apply snapshots
 * (each replaces the whole state), acknowledge the returned id.
 */
export class GitStateMirror {
  head: GitHead | null = null;
  refs = new Map<string, GitRefState>();
  op: GitOpState | null = null;
  status: GitStatusEntry[] = [];
  upstreams = new Map<string, GitUpstreamState>();
  stashes: GitStashEntry[] = [];
  flags = 0;

  /** Apply one `S2C_GIT_STATE` message; returns the `state_id` to ack,
   *  or null when malformed. */
  applyState(msg: Uint8Array): number | null {
    const parsed = parseGitState(msg);
    if (parsed === null) return null;
    const [, stateId, flags, recordBytes] = parsed;
    this.head = null;
    this.refs = new Map();
    this.op = null;
    this.status = [];
    this.upstreams = new Map();
    this.stashes = [];
    this.flags = flags;
    for (const record of gitStateRecords(recordBytes)) {
      switch (record.kind) {
        case "head":
          this.head = {
            flags: record.flags,
            oid: record.oid,
            name: record.name,
          };
          break;
        case "ref":
          this.refs.set(record.name, {
            flags: record.flags,
            oid: record.oid,
            peeled: record.peeled,
          });
          break;
        case "op":
          this.op = { op: record.op, oid: record.oid, detail: record.detail };
          break;
        case "status":
          this.status.push({
            staged: record.staged,
            unstaged: record.unstaged,
            flags: record.flags,
            oldPath: record.oldPath,
            path: record.path,
          });
          break;
        case "upstream":
          this.upstreams.set(record.name, {
            flags: record.flags,
            ahead: record.ahead,
            behind: record.behind,
            upstream: record.upstream,
          });
          break;
        case "stash":
          this.stashes.push({
            index: record.index,
            oid: record.oid,
            time: record.time,
            tz: record.tz,
            message: record.msg,
          });
          break;
      }
    }
    return stateId;
  }
}
