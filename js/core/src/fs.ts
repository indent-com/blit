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

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

// -- Opcodes ----------------------------------------------------------------

/** Start a sync: [0x40][nonce:2][flags:1][latency_ms:2][inline_max:4][path_len:2][path:N] */
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
export const FEATURE_FS_SYNC = 1 << 6;

/** `sync_id` reported by a failed `FS_SYNCED`. */
export const FS_SYNC_ID_INVALID = 0xffff;

// C2S_FS_SYNC flags.
export const FS_SYNC_RECURSIVE = 1 << 0;
export const FS_SYNC_CONTENT = 1 << 1;
export const FS_SYNC_CROSS_FILESYSTEM = 1 << 2;

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

// UPSERT content kinds.
export const FS_CONTENT_NONE = 0;
export const FS_CONTENT_FULL = 1;
export const FS_CONTENT_DELTA = 2;

// -- Message builders (client to server) ------------------------------------

export function buildFsSyncMessage(
  nonce: number,
  flags: number,
  latencyMs: number,
  inlineMax: number,
  path: string,
): Uint8Array {
  const pathBytes = textEncoder.encode(path);
  const msg = new Uint8Array(12 + pathBytes.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_FS_SYNC;
  v.setUint16(1, nonce, true);
  msg[3] = flags;
  v.setUint16(4, latencyMs, true);
  v.setUint32(6, inlineMax, true);
  v.setUint16(10, pathBytes.length, true);
  msg.set(pathBytes, 12);
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
  const compressed = fsCompressLiteral(a.content);
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
    // Overlapping copies are the point of LZ4 — byte-by-byte is required.
    let mi = di - offset;
    for (let i = 0; i < matchLen; i++) out[di++] = out[mi++];
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
    return new Uint8Array(header);
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
      const s = textDecoder.decode(data.subarray(pos + 2, pos + 2 + len));
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
  /** Attach file bytes to upserts (hashes always sync). */
  content?: boolean;
  /** Descend into mount points. */
  crossFilesystem?: boolean;
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
  /** An update was applied and acknowledged. */
  onUpdate?: () => void;
  /** The sync ended: an `FS_CLOSED` reason, or
   *  {@link FS_CLOSED_CONNECTION_LOST} when the connection dropped. */
  onClosed?: (reason: number) => void;
}

/** A live sync established by `BlitConnection.syncFs`. */
/** Options for {@link FsSyncHandle.writeFile}. */
export interface FsWriteOptions {
  /** CAS: write only if the current content hash equals this (from
   *  `live.get(path)?.hash`). Mutually exclusive with `create`/`force`. */
  ifHash?: bigint;
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

/** Result of a successful write/mkdir. */
export interface FsWriteResult {
  /** Post-op content hash (0n for a directory). */
  hash: bigint;
  mtimeNs: bigint;
}

export interface FsSyncHandle {
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
   *  matching echo can be recognized. */
  writeFile(
    path: string,
    data: Uint8Array,
    options?: FsWriteOptions,
  ): Promise<FsWriteResult>;
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
  /** The hash of the most recent successful `writeFile` at `path`, for
   *  self-echo suppression: when an incoming UPSERT's `hash` equals this,
   *  the change is this client's own write and the editor model already
   *  holds it (never `setValue` your own echo). */
  lastWrittenHash(path: string): bigint | undefined;
  /** Stop the sync; `onClosed` fires with client-request when the server confirms. */
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

/**
 * The complete client obligation: apply updates, read `live`.
 *
 * Paths are relative to the sync root, `/`-separated, "" = the root itself.
 */
export class FsMirror {
  live = new Map<string, FsNode>();
  private staging: Map<string, FsNode> | null = null;

  /**
   * Apply one `FS_UPDATE` message (starting at the opcode byte).
   * Returns the update_id to acknowledge, or null if malformed.
   */
  applyUpdate(msg: Uint8Array): number | null {
    if (msg.length < 8 || msg[0] !== S2C_FS_UPDATE) return null;
    const view = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    const updateId = view.getUint32(3, true);
    const flags = msg[7];
    const records = fsDecompress(msg.subarray(8));
    if (records === null) return null;
    if (flags & FS_UPDATE_RESET) {
      this.staging = new Map();
    }
    const map = this.staging ?? this.live;
    for (const record of fsRecords(records)) {
      switch (record.kind) {
        case "upsert": {
          const prev = map.get(record.path);
          let content: Uint8Array | null;
          const c = record.content;
          if (c.kind === "none") {
            content =
              (record.entryFlags &
                (FS_ENTRY_NO_CONTENT |
                  FS_ENTRY_UNREADABLE |
                  FS_ENTRY_UNSTABLE)) !==
              0
                ? null
                : // Metadata-only upsert keeps previous content.
                  (prev?.content ?? null);
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
          break;
        }
        case "delete": {
          for (const path of [...map.keys()]) {
            if (isUnder(path, record.path)) map.delete(path);
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
          }
          break;
        }
      }
    }
    if (flags & FS_UPDATE_SYNC && this.staging !== null) {
      this.live = this.staging;
      this.staging = null;
    }
    return updateId;
  }
}

function joinMoved(to: string, suffix: string): string {
  if (suffix.length === 0) return to;
  if (to.length === 0) return suffix;
  return `${to}/${suffix}`;
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
      chunks.push(base.subarray(offset, offset + len));
      total += len;
    } else if (op === 0x02) {
      const len = leb128();
      if (len === null || pos + len > ops.length) return null;
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
