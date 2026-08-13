/**
 * Server KV store (docs/design/kv.md): wire constants, message builders,
 * record codecs, and the client-side mirror reducer.
 *
 * A host-local key→value store with CAS writes and prefix-watch
 * subscriptions. Keys are raw UTF-8 (≤ {@link KV_MAX_KEY} bytes, no NUL,
 * non-empty); values are opaque bytes, LZ4 on the wire. CAS is BLAKE3-128
 * over value bytes with the zero-hash absent sentinel — fs-write's
 * conflict model verbatim (docs/design/fs-write.md).
 *
 * All integers little-endian, tightly packed, as everywhere in the protocol.
 */

import { fsDecompress, fsCompress, fsCompressLiteral } from "./fs.js";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

// -- Opcodes ----------------------------------------------------------------

/** Subscribe to a prefix: [0x70][nonce:2][flags:1][inline_max:4][prefix_len:2][prefix:N]
 *  The prefix is a literal byte prefix (no glob); empty = whole store. */
export const C2S_KV_OPEN = 0x70;
/** Close a subscription: [0x71][kv_id:2] */
export const C2S_KV_STOP = 0x71;
/** Cumulative acknowledgement: [0x72][kv_id:2][update_id:4] */
export const C2S_KV_ACK = 0x72;
/** CAS put/delete: [0x73][nonce:2][flags:1][base:16][key_len:2][key:N][value:LZ4] */
export const C2S_KV_PUT = 0x73;
/** Fetch one value: [0x74][nonce:2][key_len:2][key:N] */
export const C2S_KV_FETCH = 0x74;

/** Subscription accepted or refused: [0x70][nonce:2][kv_id:2][status:1][detail_len:2][detail:N] */
export const S2C_KV_OPENED = 0x70;
/** Snapshot/live records: [0x71][kv_id:2][update_id:4][flags:1][records:LZ4] */
export const S2C_KV_UPDATE = 0x71;
/** Put result: [0x72][nonce:2][status:1][hash:16][mtime_ns:8] */
export const S2C_KV_DONE = 0x72;
/** Fetch result: [0x73][nonce:2][status:1][hash:16][data:LZ4] */
export const S2C_KV_VALUE = 0x73;
/** Server-initiated close: [0x74][kv_id:2][reason:1]. The subscription is
 *  gone server-side — the `kv_id` is dead, a late `KV_ACK` for it is
 *  ignored, and recovery is a fresh `KV_OPEN` (the snapshot is the
 *  recovery: updates carry state, not events; docs/design/kv.md
 *  "Retention"). */
export const S2C_KV_CLOSED = 0x74;

/** `S2C_HELLO` feature bit: server supports the `KV_*` family. `BLIT_KV=0`
 *  refuses every `KV_*` at dispatch with `PERMISSION` instead of
 *  un-advertising. */
export const FEATURE_KV = 1 << 9;

/** `kv_id` reported by a failed `KV_OPENED`. */
export const KV_ID_INVALID = 0xffff;

/** Maximum key length in bytes (fixed, not an env knob). */
export const KV_MAX_KEY = 256;

// C2S_KV_PUT flags.
/** Ignore `base`; unconditional put/delete. */
export const KV_PUT_NO_CAS = 1 << 0;
/** Remove the entry (value must be empty). A delete is a put of absence;
 *  `base` zero with DELETE is INVALID (delete-iff-absent is meaningless). */
export const KV_PUT_DELETE = 1 << 1;
/** fsync the store before replying; default trades durability for latency. */
export const KV_PUT_DURABLE = 1 << 2;

// S2C_KV_UPDATE flags.
/** This batch completes the initial snapshot; subsequent updates are live. */
export const KV_UPDATE_SNAPSHOT_END = 1 << 0;

// S2C_KV_CLOSED reasons — the fs numbering (docs/design/fs-watch.md
// `FS_CLOSED`); 0–3 are reserved, only RESOURCE_LIMIT is produced today.
/** Queued-unacked bytes breached `BLIT_KV_UNACKED_MAX`: the client
 *  stalled its acks and the server dropped the subscription. */
export const KV_CLOSED_RESOURCE_LIMIT = 4;

/** Human-readable `S2C_KV_CLOSED` reason. */
export function kvClosedText(reason: number): string {
  return reason === KV_CLOSED_RESOURCE_LIMIT
    ? "resource limit"
    : `closed (reason ${reason})`;
}

// KV status — the common protocol registry. Same
// numeric values as `FS_DONE_*` where they overlap.
export const KV_STATUS_OK = 0;
export const KV_STATUS_NOT_FOUND = 2;
export const KV_STATUS_PERMISSION = 4;
export const KV_STATUS_TOO_LARGE = 5;
export const KV_STATUS_BUDGET = 6;
export const KV_STATUS_INVALID = 7;
export const KV_STATUS_OTHER = 9;
/** A CAS precondition failed; `KvDone.hash` carries the current value hash. */
export const KV_STATUS_CONFLICT = 11;

/** Human-readable `KV_*` status. */
export function kvStatusText(status: number): string {
  switch (status) {
    case KV_STATUS_OK:
      return "ok";
    case KV_STATUS_NOT_FOUND:
      return "not found";
    case KV_STATUS_PERMISSION:
      return "permission denied";
    case KV_STATUS_TOO_LARGE:
      return "too large";
    case KV_STATUS_BUDGET:
      return "budget exhausted";
    case KV_STATUS_INVALID:
      return "invalid request";
    case KV_STATUS_OTHER:
      return "backend error";
    case KV_STATUS_CONFLICT:
      return "conflict";
    default:
      return `unknown status ${status}`;
  }
}

/** Wire-key validity: non-empty, ≤ {@link KV_MAX_KEY} UTF-8 bytes, no NUL. */
export function kvKeyValid(key: string): boolean {
  if (key.length === 0 || key.includes("\0")) return false;
  return textEncoder.encode(key).length <= KV_MAX_KEY;
}

// Record kinds inside KV_UPDATE.
export const KV_RECORD_UPSERT = 0x01;
export const KV_RECORD_DELETE = 0x02;

// UPSERT content kinds.
export const KV_CONTENT_NONE = 0;
export const KV_CONTENT_FULL = 1;

const U64_MASK = 0xffffffffffffffffn;

/** Write a 128-bit value as two little-endian u64 (low word first). */
function setU128(v: DataView, off: number, value: bigint): void {
  v.setBigUint64(off, value & U64_MASK, true);
  v.setBigUint64(off + 8, (value >> 64n) & U64_MASK, true);
}

function getU128(v: DataView, off: number): bigint {
  return v.getBigUint64(off, true) | (v.getBigUint64(off + 8, true) << 64n);
}

// -- Message builders (client to server) ------------------------------------

export function buildKvOpenMessage(
  nonce: number,
  flags: number,
  inlineMax: number,
  prefix: string,
): Uint8Array {
  const pb = textEncoder.encode(prefix);
  const msg = new Uint8Array(10 + pb.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_KV_OPEN;
  v.setUint16(1, nonce, true);
  msg[3] = flags;
  v.setUint32(4, inlineMax, true);
  v.setUint16(8, pb.length, true);
  msg.set(pb, 10);
  return msg;
}

export function buildKvStopMessage(kvId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_KV_STOP;
  msg[1] = kvId & 0xff;
  msg[2] = (kvId >> 8) & 0xff;
  return msg;
}

export function buildKvAckMessage(kvId: number, updateId: number): Uint8Array {
  const msg = new Uint8Array(7);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_KV_ACK;
  v.setUint16(1, kvId, true);
  v.setUint32(3, updateId, true);
  return msg;
}

export interface KvPutArgs {
  nonce: number;
  flags: number;
  /** CAS precondition hash (0n = create-exclusive; ignored under NO_CAS). */
  base: bigint;
  key: string;
  value: Uint8Array;
}

export function buildKvPutMessage(a: KvPutArgs): Uint8Array {
  const kb = textEncoder.encode(a.key);
  const compressed = fsCompress(a.value);
  const msg = new Uint8Array(22 + kb.length + compressed.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_KV_PUT;
  v.setUint16(1, a.nonce, true);
  msg[3] = a.flags;
  setU128(v, 4, a.base);
  v.setUint16(20, kb.length, true);
  msg.set(kb, 22);
  msg.set(compressed, 22 + kb.length);
  return msg;
}

export function buildKvFetchMessage(nonce: number, key: string): Uint8Array {
  const kb = textEncoder.encode(key);
  const msg = new Uint8Array(5 + kb.length);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_KV_FETCH;
  v.setUint16(1, nonce, true);
  v.setUint16(3, kb.length, true);
  msg.set(kb, 5);
  return msg;
}

// -- Server-to-client parsers -----------------------------------------------

export interface KvOpened {
  nonce: number;
  kvId: number;
  status: number;
  detail: string;
}

/** Parse an `S2C_KV_OPENED`; null = malformed or wrong opcode. */
export function parseKvOpenedMessage(msg: Uint8Array): KvOpened | null {
  if (msg.length < 8 || msg[0] !== S2C_KV_OPENED) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  const nonce = v.getUint16(1, true);
  const kvId = v.getUint16(3, true);
  const status = msg[5];
  const detailLen = v.getUint16(6, true);
  if (msg.length < 8 + detailLen) return null;
  const detail = textDecoder.decode(msg.subarray(8, 8 + detailLen));
  return { nonce, kvId, status, detail };
}

export interface KvDone {
  nonce: number;
  status: number;
  /** New value hash on success (0n for a delete); current hash on CONFLICT. */
  hash: bigint;
  mtimeNs: bigint;
}

/** Parse an `S2C_KV_DONE`; null = malformed or wrong opcode. */
export function parseKvDoneMessage(msg: Uint8Array): KvDone | null {
  if (msg.length < 28 || msg[0] !== S2C_KV_DONE) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  return {
    nonce: v.getUint16(1, true),
    status: msg[3],
    hash: getU128(v, 4),
    mtimeNs: v.getBigUint64(20, true),
  };
}

export interface KvValue {
  nonce: number;
  status: number;
  hash: bigint;
  data: Uint8Array;
}

/** Parse an `S2C_KV_VALUE` (decompression guarded); null = malformed. */
export function parseKvValueMessage(msg: Uint8Array): KvValue | null {
  if (msg.length < 20 || msg[0] !== S2C_KV_VALUE) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  const data = fsDecompress(msg.subarray(20));
  if (data == null) return null;
  return {
    nonce: v.getUint16(1, true),
    status: msg[3],
    hash: getU128(v, 4),
    data,
  };
}

// -- Records ----------------------------------------------------------------

/** One decoded record from a `KV_UPDATE` payload. */
export type KvRecord =
  | {
      kind: "upsert";
      key: string;
      /** BLAKE3-128 of the value bytes. */
      hash: bigint;
      size: number;
      mtimeNs: bigint;
      /** Inline value iff `size` ≤ the subscription's `inline_max`;
       *  null = fetch on demand. */
      value: Uint8Array | null;
    }
  | { kind: "delete"; key: string };

/** Encode records into an uncompressed `KV_UPDATE` buffer (tests and mock
 *  servers; real servers use the Rust encoder). */
export function encodeKvRecords(records: readonly KvRecord[]): Uint8Array {
  const parts: Uint8Array[] = [];
  for (const r of records) {
    const kb = textEncoder.encode(r.key);
    if (r.kind === "upsert") {
      const val = r.value;
      const bodyLen =
        1 + 2 + kb.length + 16 + 4 + 8 + 1 + (val ? 4 + val.length : 0);
      const rec = new Uint8Array(4 + bodyLen);
      const v = new DataView(rec.buffer);
      v.setUint32(0, bodyLen, true);
      rec[4] = KV_RECORD_UPSERT;
      v.setUint16(5, kb.length, true);
      rec.set(kb, 7);
      let off = 7 + kb.length;
      setU128(v, off, r.hash);
      off += 16;
      v.setUint32(off, r.size, true);
      off += 4;
      v.setBigUint64(off, r.mtimeNs, true);
      off += 8;
      if (val) {
        rec[off++] = KV_CONTENT_FULL;
        v.setUint32(off, val.length, true);
        rec.set(val, off + 4);
      } else {
        rec[off] = KV_CONTENT_NONE;
      }
      parts.push(rec);
    } else {
      const bodyLen = 1 + 2 + kb.length;
      const rec = new Uint8Array(4 + bodyLen);
      const v = new DataView(rec.buffer);
      v.setUint32(0, bodyLen, true);
      rec[4] = KV_RECORD_DELETE;
      v.setUint16(5, kb.length, true);
      rec.set(kb, 7);
      parts.push(rec);
    }
  }
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

/** Build a `KV_UPDATE` from records (tests and mock servers). */
export function buildKvUpdateMessage(
  kvId: number,
  updateId: number,
  flags: number,
  records: readonly KvRecord[],
): Uint8Array {
  const compressed = fsCompressLiteral(encodeKvRecords(records));
  const msg = new Uint8Array(8 + compressed.length);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_KV_UPDATE;
  v.setUint16(1, kvId, true);
  v.setUint32(3, updateId, true);
  msg[7] = flags;
  msg.set(compressed, 8);
  return msg;
}

/** Decode an uncompressed records buffer. Unknown kinds are skipped via
 *  `record_len`; a malformed record ends decoding (the fs rule). */
export function decodeKvRecords(data: Uint8Array): KvRecord[] {
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const out: KvRecord[] = [];
  let off = 0;
  while (off + 4 <= data.length) {
    const recLen = v.getUint32(off, true);
    if (recLen === 0 || off + 4 + recLen > data.length) break;
    const bodyStart = off + 4;
    off += 4 + recLen;
    const kind = data[bodyStart];
    if (kind !== KV_RECORD_UPSERT && kind !== KV_RECORD_DELETE) continue;
    let p = bodyStart + 1;
    if (p + 2 > off) break;
    const keyLen = v.getUint16(p, true);
    p += 2;
    if (p + keyLen > off) break;
    let key: string;
    try {
      key = textDecoder.decode(data.subarray(p, p + keyLen));
    } catch {
      break;
    }
    p += keyLen;
    if (kind === KV_RECORD_DELETE) {
      out.push({ kind: "delete", key });
      continue;
    }
    if (p + 16 + 4 + 8 + 1 > off) break;
    const hash = getU128(v, p);
    p += 16;
    const size = v.getUint32(p, true);
    p += 4;
    const mtimeNs = v.getBigUint64(p, true);
    p += 8;
    const contentKind = data[p];
    p += 1;
    let value: Uint8Array | null = null;
    if (contentKind === KV_CONTENT_FULL) {
      if (p + 4 > off) break;
      const len = v.getUint32(p, true);
      p += 4;
      if (p + len > off) break;
      value = data.slice(p, p + len);
    } else if (contentKind !== KV_CONTENT_NONE) {
      break;
    }
    out.push({ kind: "upsert", key, hash, size, mtimeNs, value });
  }
  return out;
}

export interface KvUpdate {
  kvId: number;
  updateId: number;
  flags: number;
  records: KvRecord[];
}

/** Parse a `KV_UPDATE` (decompression guarded); null = malformed. */
export function parseKvUpdateMessage(msg: Uint8Array): KvUpdate | null {
  if (msg.length < 8 || msg[0] !== S2C_KV_UPDATE) return null;
  const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
  const records = fsDecompress(msg.subarray(8));
  if (records == null) return null;
  return {
    kvId: v.getUint16(1, true),
    updateId: v.getUint32(3, true),
    flags: msg[7],
    records: decodeKvRecords(records),
  };
}

// -- Mirror -----------------------------------------------------------------

/** One mirrored entry. */
export interface KvEntry {
  /** BLAKE3-128 of the value bytes. */
  hash: bigint;
  size: number;
  mtimeNs: bigint;
  /** Present iff the value arrived inline; null = fetch on demand. */
  value: Uint8Array | null;
}

/** Options for a prefix-watch subscription. */
export interface KvWatchOptions {
  /** Values at or under this size arrive inline in updates; larger ones
   *  are metadata-only (fetch on demand). 0 = server default (inline all
   *  under the value cap). */
  inlineMax?: number;
  /** Fires after each applied update (snapshot batches included). */
  onUpdate?: (mirror: KvMirror) => void;
  /** The subscription died: the connection dropped, or the server closed
   *  it (`S2C_KV_CLOSED` — the error message carries
   *  {@link kvClosedText}, e.g. "resource limit" when stalled acks
   *  breached the unacked budget). Either way the `kv_id` and mirror are
   *  dead; recovery is re-`watchKv` (the fs-family rule — the fresh
   *  snapshot is the recovery). */
  onClosed?: (error: Error) => void;
}

/** A live prefix subscription: read `mirror.live`, `close()` to stop. */
export interface KvWatchHandle {
  readonly kvId: number;
  readonly mirror: KvMirror;
  close(): void;
}

/** A fetched value. */
export interface KvFetchResult {
  hash: bigint;
  value: Uint8Array;
}

/** Options for `kvPut` — `writeFile`'s mapping exactly: `ifHash` → CAS,
 *  `create` → create-exclusive, neither → unconditional (`NO_CAS`). */
export interface KvPutOptions {
  ifHash?: bigint;
  create?: boolean;
  durable?: boolean;
}

/**
 * The complete watcher obligation: apply updates, read `live`. One mirror
 * per subscription; a re-established connection means a new `KV_OPEN`, a
 * new `kv_id`, and a fresh mirror (nothing survives, the fs-family rule).
 */
export class KvMirror {
  readonly live = new Map<string, KvEntry>();
  /** True once the initial snapshot is complete (`KV_UPDATE_SNAPSHOT_END`). */
  snapshotDone = false;

  /** Apply one `KV_UPDATE` message (starting at the opcode byte).
   *  Returns the `update_id` to acknowledge, or null if malformed. */
  applyUpdate(msg: Uint8Array): number | null {
    const update = parseKvUpdateMessage(msg);
    if (update == null) return null;
    for (const r of update.records) {
      if (r.kind === "upsert") {
        this.live.set(r.key, {
          hash: r.hash,
          size: r.size,
          mtimeNs: r.mtimeNs,
          value: r.value,
        });
      } else {
        this.live.delete(r.key);
      }
    }
    if (update.flags & KV_UPDATE_SNAPSHOT_END) this.snapshotDone = true;
    return update.updateId;
  }
}
