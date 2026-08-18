/**
 * Wasmi extensions (`docs/design/extensions.md`): the client half.
 *
 * Enough of the family for a client to say what is installed and to change
 * it — list, run, upload, control. Module identity is the BLAKE3 digest, and
 * the server verifies the bytes it receives against the digest the client
 * named, so a client never has to be trusted to hash correctly (and this one
 * cannot: the browser has no BLAKE3).
 *
 * All integers little-endian, tightly packed, as everywhere in the protocol.
 */

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

/** `S2C_HELLO` feature bit: the extension family. */
export const FEATURE_EXTENSION = 1 << 11;

/** Run or update a definition: [0x90][nonce:2][flags][restart][expect_id:8]
 *  [expect_revision:8][hash:32][name_len:2][name][argc:2][(len:4)(arg)…] */
export const C2S_EXT_RUN = 0x90;
/** Upload a chunk: [0x91][nonce:2][flags][hash:32][offset:8][total:8][data] */
export const C2S_EXT_PUT = 0x91;
/** Lifecycle verb: [0x92][nonce:2][extension_id:8][action] */
export const C2S_EXT_CONTROL = 0x92;

export const S2C_EXT_STATUS = 0x90;
export const S2C_EXT_PUT_STATUS = 0x91;
export const S2C_EXT_INFO = 0x92;

export const EXT_INFO_LIST = 2;

export const EXT_RUN_DETACH = 1 << 0;
export const EXT_RUN_PERSIST = 1 << 1;
export const EXT_RUN_UPDATE = 1 << 2;

export const EXT_FLAG_DETACH = 1 << 0;
export const EXT_FLAG_PERSIST = 1 << 1;
export const EXT_FLAG_ENABLED = 1 << 2;
export const EXT_FLAG_DESIRED_RUNNING = 1 << 3;

export const EXT_RESTART_NEVER = 0;
export const EXT_RESTART_ON_FAILURE = 1;
export const EXT_RESTART_ALWAYS = 2;

// Verbatim from `crates/remote/src/extension.rs`; the server answers an
// action it does not recognise with silence, so a wrong number here looks
// exactly like a server that does not support extensions.
export const EXT_CONTROL_CANCEL = 1;
export const EXT_CONTROL_ATTACH = 2;
export const EXT_CONTROL_UNFOLLOW = 3;
export const EXT_CONTROL_STATUS = 4;
export const EXT_CONTROL_RESTART = 5;
export const EXT_CONTROL_ENABLE = 6;
export const EXT_CONTROL_DISABLE = 7;
export const EXT_CONTROL_REMOVE = 8;
export const EXT_CONTROL_LIST = 9;

export const EXT_PUT_BEGIN = 1 << 0;
export const EXT_PUT_FINAL = 1 << 1;
/** The server already has this object; stop uploading. */
export const EXT_PUT_STATUS_ALREADY_HAVE = 128;

/** Phases, in the order a definition moves through them. */
export const EXT_PHASE_NONE = 0;
export const EXT_PHASE_NEED_OBJECT = 1;
export const EXT_PHASE_QUEUED = 2;
export const EXT_PHASE_STARTING = 3;
export const EXT_PHASE_RUNNING = 4;
export const EXT_PHASE_BACKOFF = 5;
export const EXT_PHASE_STOPPED = 6;
export const EXT_PHASE_BLOCKED = 7;
export const EXT_PHASE_STOPPING = 8;

export const EXT_PHASE_NAMES: Readonly<Record<number, string>> = {
  [EXT_PHASE_NONE]: "none",
  [EXT_PHASE_NEED_OBJECT]: "need-object",
  [EXT_PHASE_QUEUED]: "queued",
  [EXT_PHASE_STARTING]: "starting",
  [EXT_PHASE_RUNNING]: "running",
  [EXT_PHASE_BACKOFF]: "backoff",
  [EXT_PHASE_STOPPED]: "stopped",
  [EXT_PHASE_BLOCKED]: "blocked",
  [EXT_PHASE_STOPPING]: "stopping",
};

/** 64 MiB, and one upload chunk stays inside the frame limit. */
export const EXT_MAX_MODULE = 64 * 1024 * 1024;
export const EXT_UPLOAD_CHUNK = 1024 * 1024;
export const EXT_MAX_NAME = 255;

/** One extension as the server describes it. */
export interface BlitExtensionRecord {
  /** Server-boot-scoped identity; `id:<16 hex>` addresses it. */
  readonly extensionId: bigint;
  readonly definitionRevision: bigint;
  readonly phase: number;
  readonly flags: number;
  readonly restart: number;
  readonly attempt: bigint;
  readonly lastRunningAttempt: bigint;
  readonly nextStartUnixMs: bigint;
  /** BLAKE3 of the module, lowercase hex: the thing a registry can match. */
  readonly hash: string;
  /** Durable name for a persistent definition, a label otherwise. */
  readonly name: string;
}

export interface BlitExtensionStatus {
  readonly nonce: number;
  readonly status: number;
  readonly phase: number;
  readonly flags: number;
  readonly restart: number;
  readonly extensionId: bigint;
  readonly definitionRevision: bigint;
  readonly hash: string;
  readonly detail: string;
}

export interface BlitExtensionPutStatus {
  readonly nonce: number;
  readonly status: number;
  readonly hash: string;
  readonly received: bigint;
  readonly detail: string;
}

export type ExtensionMessage =
  | { kind: "status"; status: BlitExtensionStatus }
  | { kind: "put-status"; status: BlitExtensionPutStatus }
  | {
      kind: "list";
      nonce: number;
      status: number;
      records: BlitExtensionRecord[];
    };

export interface ExtensionRunRequest {
  nonce: number;
  flags: number;
  restart: number;
  /** CAS token for an update; zero when creating. */
  expectedExtensionId?: bigint;
  expectedDefinitionRevision?: bigint;
  /** 32 bytes. */
  hash: Uint8Array;
  name: string;
  args?: readonly string[];
}

function hex(bytes: Uint8Array): string {
  let out = "";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}

/** Parse a 64-character BLAKE3 digest. Returns null on anything else. */
export function parseModuleDigest(text: string): Uint8Array | null {
  const trimmed = text.trim().toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(trimmed)) return null;
  const bytes = new Uint8Array(32);
  for (let index = 0; index < 32; index++) {
    bytes[index] = Number.parseInt(trimmed.slice(index * 2, index * 2 + 2), 16);
  }
  return bytes;
}

export function buildExtensionRunMessage(
  request: ExtensionRunRequest,
): Uint8Array {
  if (request.hash.length !== 32) {
    throw new RangeError("module hash must be 32 bytes");
  }
  const name = textEncoder.encode(request.name);
  if (name.length > EXT_MAX_NAME) {
    throw new RangeError("extension name must be at most 255 bytes");
  }
  const args = (request.args ?? []).map((arg) => textEncoder.encode(arg));
  const length =
    1 +
    2 +
    1 +
    1 +
    8 +
    8 +
    32 +
    2 +
    name.length +
    2 +
    args.reduce((total, arg) => total + 4 + arg.length, 0);
  const message = new Uint8Array(length);
  const view = new DataView(message.buffer);
  message[0] = C2S_EXT_RUN;
  view.setUint16(1, request.nonce, true);
  message[3] = request.flags;
  message[4] = request.restart;
  view.setBigUint64(5, request.expectedExtensionId ?? 0n, true);
  view.setBigUint64(13, request.expectedDefinitionRevision ?? 0n, true);
  message.set(request.hash, 21);
  view.setUint16(53, name.length, true);
  message.set(name, 55);
  let offset = 55 + name.length;
  view.setUint16(offset, args.length, true);
  offset += 2;
  for (const arg of args) {
    view.setUint32(offset, arg.length, true);
    offset += 4;
    message.set(arg, offset);
    offset += arg.length;
  }
  return message;
}

export function buildExtensionPutMessage(
  nonce: number,
  flags: number,
  hash: Uint8Array,
  offset: bigint,
  totalSize: bigint,
  data: Uint8Array,
): Uint8Array {
  if (hash.length !== 32) throw new RangeError("module hash must be 32 bytes");
  const message = new Uint8Array(1 + 2 + 1 + 32 + 8 + 8 + data.length);
  const view = new DataView(message.buffer);
  message[0] = C2S_EXT_PUT;
  view.setUint16(1, nonce, true);
  message[3] = flags;
  message.set(hash, 4);
  view.setBigUint64(36, offset, true);
  view.setBigUint64(44, totalSize, true);
  message.set(data, 52);
  return message;
}

export function buildExtensionControlMessage(
  nonce: number,
  extensionId: bigint,
  action: number,
): Uint8Array {
  // The server rejects the pair that cannot mean anything: a list with an id,
  // or a verb without one.
  if ((action === EXT_CONTROL_LIST) !== (extensionId === 0n)) {
    throw new RangeError(
      "list takes no extension id; every other action needs one",
    );
  }
  const message = new Uint8Array(12);
  const view = new DataView(message.buffer);
  message[0] = C2S_EXT_CONTROL;
  view.setUint16(1, nonce, true);
  view.setBigUint64(3, extensionId, true);
  message[11] = action;
  return message;
}

function decodeDetail(bytes: Uint8Array): string {
  try {
    return textDecoder.decode(bytes);
  } catch {
    return "";
  }
}

/** Decode one server-to-client extension packet, or null if it is not one. */
export function parseExtensionMessage(
  bytes: Uint8Array,
): ExtensionMessage | null {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (bytes[0] === S2C_EXT_STATUS) {
    if (bytes.length < 99) return null;
    return {
      kind: "status",
      status: {
        nonce: view.getUint16(1, true),
        status: bytes[3],
        phase: bytes[4],
        flags: bytes[5],
        restart: bytes[6],
        extensionId: view.getBigUint64(7, true),
        definitionRevision: view.getBigUint64(15, true),
        hash: hex(bytes.subarray(67, 99)),
        detail: decodeDetail(bytes.subarray(99)),
      },
    };
  }
  if (bytes[0] === S2C_EXT_PUT_STATUS) {
    if (bytes.length < 44) return null;
    return {
      kind: "put-status",
      status: {
        nonce: view.getUint16(1, true),
        status: bytes[3],
        hash: hex(bytes.subarray(4, 36)),
        received: view.getBigUint64(36, true),
        detail: decodeDetail(bytes.subarray(44)),
      },
    };
  }
  if (bytes[0] !== S2C_EXT_INFO || bytes[1] !== EXT_INFO_LIST) return null;
  if (bytes.length < 7) return null;
  const nonce = view.getUint16(2, true);
  const status = bytes[4];
  const count = view.getUint16(5, true);
  const records: BlitExtensionRecord[] = [];
  let offset = 7;
  for (let index = 0; index < count; index++) {
    // 8+8+1+1+1+8+8+4+8+8+32+2 fixed bytes, then the name.
    if (bytes.length < offset + 89) return null;
    const nameLength = view.getUint16(offset + 87, true);
    if (bytes.length < offset + 89 + nameLength) return null;
    let name: string;
    try {
      name = textDecoder.decode(
        bytes.subarray(offset + 89, offset + 89 + nameLength),
      );
    } catch {
      return null;
    }
    records.push({
      extensionId: view.getBigUint64(offset, true),
      definitionRevision: view.getBigUint64(offset + 8, true),
      phase: bytes[offset + 16],
      flags: bytes[offset + 17],
      restart: bytes[offset + 18],
      attempt: view.getBigUint64(offset + 19, true),
      lastRunningAttempt: view.getBigUint64(offset + 27, true),
      // 35 is task_id (u32) and 39 output_sequence (u64); the next start is
      // what a viewer needs, to explain a definition sitting in backoff.
      nextStartUnixMs: view.getBigUint64(offset + 47, true),
      hash: hex(bytes.subarray(offset + 55, offset + 87)),
      name,
    });
    offset += 89 + nameLength;
  }
  return { kind: "list", nonce, status, records };
}

/** `id:<16 hex>`, the form `blit ext` prints and accepts. */
export function formatExtensionId(extensionId: bigint): string {
  return extensionId.toString(16).padStart(16, "0");
}
