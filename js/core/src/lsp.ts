/**
 * Language intelligence (docs/design/lsp.md): wire constants, message
 * builders, record codecs, and the client-side mirrors.
 *
 * The server terminates LSP and projects it into blit-native records:
 * per-backend phase/capabilities are *pushed* as whole-snapshot
 * `LSP_STATE` messages ({@link LspStateMirror}), diagnostics are *pushed*
 * as per-file replacement sets against a server-held cache
 * ({@link LspDiagMirror}), and point-in-time answers are *pulled* through
 * the single nonce-correlated `LSP_QUERY` opcode whose `kind` byte
 * selects the operation.
 *
 * Positions are 0-based lines with UTF-8 byte columns in both directions;
 * the server transcodes to each backend's negotiated encoding. All
 * integers little-endian, tightly packed, as everywhere in the protocol.
 */

import { fsCompressLiteral, fsDecompress } from "./fs.js";

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

// -- Opcodes ----------------------------------------------------------------

/** Attach to the workspace containing a path: [0x60][nonce:2][flags:1][diag_latency_ms:2][path_len:2][path:N].
 *  `path` is plain UTF-8 (client-chosen filesystem location, like
 *  `FS_SYNC`); the server walks upward for root markers. */
export const C2S_LSP_OPEN = 0x60;
/** Release an attachment (backends stay warm): [0x61][lsp_id:2] */
export const C2S_LSP_CLOSE = 0x61;
/** Acknowledge a pushed update: [0x62][lsp_id:2][stream:1][update_id:4].
 *  `stream` is {@link LSP_STREAM_STATE} or {@link LSP_STREAM_DIAG}. */
export const C2S_LSP_ACK = 0x62;
/** Point-in-time query: [0x63][nonce:2][lsp_id:2][kind:1][flags:1][line:4][col:4][path_len:2][path:N][arg_len:2][arg:N].
 *  `kind` is one of `LSP_QUERY_*`; `line`/`col` are ignored by the symbol
 *  kinds (for `WS_SYMBOLS` the `line` field is reserved as a future
 *  SymbolKind bitmask filter); `arg` carries the `WS_SYMBOLS` query
 *  string or the `RENAME` new name. */
export const C2S_LSP_QUERY = 0x63;
/** Advisory cancel of an in-flight query: [0x64][nonce:2] */
export const C2S_LSP_CANCEL = 0x64;
/** Enumerate every live backend, daemon-wide: [0x65][nonce:2] */
export const C2S_LSP_SERVERS = 0x65;
/** Shut one backend down by `server_ref`: [0x66][nonce:2][server_ref:2].
 *  A later query respawns it; observability before force. */
export const C2S_LSP_STOP = 0x66;

/** Open outcome: [0x60][nonce:2][lsp_id:2][status:1][flags:1][root_len:2][root:N][detail_len:2][detail:N].
 *  On failure `lsp_id` = {@link LSP_ID_INVALID} and `detail` carries a
 *  diagnostic; on success `root` is the canonical workspace root, escaped. */
export const S2C_LSP_OPENED = 0x60;
/** Whole-state snapshot: [0x61][lsp_id:2][state_id:4][flags:1][records:LZ4].
 *  One `SERVER` record per live backend of the attachment. */
export const S2C_LSP_STATE = 0x61;
/** Diagnostics update: [0x62][lsp_id:2][update_id:4][flags:1][records:LZ4].
 *  Per-file replacement sets; bit 0 {@link LSP_DIAG_FULL} carries the
 *  complete workspace state (drop everything, then apply). */
export const S2C_LSP_DIAG = 0x62;
/** Query response: [0x63][nonce:2][status:1][flags:1][records:LZ4] */
export const S2C_LSP_QUERY = 0x63;
/** Attachment ended server-side: [0x64][lsp_id:2][reason:1] */
export const S2C_LSP_CLOSED = 0x64;
/** Backend enumeration: [0x65][nonce:2][status:1][flags:1][records:LZ4].
 *  `SERVER` records as in `LSP_STATE` plus the escaped root. */
export const S2C_LSP_SERVERS = 0x65;
/** Stop outcome: [0x66][nonce:2][status:1] */
export const S2C_LSP_STOPPED = 0x66;

/** `S2C_HELLO` feature bit: server supports the `LSP_*` message family. */
export const FEATURE_LSP = 1 << 8;

// Unified status table (docs/design/lsp.md "Statuses"): the docs/design/git.md
// codes 0-9 with the same numbers and semantics where they overlap, plus
// WARMING.
export const LSP_STATUS_OK = 0;
/** `lsp_id` unknown or already closed. */
export const LSP_STATUS_UNKNOWN_ID = 1;
/** Path, symbol, or backend does not exist; discovery failures name the
 *  missing binary in the detail field. */
export const LSP_STATUS_NOT_FOUND = 2;
/** The element cannot answer this query (e.g. rename on a non-symbol). */
export const LSP_STATUS_WRONG_TYPE = 3;
export const LSP_STATUS_PERMISSION = 4;
/** Over a size cap; truncation flags cover the paginatable cases. */
export const LSP_STATUS_TOO_LARGE = 5;
/** A budget was exhausted with no way to truncate. */
export const LSP_STATUS_BUDGET = 6;
/** Malformed request (unknown flags, kind, or field combination). */
export const LSP_STATUS_INVALID = 7;
/** Ended by `LSP_CANCEL`. */
export const LSP_STATUS_CANCELLED = 8;
/** Diagnostic in the message's detail field where it has one. */
export const LSP_STATUS_OTHER = 9;
/** The backing server has not finished initialize/indexing; retryable. */
export const LSP_STATUS_WARMING = 10;

/** Human-readable name for an `LSP_STATUS_*` code. */
export function lspStatusText(status: number): string {
  switch (status) {
    case LSP_STATUS_OK:
      return "ok";
    case LSP_STATUS_UNKNOWN_ID:
      return "unknown attachment";
    case LSP_STATUS_NOT_FOUND:
      return "not found";
    case LSP_STATUS_WRONG_TYPE:
      return "wrong type";
    case LSP_STATUS_PERMISSION:
      return "permission denied";
    case LSP_STATUS_TOO_LARGE:
      return "too large";
    case LSP_STATUS_BUDGET:
      return "budget exhausted";
    case LSP_STATUS_INVALID:
      return "invalid request";
    case LSP_STATUS_CANCELLED:
      return "cancelled";
    case LSP_STATUS_WARMING:
      return "warming up";
    default:
      return "error";
  }
}

// C2S_LSP_OPEN flags.

/** Stream `LSP_STATE`. */
export const LSP_OPEN_WATCH = 1 << 0;
/** Stream `LSP_DIAG`; implies `WATCH`. */
export const LSP_OPEN_DIAGS = 1 << 1;

/** `lsp_id` value reporting an open failure. */
export const LSP_ID_INVALID = 0xffff;

// C2S_LSP_ACK streams.

export const LSP_STREAM_STATE = 0;
export const LSP_STREAM_DIAG = 1;

// C2S_LSP_QUERY kinds.

/** → `LOCATION` records. */
export const LSP_QUERY_DEFINITION = 1;
/** → `LOCATION` records; flags bit 0 {@link LSP_REFS_INCLUDE_DECLARATION}. */
export const LSP_QUERY_REFERENCES = 2;
/** → one `MARKUP` record (plus an optional `LOCATION` for the range). */
export const LSP_QUERY_HOVER = 3;
/** → `SYMBOL` records, pre-order; `line`/`col` ignored. */
export const LSP_QUERY_DOC_SYMBOLS = 4;
/** → `SYMBOL` records; `path` empty, `arg` = query string. */
export const LSP_QUERY_WS_SYMBOLS = 5;
/** → `EDIT` records; `arg` = new name. Data, never applied. */
export const LSP_QUERY_RENAME = 6;

// C2S_LSP_QUERY flags.

/** `REFERENCES`: include the declaration itself. */
export const LSP_REFS_INCLUDE_DECLARATION = 1 << 0;

// S2C_LSP_DIAG flags.

/** The update carries complete workspace diagnostic state: drop
 *  everything, then apply. Every `DIAGS` subscribe begins with one (the
 *  cache replay), and the server may send one at any time instead of an
 *  incremental update. */
export const LSP_DIAG_FULL = 1 << 0;

// S2C response flags (query, state, diag, servers).

/** The entries budget was hit; records present are valid. */
export const LSP_RESP_TRUNCATED = 1 << 0;
/** A `RENAME` plan dropped file operations it cannot project (create /
 *  rename / delete of whole files in a `WorkspaceEdit`): the returned
 *  `EDIT` records are the text edits only, so the plan is incomplete. */
export const LSP_RESP_INCOMPLETE = 1 << 1;

// S2C_LSP_CLOSED reasons.

export const LSP_CLOSED_CLIENT_REQUEST = 0;
export const LSP_CLOSED_ROOT_GONE = 1;
export const LSP_CLOSED_PERMISSION_LOST = 2;
export const LSP_CLOSED_BACKEND_FAILED = 3;
export const LSP_CLOSED_RESOURCE_LIMIT = 4;
/** Client-side pseudo-reason: the connection dropped or was re-established.
 *  Attachments do not survive reconnects — re-`openLsp`. */
export const LSP_CLOSED_CONNECTION_LOST = -1;

// SERVER record phases.

export const LSP_PHASE_SPAWNING = 0;
export const LSP_PHASE_INITIALIZING = 1;
export const LSP_PHASE_INDEXING = 2;
export const LSP_PHASE_READY = 3;
export const LSP_PHASE_FAILED = 4;

/** `progress_pct` value when the backend reports no percentage. */
export const LSP_PROGRESS_UNKNOWN = 255;

// SERVER record capability bits (`caps:4`), aligned with query kinds:
// bit `kind - 1`.

export const LSP_CAP_DEFINITION = 1 << 0;
export const LSP_CAP_REFERENCES = 1 << 1;
export const LSP_CAP_HOVER = 1 << 2;
export const LSP_CAP_DOC_SYMBOLS = 1 << 3;
export const LSP_CAP_WS_SYMBOLS = 1 << 4;
export const LSP_CAP_RENAME = 1 << 5;

// DIAG record severities (LSP values).

export const LSP_SEVERITY_ERROR = 1;
export const LSP_SEVERITY_WARNING = 2;
export const LSP_SEVERITY_INFO = 3;
export const LSP_SEVERITY_HINT = 4;

// DIAG record flags (LSP diagnostic tags).

export const LSP_DIAG_UNNECESSARY = 1 << 0;
export const LSP_DIAG_DEPRECATED = 1 << 1;

// MARKUP record formats.

export const LSP_MARKUP_PLAIN = 0;
export const LSP_MARKUP_MARKDOWN = 1;

// SYMBOL record flags.

export const LSP_SYMBOL_DEPRECATED = 1 << 0;

// Record kinds, namespaced per message type.

export const LSP_STATE_RECORD_SERVER = 0x01;
export const LSP_DIAG_RECORD_FILE = 0x01;
export const LSP_DIAG_RECORD_DIAG = 0x02;
export const LSP_QUERY_RECORD_LOCATION = 0x01;
export const LSP_QUERY_RECORD_MARKUP = 0x02;
export const LSP_QUERY_RECORD_SYMBOL = 0x03;
export const LSP_QUERY_RECORD_EDIT = 0x04;

// -- Hashes -----------------------------------------------------------------

/** BLAKE3 truncated to 128 bits, as in the fs family: the content
 *  version a record describes. Always 16 bytes on the wire. */
export type LspHash = Uint8Array;

/** The all-zero hash: content version unknown. */
export const LSP_HASH_NONE: LspHash = new Uint8Array(16);

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

function pushU64(buf: number[], value: bigint): void {
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

/** A u32-length-prefixed UTF-8 string (diagnostic messages, markup, edit
 *  text). */
function pushText(buf: number[], text: string): void {
  const bytes = textEncoder.encode(text);
  pushU32(buf, bytes.length);
  buf.push(...bytes);
}

function pushHash(buf: number[], hash: LspHash): void {
  for (let i = 0; i < 16; i++) buf.push(hash[i] ?? 0);
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
  hash(): LspHash {
    return new Uint8Array(this.bytes(16));
  }
  str(): string {
    return textDecoder.decode(this.bytes(this.u16()));
  }
  /** A u32-length-prefixed UTF-8 string. */
  text(): string {
    return textDecoder.decode(this.bytes(this.u32()));
  }
  rest(): Uint8Array {
    return this.pos < 0 ? new Uint8Array(0) : this.data.subarray(this.pos);
  }
}

// -- Message builders and parsers -------------------------------------------

export function msgLspOpen(
  nonce: number,
  flags: number,
  diagLatencyMs: number,
  path: string,
): Uint8Array {
  const buf: number[] = [C2S_LSP_OPEN];
  pushU16(buf, nonce);
  buf.push(flags);
  pushU16(buf, diagLatencyMs);
  pushStr(buf, path);
  return new Uint8Array(buf);
}

export function msgLspClose(lspId: number): Uint8Array {
  const buf: number[] = [C2S_LSP_CLOSE];
  pushU16(buf, lspId);
  return new Uint8Array(buf);
}

export function msgLspAck(
  lspId: number,
  stream: number,
  updateId: number,
): Uint8Array {
  const buf: number[] = [C2S_LSP_ACK];
  pushU16(buf, lspId);
  buf.push(stream);
  pushU32(buf, updateId);
  return new Uint8Array(buf);
}

export interface LspQueryRequest {
  nonce: number;
  lspId: number;
  /** One of `LSP_QUERY_*`. */
  kind: number;
  flags: number;
  /** 0-based; ignored by the symbol kinds. */
  line: number;
  /** UTF-8 byte offset within the line; ignored by the symbol kinds. */
  col: number;
  /** Escaped form; empty for `WS_SYMBOLS`. */
  path: string;
  /** `WS_SYMBOLS` query string or `RENAME` new name; empty otherwise. */
  arg: string;
}

export function msgLspQuery(req: LspQueryRequest): Uint8Array {
  const buf: number[] = [C2S_LSP_QUERY];
  pushU16(buf, req.nonce);
  pushU16(buf, req.lspId);
  buf.push(req.kind);
  buf.push(req.flags);
  pushU32(buf, req.line);
  pushU32(buf, req.col);
  pushStr(buf, req.path);
  pushStr(buf, req.arg);
  return new Uint8Array(buf);
}

export function msgLspCancel(nonce: number): Uint8Array {
  const buf: number[] = [C2S_LSP_CANCEL];
  pushU16(buf, nonce);
  return new Uint8Array(buf);
}

export function msgLspServers(nonce: number): Uint8Array {
  const buf: number[] = [C2S_LSP_SERVERS];
  pushU16(buf, nonce);
  return new Uint8Array(buf);
}

export function msgLspStop(nonce: number, serverRef: number): Uint8Array {
  const buf: number[] = [C2S_LSP_STOP];
  pushU16(buf, nonce);
  pushU16(buf, serverRef);
  return new Uint8Array(buf);
}

/** A decoded `S2C_LSP_OPENED`. */
export interface LspOpened {
  nonce: number;
  lspId: number;
  status: number;
  flags: number;
  /** Escaped canonical workspace root; empty on failure. */
  root: string;
  /** Diagnostic on failure. */
  detail: string;
}

export function parseLspOpened(msg: Uint8Array): LspOpened | null {
  if (msg.length < 11 || msg[0] !== S2C_LSP_OPENED) return null;
  const c = new Cursor(msg.subarray(1));
  const opened: LspOpened = {
    nonce: c.u16(),
    lspId: c.u16(),
    status: c.u8(),
    flags: c.u8(),
    root: c.str(),
    detail: c.str(),
  };
  return c.ok ? opened : null;
}

/** Parse `S2C_LSP_STATE` into `[lspId, stateId, flags, records]` with the
 *  records decompressed. */
export function parseLspState(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  if (msg.length < 8 || msg[0] !== S2C_LSP_STATE) return null;
  const c = new Cursor(msg.subarray(1));
  const lspId = c.u16();
  const stateId = c.u32();
  const flags = c.u8();
  const records = fsDecompress(c.rest());
  if (!c.ok || records === null) return null;
  return [lspId, stateId, flags, records];
}

/** Parse `S2C_LSP_DIAG` into `[lspId, updateId, flags, records]` with the
 *  records decompressed. */
export function parseLspDiag(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  if (msg.length < 8 || msg[0] !== S2C_LSP_DIAG) return null;
  const c = new Cursor(msg.subarray(1));
  const lspId = c.u16();
  const updateId = c.u32();
  const flags = c.u8();
  const records = fsDecompress(c.rest());
  if (!c.ok || records === null) return null;
  return [lspId, updateId, flags, records];
}

/** Parse query/servers responses: `[nonce, status, flags, records]`. */
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

export function parseLspQueryResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  return parseRecordsResp(msg, S2C_LSP_QUERY);
}

export function parseLspServersResp(
  msg: Uint8Array,
): [number, number, number, Uint8Array] | null {
  return parseRecordsResp(msg, S2C_LSP_SERVERS);
}

/** Parse `S2C_LSP_CLOSED` into `[lspId, reason]`. */
export function parseLspClosed(msg: Uint8Array): [number, number] | null {
  if (msg.length < 4 || msg[0] !== S2C_LSP_CLOSED) return null;
  return [msg[1] | (msg[2] << 8), msg[3]];
}

/** Parse `S2C_LSP_STOPPED` into `[nonce, status]`. */
export function parseLspStopped(msg: Uint8Array): [number, number] | null {
  if (msg.length < 4 || msg[0] !== S2C_LSP_STOPPED) return null;
  return [msg[1] | (msg[2] << 8), msg[3]];
}

// Server-side builders (tests and mock servers).

export function msgLspOpened(
  nonce: number,
  lspId: number,
  status: number,
  flags: number,
  root: string,
  detail: string,
): Uint8Array {
  const buf: number[] = [S2C_LSP_OPENED];
  pushU16(buf, nonce);
  pushU16(buf, lspId);
  buf.push(status);
  buf.push(flags);
  pushStr(buf, root);
  pushStr(buf, detail);
  return new Uint8Array(buf);
}

export function msgLspState(
  lspId: number,
  stateId: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  const buf: number[] = [S2C_LSP_STATE];
  pushU16(buf, lspId);
  pushU32(buf, stateId);
  buf.push(flags);
  const compressed = fsCompressLiteral(records);
  const msg = new Uint8Array(buf.length + compressed.length);
  msg.set(buf, 0);
  msg.set(compressed, buf.length);
  return msg;
}

export function msgLspDiag(
  lspId: number,
  updateId: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  const buf: number[] = [S2C_LSP_DIAG];
  pushU16(buf, lspId);
  pushU32(buf, updateId);
  buf.push(flags);
  const compressed = fsCompressLiteral(records);
  const msg = new Uint8Array(buf.length + compressed.length);
  msg.set(buf, 0);
  msg.set(compressed, buf.length);
  return msg;
}

function msgRecordsResp(
  opcode: number,
  nonce: number,
  status: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  const buf: number[] = [opcode];
  pushU16(buf, nonce);
  buf.push(status);
  buf.push(flags);
  const compressed = fsCompressLiteral(records);
  const msg = new Uint8Array(buf.length + compressed.length);
  msg.set(buf, 0);
  msg.set(compressed, buf.length);
  return msg;
}

export function msgLspQueryResp(
  nonce: number,
  status: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  return msgRecordsResp(S2C_LSP_QUERY, nonce, status, flags, records);
}

export function msgLspServersResp(
  nonce: number,
  status: number,
  flags: number,
  records: Uint8Array,
): Uint8Array {
  return msgRecordsResp(S2C_LSP_SERVERS, nonce, status, flags, records);
}

export function msgLspClosed(lspId: number, reason: number): Uint8Array {
  const buf: number[] = [S2C_LSP_CLOSED];
  pushU16(buf, lspId);
  buf.push(reason);
  return new Uint8Array(buf);
}

export function msgLspStopped(nonce: number, status: number): Uint8Array {
  const buf: number[] = [S2C_LSP_STOPPED];
  pushU16(buf, nonce);
  buf.push(status);
  return new Uint8Array(buf);
}

// -- Records ----------------------------------------------------------------

/** SERVER 0x01: [kind:1][server_ref:2][phase:1][progress_pct:1][caps:4]
 *  [epoch:4][refused_edits:4][rss:8][id_len:2][id:N][msg_len:2][msg:N] */
export type LspStateRecord = {
  kind: "server";
  /** Daemon-scoped backend id (`LSP_STOP` target). */
  serverRef: number;
  /** One of `LSP_PHASE_*`. */
  phase: number;
  /** 0-100, or {@link LSP_PROGRESS_UNKNOWN}. */
  progressPct: number;
  /** `LSP_CAP_*` bits. */
  caps: number;
  /** Increments on dynamic capability (re)registration. */
  epoch: number;
  /** `workspace/applyEdit` requests answered `applied:false`. */
  refusedEdits: number;
  /** Best-effort resident set size in bytes; 0n = unknown. */
  rss: bigint;
  /** Server id from the discovery table (e.g. `rust-analyzer`). */
  id: string;
  /** Last progress or showMessage line. */
  msg: string;
};

/** SERVER 0x01: the `LSP_STATE` layout + [root_len:2][root:N] */
export type LspServersRecord = LspStateRecord & {
  /** Escaped canonical workspace root. */
  root: string;
};

export type LspDiagRecord =
  /** FILE 0x01: [kind:1][hash:16][n:2][path_len:2][path:N].
   *  Replaces the file's entire diagnostic set (the following `n` diag
   *  records); `n` = 0 clears. `hash` names the content version the set
   *  describes; zero when unknown. */
  | { kind: "file"; hash: LspHash; n: number; path: string }
  /** DIAG 0x02: [kind:1][severity:1][flags:1][line:4][col:4][end_line:4][end_col:4]
   *  [code_len:2][code:N][src_len:2][source:N][msg_len:4][msg:N] */
  | {
      kind: "diag";
      /** One of `LSP_SEVERITY_*`. */
      severity: number;
      /** {@link LSP_DIAG_UNNECESSARY} / {@link LSP_DIAG_DEPRECATED}. */
      flags: number;
      line: number;
      col: number;
      endLine: number;
      endCol: number;
      code: string;
      /** The producing backend (e.g. `rust-analyzer`, `clippy`). */
      source: string;
      msg: string;
    };

export type LspQueryRecord =
  /** LOCATION 0x01: [kind:1][flags:1][hash:16][line:4][col:4][end_line:4][end_col:4][path_len:2][path:N] */
  | {
      kind: "location";
      flags: number;
      /** Content version the location refers into; zero when unknown. */
      hash: LspHash;
      line: number;
      col: number;
      endLine: number;
      endCol: number;
      path: string;
    }
  /** MARKUP 0x02: [kind:1][format:1][text_len:4][text:N] */
  | {
      kind: "markup";
      /** {@link LSP_MARKUP_PLAIN} or {@link LSP_MARKUP_MARKDOWN}. */
      format: number;
      text: string;
    }
  /** SYMBOL 0x03: [kind:1][sym_kind:1][flags:1][depth:1][line:4][col:4]
   *  [end_line:4][end_col:4][name_len:2][name:N][path_len:2][path:N].
   *  `depth` nests document outlines (pre-order); 0 at the top level. */
  | {
      kind: "symbol";
      /** LSP SymbolKind value. */
      symKind: number;
      /** {@link LSP_SYMBOL_DEPRECATED}. */
      flags: number;
      depth: number;
      line: number;
      col: number;
      endLine: number;
      endCol: number;
      name: string;
      path: string;
    }
  /** EDIT 0x04: [kind:1][flags:1][hash:16][line:4][col:4][end_line:4][end_col:4]
   *  [new_len:4][new_text:N][path_len:2][path:N].
   *  One ordered edit of a rename plan, against the content version named
   *  by `hash`. Data, never applied. */
  | {
      kind: "edit";
      flags: number;
      hash: LspHash;
      line: number;
      col: number;
      endLine: number;
      endCol: number;
      newText: string;
      path: string;
    };

/**
 * Iterate `[record_len:4][kind:1][…]` records. Unknown kinds are skipped
 * (their decoder returns null without touching the cursor); a malformed
 * record — one whose decoder overran the body — ends the payload, matching
 * the Rust codec (docs/design/lsp.md).
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

/** Decode the shared `SERVER` field layout after the kind byte. */
function decodeServer(c: Cursor): LspStateRecord {
  return {
    kind: "server",
    serverRef: c.u16(),
    phase: c.u8(),
    progressPct: c.u8(),
    caps: c.u32(),
    epoch: c.u32(),
    refusedEdits: c.u32(),
    rss: c.u64(),
    id: c.str(),
    msg: c.str(),
  };
}

/** Iterate records in an uncompressed `LSP_STATE` payload. */
export function lspStateRecords(data: Uint8Array): Generator<LspStateRecord> {
  return records(data, (kind, c) =>
    kind === LSP_STATE_RECORD_SERVER ? decodeServer(c) : null,
  );
}

/** Iterate records in an uncompressed `LSP_SERVERS` payload. */
export function lspServersRecords(
  data: Uint8Array,
): Generator<LspServersRecord> {
  return records(data, (kind, c) => {
    if (kind !== LSP_STATE_RECORD_SERVER) return null;
    return { ...decodeServer(c), root: c.str() };
  });
}

/** Iterate records in an uncompressed `LSP_DIAG` payload. */
export function lspDiagRecords(data: Uint8Array): Generator<LspDiagRecord> {
  return records(data, (kind, c) => {
    switch (kind) {
      case LSP_DIAG_RECORD_FILE:
        return {
          kind: "file",
          hash: c.hash(),
          n: c.u16(),
          path: c.str(),
        } as const;
      case LSP_DIAG_RECORD_DIAG:
        return {
          kind: "diag",
          severity: c.u8(),
          flags: c.u8(),
          line: c.u32(),
          col: c.u32(),
          endLine: c.u32(),
          endCol: c.u32(),
          code: c.str(),
          source: c.str(),
          msg: c.text(),
        } as const;
      default:
        return null;
    }
  });
}

/** Iterate records in an uncompressed `LSP_QUERY` response payload. */
export function lspQueryRecords(data: Uint8Array): Generator<LspQueryRecord> {
  return records(data, (kind, c) => {
    switch (kind) {
      case LSP_QUERY_RECORD_LOCATION:
        return {
          kind: "location",
          flags: c.u8(),
          hash: c.hash(),
          line: c.u32(),
          col: c.u32(),
          endLine: c.u32(),
          endCol: c.u32(),
          path: c.str(),
        } as const;
      case LSP_QUERY_RECORD_MARKUP:
        return { kind: "markup", format: c.u8(), text: c.text() } as const;
      case LSP_QUERY_RECORD_SYMBOL:
        return {
          kind: "symbol",
          symKind: c.u8(),
          flags: c.u8(),
          depth: c.u8(),
          line: c.u32(),
          col: c.u32(),
          endLine: c.u32(),
          endCol: c.u32(),
          name: c.str(),
          path: c.str(),
        } as const;
      case LSP_QUERY_RECORD_EDIT: {
        const flags = c.u8();
        const hash = c.hash();
        const line = c.u32();
        const col = c.u32();
        const endLine = c.u32();
        const endCol = c.u32();
        const newText = c.text();
        const path = c.str();
        return {
          kind: "edit",
          flags,
          hash,
          line,
          col,
          endLine,
          endCol,
          newText,
          path,
        } as const;
      }
      default:
        return null;
    }
  });
}

/** Append the shared `SERVER` field layout after the kind byte. */
function appendServerFields(buf: number[], record: LspStateRecord): void {
  pushU16(buf, record.serverRef);
  buf.push(record.phase, record.progressPct);
  pushU32(buf, record.caps);
  pushU32(buf, record.epoch);
  pushU32(buf, record.refusedEdits);
  pushU64(buf, record.rss);
  pushStr(buf, record.id);
  pushStr(buf, record.msg);
}

function endRecord(buf: number[], start: number): void {
  const len = buf.length - start - 4;
  buf[start] = len & 0xff;
  buf[start + 1] = (len >> 8) & 0xff;
  buf[start + 2] = (len >> 16) & 0xff;
  buf[start + 3] = (len >> 24) & 0xff;
}

/** Append one record to an uncompressed `LSP_STATE` records buffer. */
export function appendLspStateRecord(
  buf: number[],
  record: LspStateRecord,
): void {
  const start = buf.length;
  buf.push(0, 0, 0, 0); // record_len placeholder
  buf.push(LSP_STATE_RECORD_SERVER);
  appendServerFields(buf, record);
  endRecord(buf, start);
}

/** Append one record to an uncompressed `LSP_SERVERS` records buffer. */
export function appendLspServersRecord(
  buf: number[],
  record: LspServersRecord,
): void {
  const start = buf.length;
  buf.push(0, 0, 0, 0);
  buf.push(LSP_STATE_RECORD_SERVER);
  appendServerFields(buf, record);
  pushStr(buf, record.root);
  endRecord(buf, start);
}

/** Append one record to an uncompressed `LSP_DIAG` records buffer. */
export function appendLspDiagRecord(
  buf: number[],
  record: LspDiagRecord,
): void {
  const start = buf.length;
  buf.push(0, 0, 0, 0);
  switch (record.kind) {
    case "file":
      buf.push(LSP_DIAG_RECORD_FILE);
      pushHash(buf, record.hash);
      pushU16(buf, record.n);
      pushStr(buf, record.path);
      break;
    case "diag":
      buf.push(LSP_DIAG_RECORD_DIAG, record.severity, record.flags);
      pushU32(buf, record.line);
      pushU32(buf, record.col);
      pushU32(buf, record.endLine);
      pushU32(buf, record.endCol);
      pushStr(buf, record.code);
      pushStr(buf, record.source);
      pushText(buf, record.msg);
      break;
  }
  endRecord(buf, start);
}

/** Append one record to an uncompressed `LSP_QUERY` response buffer. */
export function appendLspQueryRecord(
  buf: number[],
  record: LspQueryRecord,
): void {
  const start = buf.length;
  buf.push(0, 0, 0, 0);
  switch (record.kind) {
    case "location":
      buf.push(LSP_QUERY_RECORD_LOCATION, record.flags);
      pushHash(buf, record.hash);
      pushU32(buf, record.line);
      pushU32(buf, record.col);
      pushU32(buf, record.endLine);
      pushU32(buf, record.endCol);
      pushStr(buf, record.path);
      break;
    case "markup":
      buf.push(LSP_QUERY_RECORD_MARKUP, record.format);
      pushText(buf, record.text);
      break;
    case "symbol":
      buf.push(
        LSP_QUERY_RECORD_SYMBOL,
        record.symKind,
        record.flags,
        record.depth,
      );
      pushU32(buf, record.line);
      pushU32(buf, record.col);
      pushU32(buf, record.endLine);
      pushU32(buf, record.endCol);
      pushStr(buf, record.name);
      pushStr(buf, record.path);
      break;
    case "edit":
      buf.push(LSP_QUERY_RECORD_EDIT, record.flags);
      pushHash(buf, record.hash);
      pushU32(buf, record.line);
      pushU32(buf, record.col);
      pushU32(buf, record.endLine);
      pushU32(buf, record.endCol);
      pushText(buf, record.newText);
      pushStr(buf, record.path);
      break;
  }
  endRecord(buf, start);
}

// -- Connection-level API shapes --------------------------------------------

export interface LspOpenOptions {
  /** Stream `LSP_STATE` snapshots. */
  watch?: boolean;
  /** Stream `LSP_DIAG` replacement sets (implies watch). */
  diagnostics?: boolean;
  /** Diagnostics settle window in ms; 0 = server default (500). */
  diagLatencyMs?: number;
  /** A state snapshot was applied and acknowledged. */
  onState?: (mirror: LspStateMirror, stateId: number) => void;
  /** A diagnostics update was applied and acknowledged. */
  onDiagnostics?: (mirror: LspDiagMirror, updateId: number) => void;
  /** The attachment ended: an `LSP_CLOSED` reason, or
   *  {@link LSP_CLOSED_CONNECTION_LOST} when the connection dropped. */
  onClosed?: (reason: number) => void;
}

/** One query's outcome. Non-OK statuses resolve (WARMING is retryable);
 *  only connection loss rejects. */
export interface LspQueryResult {
  /** An `LSP_STATUS_*` code; inspect before reading records. */
  status: number;
  /** The entries budget was hit; records present are valid. */
  truncated: boolean;
  /** A `RENAME` plan dropped file operations it cannot project (create /
   *  rename / delete of whole files in a `WorkspaceEdit`): the returned
   *  `EDIT` records are the text edits only, so the plan is incomplete. */
  incomplete: boolean;
  records: LspQueryRecord[];
}

/** A workspace attachment opened by `BlitConnection.openLsp`. */
export interface LspHandle {
  readonly lspId: number;
  /** Escaped canonical workspace root. */
  readonly root: string;
  /** Live backend state; populated when watching. Replaced wholesale per
   *  snapshot. */
  readonly state: LspStateMirror;
  /** Live diagnostics; populated when subscribed. Per-file replacement
   *  sets; absence of a path means unknown, not clean. */
  readonly diags: LspDiagMirror;
  /** Definition of the symbol at a position → `location` records. */
  definition(path: string, line: number, col: number): Promise<LspQueryResult>;
  /** References to the symbol at a position → `location` records. */
  references(
    path: string,
    line: number,
    col: number,
    includeDeclaration?: boolean,
  ): Promise<LspQueryResult>;
  /** Hover at a position → one `markup` record (plus an optional
   *  `location` for the range). */
  hover(path: string, line: number, col: number): Promise<LspQueryResult>;
  /** Document outline → `symbol` records, pre-order. */
  documentSymbols(path: string): Promise<LspQueryResult>;
  /** Workspace-wide symbol search → `symbol` records. */
  workspaceSymbols(query: string): Promise<LspQueryResult>;
  /** Rename plan → ordered `edit` records. Data, never applied. */
  rename(
    path: string,
    line: number,
    col: number,
    newName: string,
  ): Promise<LspQueryResult>;
  /** Release the attachment (backends stay warm); `onClosed` fires when
   *  the server confirms. */
  close(): void;
}

// -- Mirrors ----------------------------------------------------------------

/** One backend's projected state, from a `SERVER` record. */
export interface LspServerState {
  phase: number;
  progressPct: number;
  caps: number;
  epoch: number;
  refusedEdits: number;
  rss: bigint;
  id: string;
  msg: string;
}

/**
 * The complete client obligation for `LSP_STATE`: replace the whole map,
 * acknowledge the returned id.
 */
export class LspStateMirror {
  /** Keyed by `server_ref`. */
  servers = new Map<number, LspServerState>();
  /** The last snapshot's flags. */
  flags = 0;

  /** Apply one `S2C_LSP_STATE` message (starting at the opcode byte),
   *  replacing the whole state. Returns the `state_id` to ack, or null
   *  when malformed. */
  applyState(msg: Uint8Array): number | null {
    const parsed = parseLspState(msg);
    if (parsed === null) return null;
    const [, stateId, flags, recordBytes] = parsed;
    this.servers = new Map();
    this.flags = flags;
    for (const record of lspStateRecords(recordBytes)) {
      this.servers.set(record.serverRef, {
        phase: record.phase,
        progressPct: record.progressPct,
        caps: record.caps,
        epoch: record.epoch,
        refusedEdits: record.refusedEdits,
        rss: record.rss,
        id: record.id,
        msg: record.msg,
      });
    }
    return stateId;
  }
}

/** One diagnostic, owned, from a `diag` record. */
export interface LspDiagnostic {
  severity: number;
  flags: number;
  line: number;
  col: number;
  endLine: number;
  endCol: number;
  code: string;
  source: string;
  msg: string;
}

/** One file's diagnostic set. */
export interface LspFileDiags {
  /** Content version the set describes; zero when unknown. */
  hash: LspHash;
  diags: LspDiagnostic[];
}

/**
 * The complete client obligation for `LSP_DIAG`: apply per-file
 * replacement sets, acknowledge. Absence of a path means unknown, not
 * clean.
 */
export class LspDiagMirror {
  /** Keyed by escaped workspace-relative path. */
  files = new Map<string, LspFileDiags>();

  /** Apply one `S2C_LSP_DIAG` message (starting at the opcode byte).
   *  Returns the `update_id` to ack, or null when malformed. */
  applyDiag(msg: Uint8Array): number | null {
    const parsed = parseLspDiag(msg);
    if (parsed === null) return null;
    const [, updateId, flags, recordBytes] = parsed;
    if (flags & LSP_DIAG_FULL) {
      this.files.clear();
    }
    // `diag` records attach to the most recent `file` record.
    let current: LspFileDiags | null = null;
    for (const record of lspDiagRecords(recordBytes)) {
      if (record.kind === "file") {
        if (record.n === 0) {
          this.files.delete(record.path);
          current = null;
        } else {
          current = { hash: record.hash, diags: [] };
          this.files.set(record.path, current);
        }
      } else if (current !== null) {
        current.diags.push({
          severity: record.severity,
          flags: record.flags,
          line: record.line,
          col: record.col,
          endLine: record.endLine,
          endCol: record.endCol,
          code: record.code,
          source: record.source,
          msg: record.msg,
        });
      }
    }
    return updateId;
  }
}
