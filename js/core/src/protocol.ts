import {
  C2S_ACK,
  C2S_CLIENT_METRICS,
  C2S_CLIENT_LIST,
  CLIENT_LIST_WANT_ORIGIN,
  C2S_CLIENT_WATCH,
  C2S_CLIENT_UNWATCH,
  C2S_CLIPBOARD_GET,
  C2S_CLIPBOARD_LIST,
  C2S_CLIPBOARD_SET,
  C2S_PRIMARY_SET,
  C2S_DISPLAY_RATE,
  C2S_INPUT,
  C2S_KILL,
  C2S_KICK,
  KILL_LEADER_ONLY,
  C2S_TERM_CWD,
  S2C_TERM_CWD,
  S2C_TERM_CWD_EVENT,
  C2S_MOUSE,
  C2S_RESTART,
  C2S_RESIZE,
  C2S_SCROLL,
  C2S_SCROLL_BY,
  C2S_FOCUS,
  C2S_CLOSE,
  C2S_SUBSCRIBE,
  C2S_UNSUBSCRIBE,
  C2S_SEARCH,
  C2S_COPY_RANGE,
  C2S_CREATE2,
  C2S_SURFACE_INPUT,
  C2S_SURFACE_POINTER,
  C2S_SURFACE_POINTER_AXIS,
  C2S_SURFACE_POINTER_AXIS2,
  AXIS_FLAG_SOURCE_KNOWN,
  AXIS_FLAG_STOP,
  C2S_SURFACE_RESIZE,
  C2S_SURFACE_FOCUS,
  C2S_SURFACE_SUBSCRIBE,
  C2S_SURFACE_UNSUBSCRIBE,
  C2S_SURFACE_ACK,
  C2S_SURFACE_CLOSE,
  C2S_CLIENT_FEATURES,
  CLIENT_FEATURE_SURFACE_TIMESTAMP_SUB_US,
  C2S_SURFACE_PREEDIT,
  C2S_SURFACE_TEXT,
  C2S_SURFACE_DRAG_ENTER,
  C2S_SURFACE_DRAG_MOTION,
  C2S_SURFACE_DRAG_LEAVE,
  C2S_SURFACE_DRAG_DROP,
  C2S_SURFACE_DRAG_CANCEL,
  C2S_SURFACE_TOUCH,
  C2S_AUDIO_SUBSCRIBE,
  C2S_AUDIO_UNSUBSCRIBE,
  CREATE2_HAS_SRC_PTY,
  CREATE2_HAS_COMMAND,
  CREATE2_HAS_CWD,
  CREATE2_WANT_STATUS,
  CREATE2_HAS_DEADLINE,
  CREATE2_HAS_ENV,
  CREATE2_HAS_ARGV,
} from "./types";

const textEncoder = new TextEncoder();

type ResizeEntry = {
  ptyId: number;
  rows: number;
  cols: number;
};

const UNSET_VIEW_SIZE = 0;

export function buildAckMessage(): Uint8Array {
  return new Uint8Array([C2S_ACK]);
}

/**
 * Request the server's connection catalog.
 *
 * `wantOrigin` adds the flags byte that asks for `S2C_CLIENT_LIST2`, whose
 * entries say where each connection came from. Only send it to a server
 * advertising `FEATURE_CLIENT_ORIGIN`: every server answers a client-control
 * request with unexpected trailing bytes with `INVALID`, so asking blindly
 * costs the catalog rather than the extra field.
 */
export function buildClientListMessage(
  nonce: number,
  wantOrigin = false,
): Uint8Array {
  return buildClientCatalogRequest(C2S_CLIENT_LIST, nonce, wantOrigin);
}

/** Start streaming the server's connection catalog. */
export function buildClientWatchMessage(
  nonce: number,
  wantOrigin = false,
): Uint8Array {
  return buildClientCatalogRequest(C2S_CLIENT_WATCH, nonce, wantOrigin);
}

function buildClientCatalogRequest(
  opcode: number,
  nonce: number,
  wantOrigin: boolean,
): Uint8Array {
  const msg = new Uint8Array(wantOrigin ? 4 : 3);
  msg[0] = opcode;
  new DataView(msg.buffer).setUint16(1, nonce, true);
  if (wantOrigin) msg[3] = CLIENT_LIST_WANT_ORIGIN;
  return msg;
}

/** Stop a connection-catalog stream. */
export function buildClientUnwatchMessage(nonce: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_CLIENT_UNWATCH;
  new DataView(msg.buffer).setUint16(1, nonce, true);
  return msg;
}

/** Longest UTF-8 kick reason the server will accept (`KICK_REASON_MAX`). */
export const KICK_REASON_MAX = 1024;

/** UTF-8 byte length of a kick reason, for validating before sending. */
export function kickReasonByteLength(reason: string): number {
  return textEncoder.encode(reason).length;
}

/** Request that another server connection be disconnected. */
export function buildKickClientMessage(
  nonce: number,
  clientId: bigint,
  reason = "",
): Uint8Array {
  // Clamping here is a backstop — callers validate with kickReasonByteLength
  // and refuse, rather than silently sending a shortened reason. encodeInto
  // never writes a partial UTF-8 scalar, so a clamped tail stays valid.
  const reasonBuffer = new Uint8Array(KICK_REASON_MAX);
  const { written } = textEncoder.encodeInto(reason, reasonBuffer);
  const msg = new Uint8Array(11 + written);
  msg[0] = C2S_KICK;
  const view = new DataView(msg.buffer);
  view.setUint16(1, nonce, true);
  view.setBigUint64(3, clientId, true);
  msg.set(reasonBuffer.subarray(0, written), 11);
  return msg;
}

export function buildClientMetricsMessage(
  backlogFrames: number,
  ackAheadFrames: number,
  applyMsX10: number,
): Uint8Array {
  const msg = new Uint8Array(7);
  msg[0] = C2S_CLIENT_METRICS;
  msg[1] = backlogFrames & 0xff;
  msg[2] = (backlogFrames >> 8) & 0xff;
  msg[3] = ackAheadFrames & 0xff;
  msg[4] = (ackAheadFrames >> 8) & 0xff;
  msg[5] = applyMsX10 & 0xff;
  msg[6] = (applyMsX10 >> 8) & 0xff;
  return msg;
}

export function buildDisplayRateMessage(fps: number): Uint8Array {
  // Keep the original integer field so this client remains compatible with
  // older servers, then append the unsnapped millihertz measurement for new
  // servers. At 240 Hz, rounding to a whole fps throws away more than four
  // milliseconds of source time per second.
  const clamped = Math.max(0, Math.min(65_535, fps));
  const msg = new Uint8Array(7);
  msg[0] = C2S_DISPLAY_RATE;
  const view = new DataView(msg.buffer);
  view.setUint16(1, Math.round(clamped), true);
  view.setUint32(3, Math.round(clamped * 1_000), true);
  return msg;
}

export function buildInputMessage(ptyId: number, data: Uint8Array): Uint8Array {
  const msg = new Uint8Array(3 + data.length);
  msg[0] = C2S_INPUT;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  msg.set(data, 3);
  return msg;
}

export function buildResizeMessage(
  ptyId: number,
  rows: number,
  cols: number,
): Uint8Array {
  return buildResizeBatchMessage([{ ptyId, rows, cols }]);
}

export function buildResizeBatchMessage(
  entries: ReadonlyArray<ResizeEntry>,
): Uint8Array {
  const msg = new Uint8Array(1 + entries.length * 6);
  msg[0] = C2S_RESIZE;
  let offset = 1;
  for (const entry of entries) {
    msg[offset] = entry.ptyId & 0xff;
    msg[offset + 1] = (entry.ptyId >> 8) & 0xff;
    msg[offset + 2] = entry.rows & 0xff;
    msg[offset + 3] = (entry.rows >> 8) & 0xff;
    msg[offset + 4] = entry.cols & 0xff;
    msg[offset + 5] = (entry.cols >> 8) & 0xff;
    offset += 6;
  }
  return msg;
}

export function buildClearResizeMessage(ptyId: number): Uint8Array {
  return buildResizeBatchMessage([
    { ptyId, rows: UNSET_VIEW_SIZE, cols: UNSET_VIEW_SIZE },
  ]);
}

export function buildClearResizeBatchMessage(
  ptyIds: ReadonlyArray<number>,
): Uint8Array {
  return buildResizeBatchMessage(
    ptyIds.map((ptyId) => ({
      ptyId,
      rows: UNSET_VIEW_SIZE,
      cols: UNSET_VIEW_SIZE,
    })),
  );
}

/** Move a scrolled view by `lines` from wherever the server holds it
 *  (negative = back toward the live bottom).  See {@link C2S_SCROLL_BY}. */
export function buildScrollByMessage(ptyId: number, lines: number): Uint8Array {
  const msg = new Uint8Array(7);
  const delta = Math.trunc(lines) | 0;
  msg[0] = C2S_SCROLL_BY;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  msg[3] = delta & 0xff;
  msg[4] = (delta >> 8) & 0xff;
  msg[5] = (delta >> 16) & 0xff;
  msg[6] = (delta >> 24) & 0xff;
  return msg;
}

export function buildScrollMessage(ptyId: number, offset: number): Uint8Array {
  const msg = new Uint8Array(7);
  msg[0] = C2S_SCROLL;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  msg[3] = offset & 0xff;
  msg[4] = (offset >> 8) & 0xff;
  msg[5] = (offset >> 16) & 0xff;
  msg[6] = (offset >> 24) & 0xff;
  return msg;
}

export function buildFocusMessage(ptyId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_FOCUS;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  return msg;
}

export function buildCloseMessage(ptyId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_CLOSE;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  return msg;
}

export function buildSubscribeMessage(ptyId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SUBSCRIBE;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  return msg;
}

export function buildUnsubscribeMessage(ptyId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_UNSUBSCRIBE;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  return msg;
}

export function buildSearchMessage(
  requestId: number,
  query: string,
): Uint8Array {
  const queryBytes = textEncoder.encode(query);
  const msg = new Uint8Array(3 + queryBytes.length);
  msg[0] = C2S_SEARCH;
  msg[1] = requestId & 0xff;
  msg[2] = (requestId >> 8) & 0xff;
  msg.set(queryBytes, 3);
  return msg;
}

export type Create2Options = {
  tag?: string;
  /** Run this through the server's login shell. Mutually exclusive with
   *  {@link argv}. */
  command?: string;
  /** Exec this argv directly, no shell. Only pass it when the server
   *  advertised `FEATURE_CREATE_EXEC`; an older one ignores the flag and
   *  spawns a plain interactive shell instead of what was asked for. */
  argv?: readonly string[];
  srcPtyId?: number;
  cwd?: string;
  /** Environment overrides, applied on top of everything the server derives.
   *  Only pass this when the server advertised `FEATURE_CREATE_EXEC`. */
  env?:
    | Readonly<Record<string, string>>
    | readonly (readonly [string, string])[];
  /** Only pass this when the server advertised `FEATURE_PTY_DEADLINE`. */
  deadlineMs?: number;
  /** Only pass this when the server advertised `FEATURE_CREATE_STATUS`. */
  wantStatus?: boolean;
};

/** Encode a `C2S_CREATE2`.
 *
 *  Field order is load-bearing and matches the server's parser: tag,
 *  `src_pty_id`, cwd, deadline, env, argv, then the command — which has no
 *  length prefix and therefore has to be last.
 *
 *  Every optional field past the cwd needs its feature bit negotiated first.
 *  An older server does not reject an unknown `features` bit; it ignores the
 *  bit, does not skip the bytes, and reads them as the start of the command.
 *  See the constants in `types.ts` for what each one does when unsupported. */
export function buildCreate2Message(
  nonce: number,
  rows: number,
  cols: number,
  options?: Create2Options,
): Uint8Array {
  const tagBytes = options?.tag
    ? textEncoder.encode(options.tag)
    : new Uint8Array(0);
  let features = 0;
  const hasSrc = options?.srcPtyId != null;
  const cwdText = options?.cwd?.trim() ?? "";
  const rawCwdBytes = cwdText.length > 0 ? textEncoder.encode(cwdText) : null;
  const cwdBytes = rawCwdBytes
    ? rawCwdBytes.subarray(0, Math.min(rawCwdBytes.length, 0xffff))
    : new Uint8Array(0);
  const hasCwd = cwdBytes.length > 0;
  const argv = options?.argv ?? null;
  if (argv && options?.command) {
    throw new Error("buildCreate2Message: argv and command are exclusive");
  }
  if (argv && argv.length === 0) {
    throw new Error("buildCreate2Message: argv is empty");
  }
  const argvBytes = argv?.map((arg) => textEncoder.encode(arg)) ?? null;
  const env = normalizeEnv(options?.env);
  const envBytes = env.map(
    ([key, value]) =>
      [textEncoder.encode(key), textEncoder.encode(value)] as const,
  );
  const deadlineMs = options?.deadlineMs;
  const hasDeadline = deadlineMs != null && deadlineMs > 0;
  const cmdText = options?.command?.trim() ?? "";
  const hasCmd = cmdText.length > 0;
  if (hasSrc) features |= CREATE2_HAS_SRC_PTY;
  if (hasCwd) features |= CREATE2_HAS_CWD;
  if (hasDeadline) features |= CREATE2_HAS_DEADLINE;
  if (envBytes.length) features |= CREATE2_HAS_ENV;
  if (argvBytes) features |= CREATE2_HAS_ARGV;
  if (hasCmd) features |= CREATE2_HAS_COMMAND;
  if (options?.wantStatus) features |= CREATE2_WANT_STATUS;
  const cmdBytes = hasCmd ? textEncoder.encode(cmdText) : new Uint8Array(0);
  const msg = new Uint8Array(
    10 +
      tagBytes.length +
      (hasSrc ? 2 : 0) +
      (hasCwd ? 2 + cwdBytes.length : 0) +
      (hasDeadline ? 4 : 0) +
      (envBytes.length
        ? 2 + envBytes.reduce((n, [k, v]) => n + 2 + k.length + 4 + v.length, 0)
        : 0) +
      (argvBytes
        ? 2 + argvBytes.reduce((n, arg) => n + 4 + arg.length, 0)
        : 0) +
      cmdBytes.length,
  );
  msg[0] = C2S_CREATE2;
  msg[1] = nonce & 0xff;
  msg[2] = (nonce >> 8) & 0xff;
  msg[3] = rows & 0xff;
  msg[4] = (rows >> 8) & 0xff;
  msg[5] = cols & 0xff;
  msg[6] = (cols >> 8) & 0xff;
  msg[7] = features;
  msg[8] = tagBytes.length & 0xff;
  msg[9] = (tagBytes.length >> 8) & 0xff;
  let cursor = 10;
  if (tagBytes.length) {
    msg.set(tagBytes, cursor);
    cursor += tagBytes.length;
  }
  if (hasSrc) {
    msg[cursor] = options!.srcPtyId! & 0xff;
    msg[cursor + 1] = (options!.srcPtyId! >> 8) & 0xff;
    cursor += 2;
  }
  if (hasCwd) {
    msg[cursor] = cwdBytes.length & 0xff;
    msg[cursor + 1] = (cwdBytes.length >> 8) & 0xff;
    cursor += 2;
    msg.set(cwdBytes, cursor);
    cursor += cwdBytes.length;
  }
  if (hasDeadline) {
    const ms = Math.min(deadlineMs!, 0xffffffff) >>> 0;
    msg[cursor] = ms & 0xff;
    msg[cursor + 1] = (ms >>> 8) & 0xff;
    msg[cursor + 2] = (ms >>> 16) & 0xff;
    msg[cursor + 3] = (ms >>> 24) & 0xff;
    cursor += 4;
  }
  if (envBytes.length) {
    msg[cursor] = envBytes.length & 0xff;
    msg[cursor + 1] = (envBytes.length >> 8) & 0xff;
    cursor += 2;
    for (const [key, value] of envBytes) {
      msg[cursor] = key.length & 0xff;
      msg[cursor + 1] = (key.length >> 8) & 0xff;
      cursor += 2;
      msg.set(key, cursor);
      cursor += key.length;
      msg[cursor] = value.length & 0xff;
      msg[cursor + 1] = (value.length >>> 8) & 0xff;
      msg[cursor + 2] = (value.length >>> 16) & 0xff;
      msg[cursor + 3] = (value.length >>> 24) & 0xff;
      cursor += 4;
      msg.set(value, cursor);
      cursor += value.length;
    }
  }
  if (argvBytes) {
    msg[cursor] = argvBytes.length & 0xff;
    msg[cursor + 1] = (argvBytes.length >> 8) & 0xff;
    cursor += 2;
    for (const arg of argvBytes) {
      msg[cursor] = arg.length & 0xff;
      msg[cursor + 1] = (arg.length >>> 8) & 0xff;
      msg[cursor + 2] = (arg.length >>> 16) & 0xff;
      msg[cursor + 3] = (arg.length >>> 24) & 0xff;
      cursor += 4;
      msg.set(arg, cursor);
      cursor += arg.length;
    }
  }
  if (cmdBytes.length) msg.set(cmdBytes, cursor);
  return msg;
}

/** Accept either an object or entry pairs, and reject what the server would.
 *  A key carrying `=` or a NUL cannot survive `execve`, and a duplicate has no
 *  resolution that does not silently discard a value. */
function normalizeEnv(
  env: Create2Options["env"],
): (readonly [string, string])[] {
  if (!env) return [];
  const entries = Array.isArray(env)
    ? (env as (readonly [string, string])[])
    : Object.entries(env as Record<string, string>);
  const seen = new Set<string>();
  for (const [key, value] of entries) {
    if (!key || key.includes("=") || key.includes("\0")) {
      throw new Error(
        `buildCreate2Message: bad environment key ${JSON.stringify(key)}`,
      );
    }
    if (value.includes("\0")) {
      throw new Error(`buildCreate2Message: NUL in value for ${key}`);
    }
    if (seen.has(key)) {
      throw new Error(`buildCreate2Message: duplicate environment key ${key}`);
    }
    seen.add(key);
  }
  return entries;
}

/** Mouse event types for C2S_MOUSE. */
export const MOUSE_DOWN = 0;
export const MOUSE_UP = 1;
export const MOUSE_MOVE = 2;

export function buildMouseMessage(
  ptyId: number,
  type: number,
  button: number,
  col: number,
  row: number,
): Uint8Array {
  const msg = new Uint8Array(9);
  msg[0] = C2S_MOUSE;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  msg[3] = type;
  msg[4] = button;
  msg[5] = col & 0xff;
  msg[6] = (col >> 8) & 0xff;
  msg[7] = row & 0xff;
  msg[8] = (row >> 8) & 0xff;
  return msg;
}

export function buildRestartMessage(ptyId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_RESTART;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  return msg;
}

/**
 * Signals a terminal. The server's default reaches the child's process group,
 * which is what "stop this terminal" means and what the kernel does for a
 * real `^C`. Pass `leaderOnly` to address the session leader alone; that
 * needs `FEATURE_KILL_MODE`, and an older server is leader-only regardless
 * because it ignores the trailing byte.
 */
export function buildKillMessage(
  ptyId: number,
  signal: number,
  leaderOnly = false,
): Uint8Array {
  const msg = new Uint8Array(leaderOnly ? 8 : 7);
  msg[0] = C2S_KILL;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  const view = new DataView(msg.buffer);
  view.setInt32(3, signal, true);
  if (leaderOnly) msg[7] = KILL_LEADER_ONLY;
  return msg;
}

/** [0x1C][nonce:2][pty_id:2] — request a pty's live cwd. */
export function buildTermCwdMessage(nonce: number, ptyId: number): Uint8Array {
  const msg = new Uint8Array(5);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_TERM_CWD;
  v.setUint16(1, nonce, true);
  v.setUint16(3, ptyId, true);
  return msg;
}

/** [0x0E][nonce:2][cwd_len:2][cwd:N] → { nonce, cwd } (empty cwd = unknown). */
export function parseTermCwdReply(
  data: Uint8Array,
): { nonce: number; cwd: string } | null {
  if (data.length < 5 || data[0] !== S2C_TERM_CWD) return null;
  const v = new DataView(data.buffer, data.byteOffset, data.byteLength);
  const nonce = v.getUint16(1, true);
  const len = v.getUint16(3, true);
  if (data.length < 5 + len) return null;
  return { nonce, cwd: new TextDecoder().decode(data.subarray(5, 5 + len)) };
}

/** The server-enforced cap on a pushed cwd (docs/protocol.md
 *  `TERM_CWD_MAX`), mirrored so a hostile frame cannot mint an
 *  unbounded string. */
const TERM_CWD_MAX = 4096;

/** [0x0F][pty_id:2][cwd:N] → { ptyId, cwd } — unsolicited push when the
 *  OSC 7-reported cwd changes (docs/protocol.md `TERM_CWD_EVENT`). `cwd`
 *  is the remainder of the message, no length prefix (the S2C_TITLE
 *  convention): a non-empty UTF-8 absolute path of at most 4096 bytes.
 *  Null = malformed. */
export function parseTermCwdEvent(
  data: Uint8Array,
): { ptyId: number; cwd: string } | null {
  if (data.length < 4 || data[0] !== S2C_TERM_CWD_EVENT) return null;
  const cwdBytes = data.subarray(3);
  if (cwdBytes.length > TERM_CWD_MAX) return null;
  let cwd: string;
  try {
    cwd = new TextDecoder("utf-8", { fatal: true }).decode(cwdBytes);
  } catch {
    return null;
  }
  return { ptyId: data[1] | (data[2] << 8), cwd };
}

export function buildCopyRangeMessage(
  nonce: number,
  ptyId: number,
  startTail: number,
  startCol: number,
  endTail: number,
  endCol: number,
): Uint8Array {
  const msg = new Uint8Array(18);
  const v = new DataView(msg.buffer);
  msg[0] = C2S_COPY_RANGE;
  v.setUint16(1, nonce, true);
  v.setUint16(3, ptyId, true);
  v.setUint32(5, startTail, true);
  v.setUint16(9, startCol, true);
  v.setUint32(11, endTail, true);
  v.setUint16(15, endCol, true);
  msg[17] = 0;
  return msg;
}

export function buildSurfaceInputMessage(
  surfaceId: number,
  keycode: number,
  pressed: boolean,
  timeMs = 0,
): Uint8Array {
  const msg = new Uint8Array(12);
  msg[0] = C2S_SURFACE_INPUT;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  const v = new DataView(msg.buffer);
  v.setUint32(3, keycode, true);
  msg[7] = pressed ? 1 : 0;
  // The browser event's own time, so every input path is paced by one clock
  // rather than by whenever the compositor drained its queue.
  v.setUint32(8, Math.max(0, Math.round(timeMs)) >>> 0, true);
  return msg;
}

export function buildSurfaceTextMessage(
  surfaceId: number,
  text: string,
): Uint8Array {
  const encoded = textEncoder.encode(text);
  const msg = new Uint8Array(3 + encoded.length);
  msg[0] = C2S_SURFACE_TEXT;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg.set(encoded, 3);
  return msg;
}

/**
 * Composition in progress: [0x34][surface_id:2][cursor:2][text:N].
 *
 * `cursorUtf16` is a UTF-16 offset (what the DOM counts in) and goes on the
 * wire as a byte offset, which is what zwp_text_input_v3 wants — the two
 * disagree for exactly the characters a composition is usually made of.
 */
export function buildSurfacePreeditMessage(
  surfaceId: number,
  text: string,
  cursorUtf16: number,
): Uint8Array {
  const encoded = textEncoder.encode(text);
  const cursor = textEncoder.encode(
    text.slice(0, Math.max(0, cursorUtf16)),
  ).length;
  const msg = new Uint8Array(5 + encoded.length);
  msg[0] = C2S_SURFACE_PREEDIT;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = cursor & 0xff;
  msg[4] = (cursor >> 8) & 0xff;
  msg.set(encoded, 5);
  return msg;
}

export const SURFACE_POINTER_DOWN = 0;
export const SURFACE_POINTER_UP = 1;
export const SURFACE_POINTER_MOVE = 2;
/** The pointer left the surface's drawn area. Carries no position, and older
 *  servers ignore the type, so it needs no feature bit: they simply keep the
 *  pre-existing behaviour of never retiring the shared-pointer overlay. */
export const SURFACE_POINTER_LEAVE = 3;

export function buildSurfacePointerMessage(
  surfaceId: number,
  type: number,
  button: number,
  x: number,
  y: number,
  timeMs = 0,
): Uint8Array {
  const msg = new Uint8Array(13);
  msg[0] = C2S_SURFACE_POINTER;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = type;
  msg[4] = button;
  msg[5] = x & 0xff;
  msg[6] = (x >> 8) & 0xff;
  msg[7] = y & 0xff;
  msg[8] = (y >> 8) & 0xff;
  // The browser event's own time. Anything that differentiates pointer motion
  // — a gesture recogniser, a stroke width, a drag-throw — needs the browser's
  // spacing, not the instant the compositor drained its command queue.
  new DataView(msg.buffer).setUint32(
    9,
    Math.max(0, Math.round(timeMs)) >>> 0,
    true,
  );
  return msg;
}

/** One dropped payload: the bytes, the MIME they are offered under, and
 *  the file name when the drag source had one. */
export interface SurfaceDragItem {
  mime: string;
  name: string;
  data: Uint8Array;
}

/** Drag session messages mirror C2S_SURFACE_POINTER's field encoding:
 *  surface_id, x and y are little-endian u16.  An ENTER may carry an
 *  optional item trailer — `[item_count:2]` then per item
 *  `[mime_len:2][mime]`, one MIME per dragged file in item order.  The
 *  trailer is append-only: no `items` arg builds bytes identical to the
 *  pre-trailer form.  Chromium fetches the drag offer's data at
 *  wl_data_device.enter, so the server uses the list to pre-create the
 *  planned staging files and serve a real text/uri-list during hover. */
export function buildSurfaceDragEnterMessage(
  surfaceId: number,
  x: number,
  y: number,
  mimes: string[],
  items?: string[],
): Uint8Array {
  const encoded = mimes.map((mime) => textEncoder.encode(mime));
  const encodedItems = items?.map((mime) => textEncoder.encode(mime));
  let length = 9;
  for (const mime of encoded) length += 2 + mime.length;
  if (encodedItems) {
    length += 2;
    for (const mime of encodedItems) length += 2 + mime.length;
  }
  const msg = new Uint8Array(length);
  msg[0] = C2S_SURFACE_DRAG_ENTER;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = x & 0xff;
  msg[4] = (x >> 8) & 0xff;
  msg[5] = y & 0xff;
  msg[6] = (y >> 8) & 0xff;
  msg[7] = encoded.length & 0xff;
  msg[8] = (encoded.length >> 8) & 0xff;
  let at = 9;
  for (const mime of encoded) {
    msg[at] = mime.length & 0xff;
    msg[at + 1] = (mime.length >> 8) & 0xff;
    msg.set(mime, at + 2);
    at += 2 + mime.length;
  }
  if (encodedItems) {
    msg[at] = encodedItems.length & 0xff;
    msg[at + 1] = (encodedItems.length >> 8) & 0xff;
    at += 2;
    for (const mime of encodedItems) {
      msg[at] = mime.length & 0xff;
      msg[at + 1] = (mime.length >> 8) & 0xff;
      msg.set(mime, at + 2);
      at += 2 + mime.length;
    }
  }
  return msg;
}

export function buildSurfaceDragMotionMessage(
  surfaceId: number,
  x: number,
  y: number,
): Uint8Array {
  const msg = new Uint8Array(7);
  msg[0] = C2S_SURFACE_DRAG_MOTION;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = x & 0xff;
  msg[4] = (x >> 8) & 0xff;
  msg[5] = y & 0xff;
  msg[6] = (y >> 8) & 0xff;
  return msg;
}

export function buildSurfaceDragLeaveMessage(surfaceId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SURFACE_DRAG_LEAVE;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  return msg;
}

export function buildSurfaceDragDropMessage(
  surfaceId: number,
  x: number,
  y: number,
  items: SurfaceDragItem[],
): Uint8Array {
  const encoded = items.map((item) => ({
    mime: textEncoder.encode(item.mime),
    name: textEncoder.encode(item.name),
    data: item.data,
  }));
  let length = 9;
  for (const item of encoded)
    length +=
      2 + item.mime.length + 2 + item.name.length + 4 + item.data.length;
  const msg = new Uint8Array(length);
  msg[0] = C2S_SURFACE_DRAG_DROP;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = x & 0xff;
  msg[4] = (x >> 8) & 0xff;
  msg[5] = y & 0xff;
  msg[6] = (y >> 8) & 0xff;
  msg[7] = encoded.length & 0xff;
  msg[8] = (encoded.length >> 8) & 0xff;
  const v = new DataView(msg.buffer);
  let at = 9;
  for (const item of encoded) {
    msg[at] = item.mime.length & 0xff;
    msg[at + 1] = (item.mime.length >> 8) & 0xff;
    msg.set(item.mime, at + 2);
    at += 2 + item.mime.length;
    msg[at] = item.name.length & 0xff;
    msg[at + 1] = (item.name.length >> 8) & 0xff;
    msg.set(item.name, at + 2);
    at += 2 + item.name.length;
    v.setUint32(at, item.data.length, true);
    msg.set(item.data, at + 4);
    at += 4 + item.data.length;
  }
  return msg;
}

export function buildSurfaceDragCancelMessage(): Uint8Array {
  return new Uint8Array([C2S_SURFACE_DRAG_CANCEL]);
}

export function buildSurfaceAxisMessage(
  surfaceId: number,
  axis: number,
  valueX100: number,
): Uint8Array {
  const msg = new Uint8Array(8);
  msg[0] = C2S_SURFACE_POINTER_AXIS;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = axis;
  const v = new DataView(msg.buffer);
  v.setInt32(4, valueX100, true);
  return msg;
}

/** One scroll event: both axes, the device that produced it, and whether
 *  the gesture has ended. */
export interface SurfaceAxisEvent {
  /** Horizontal distance in composited-frame pixels, positive = right. */
  dx: number;
  /** Vertical distance in composited-frame pixels, positive = down. */
  dy: number;
  /** Horizontal wheel travel in 120ths of a detent. */
  v120x: number;
  /** Vertical wheel travel in 120ths of a detent. */
  v120y: number;
  /** An AXIS_SOURCE_* value, or null when the device is unclassified. */
  source: number | null;
  /** True when this ends the scroll sequence; deltas are ignored. */
  stop: boolean;
  /** The browser wheel event's own `timeStamp`, in ms. Toolkits integrate axis
   *  deltas against these timestamps for kinetic scrolling, so the spacing must
   *  be the browser's and not the server's arrival time. */
  timeMs?: number;
}

/** Clamp to the wire's signed range so a runaway delta cannot wrap into a
 *  scroll the other direction. */
function clampI32(v: number): number {
  if (!Number.isFinite(v)) return 0;
  return Math.max(-2147483648, Math.min(2147483647, Math.round(v)));
}

function clampI16(v: number): number {
  if (!Number.isFinite(v)) return 0;
  return Math.max(-32768, Math.min(32767, Math.round(v)));
}

export function buildSurfaceAxis2Message(
  surfaceId: number,
  ev: SurfaceAxisEvent,
): Uint8Array {
  const msg = new Uint8Array(20);
  msg[0] = C2S_SURFACE_POINTER_AXIS2;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] =
    (ev.source === null ? 0 : (ev.source & 0b11) | AXIS_FLAG_SOURCE_KNOWN) |
    (ev.stop ? AXIS_FLAG_STOP : 0);
  const v = new DataView(msg.buffer);
  v.setInt32(4, clampI32(ev.dx * 100), true);
  v.setInt32(8, clampI32(ev.dy * 100), true);
  v.setInt16(12, clampI16(ev.v120x), true);
  v.setInt16(14, clampI16(ev.v120y), true);
  // The browser wheel event's own time: toolkits integrate axis deltas against
  // these timestamps for kinetic scrolling, so a burst that all shares one
  // instant reads as infinite velocity and flings nothing.
  v.setUint32(16, Math.max(0, Math.round(ev.timeMs ?? 0)) >>> 0, true);
  return msg;
}

export interface SurfaceTouchPoint {
  identifier: number;
  /** Horizontal position in the composited frame's pixel space. */
  x: number;
  /** Vertical position in the composited frame's pixel space. */
  y: number;
}

/** Build one direct-touch event. Its message boundary becomes
 * `wl_touch.frame`, preserving contacts that changed together. */
export function buildSurfaceTouchMessage(
  surfaceId: number,
  phase: number,
  contacts: readonly SurfaceTouchPoint[] = [],
  timeMs = 0,
): Uint8Array {
  const count = Math.min(255, contacts.length);
  const msg = new Uint8Array(9 + count * 12);
  const view = new DataView(msg.buffer);
  msg[0] = C2S_SURFACE_TOUCH;
  view.setUint16(1, surfaceId, true);
  msg[3] = phase;
  msg[4] = count;
  // The browser's own event time. Apps derive fling velocity from the spacing
  // between motion events, and stamping on arrival instead collapses a burst of
  // coalesced moves onto one instant — which reads as infinite velocity and
  // kills inertial scrolling outright.
  view.setUint32(5, Math.max(0, Math.round(timeMs)) >>> 0, true);
  for (let i = 0; i < count; i++) {
    const point = contacts[i]!;
    const offset = 9 + i * 12;
    view.setInt32(offset, clampI32(point.identifier), true);
    view.setInt32(offset + 4, clampI32(point.x * 100), true);
    view.setInt32(offset + 8, clampI32(point.y * 100), true);
  }
  return msg;
}

/**
 * @param scale120 Requested presentation scale in 1/120th units:
 *                 60 = 0.5×, 120 = 1×, 180 = 1.5×, 240 = 2×.
 *                 0 means unspecified (server defaults to 1×).
 * @param codecSupport Bitmask of CODEC_SUPPORT_* flags. 0 = accept anything.
 */
export function buildSurfaceResizeMessage(
  surfaceId: number,
  width: number,
  height: number,
  scale120: number = 0,
): Uint8Array {
  const msg = new Uint8Array(9);
  msg[0] = C2S_SURFACE_RESIZE;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = width & 0xff;
  msg[4] = (width >> 8) & 0xff;
  msg[5] = height & 0xff;
  msg[6] = (height >> 8) & 0xff;
  msg[7] = scale120 & 0xff;
  msg[8] = (scale120 >> 8) & 0xff;
  return msg;
}

export function buildSurfaceFocusMessage(surfaceId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SURFACE_FOCUS;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  return msg;
}

export function buildSurfaceCloseMessage(surfaceId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SURFACE_CLOSE;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  return msg;
}

/**
 * Build a surface subscribe message with optional codec, bandwidth and
 * speed overrides, and an optional fixed encode size.
 *
 * @param codecSupport - CODEC_SUPPORT_* bitmask (0 = use connection default from sendClientFeatures)
 * @param bandwidth - SURFACE_BANDWIDTH_* constant (0 = use server default)
 * @param speed - SURFACE_SPEED_* constant (0 = use server default)
 * @param width - fixed encode width in pixels (0 = participate in mediation)
 * @param height - fixed encode height in pixels (0 = participate in mediation)
 * @param maxFps - per-surface frame-rate ceiling (0 = client display rate)
 *
 * Asking for a size opts this subscription out of surface-size mediation:
 * the server encodes a downscale of the surface for this client alone
 * instead of pulling the compositor surface down to fit it.  Contract —
 * see the C2S_SURFACE_SUBSCRIBE arm in crates/server.
 */
export function buildSurfaceSubscribeMessage(
  surfaceId: number,
  codecSupport?: number,
  bandwidth?: number,
  speed?: number,
  width?: number,
  height?: number,
  maxFps?: number,
): Uint8Array {
  const cs = (codecSupport ?? 0) & 0xff;
  const bw = (bandwidth ?? 0) & 0xff;
  const sp = (speed ?? 0) & 0xff;
  const w = (width ?? 0) & 0xffff;
  const h = (height ?? 0) & 0xffff;
  const requestedFps = maxFps ?? 0;
  const fps = Number.isFinite(requestedFps)
    ? Math.max(0, Math.min(65_535, Math.round(requestedFps)))
    : 0;
  // The size lives at bytes 6..10, so asking for one forces the long form
  // even when all three preference bytes are at their defaults.
  const hasScaled = w !== 0 && h !== 0;
  const hasCadence = fps !== 0;
  const hasExtended =
    hasScaled || hasCadence || cs !== 0 || bw !== 0 || sp !== 0;
  const len = hasCadence ? 12 : hasScaled ? 10 : hasExtended ? 6 : 3;
  const msg = new Uint8Array(len);
  msg[0] = C2S_SURFACE_SUBSCRIBE;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  if (hasExtended) {
    msg[3] = cs;
    msg[4] = bw;
    msg[5] = sp;
  }
  if (hasScaled) {
    msg[6] = w & 0xff;
    msg[7] = (w >> 8) & 0xff;
    msg[8] = h & 0xff;
    msg[9] = (h >> 8) & 0xff;
  }
  if (hasCadence) {
    msg[10] = fps & 0xff;
    msg[11] = (fps >> 8) & 0xff;
  }
  return msg;
}

export function buildSurfaceUnsubscribeMessage(surfaceId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SURFACE_UNSUBSCRIBE;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  return msg;
}

export function buildSurfaceAckMessage(
  surfaceId: number,
  decoderQueueDepth = 0,
): Uint8Array {
  // Byte 3 is an optional, backward-compatible WebCodecs decodeQueueSize.
  // Old servers ignore it; old/CLI clients omit it and new servers read 0.
  const msg = new Uint8Array(4);
  msg[0] = C2S_SURFACE_ACK;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = Math.max(0, Math.min(255, Math.trunc(decoderQueueDepth)));
  return msg;
}

export function buildClipboardMessage(
  mimeType: string,
  data: Uint8Array,
): Uint8Array {
  return buildSelectionMessage(C2S_CLIPBOARD_SET, mimeType, data);
}

/** Request the MIME types currently offered by the compositor clipboard. */
export function buildClipboardListMessage(): Uint8Array {
  return new Uint8Array([C2S_CLIPBOARD_LIST]);
}

/** Read one MIME type from the compositor clipboard. */
export function buildClipboardGetMessage(mimeType: string): Uint8Array {
  const mimeBytes = textEncoder.encode(mimeType);
  const msg = new Uint8Array(3 + mimeBytes.length);
  msg[0] = C2S_CLIPBOARD_GET;
  msg[1] = mimeBytes.length & 0xff;
  msg[2] = (mimeBytes.length >> 8) & 0xff;
  msg.set(mimeBytes, 3);
  return msg;
}

/**
 * Take ownership of PRIMARY, the selection a middle click pastes.
 *
 * Sent just before the middle button reaches the surface rather than
 * whenever the page selection changes: the compositor serves these bytes
 * itself, so owning PRIMARY continuously would mean permanently displacing
 * whichever Wayland client the user last selected text in.
 */
export function buildPrimaryMessage(
  mimeType: string,
  data: Uint8Array,
): Uint8Array {
  return buildSelectionMessage(C2S_PRIMARY_SET, mimeType, data);
}

/** Shared framing for the two selection setters, which differ only in tag. */
function buildSelectionMessage(
  tag: number,
  mimeType: string,
  data: Uint8Array,
): Uint8Array {
  const mimeBytes = textEncoder.encode(mimeType);
  const msg = new Uint8Array(7 + mimeBytes.length + data.length);
  msg[0] = tag;
  msg[1] = mimeBytes.length & 0xff;
  msg[2] = (mimeBytes.length >> 8) & 0xff;
  msg.set(mimeBytes, 3);
  const v = new DataView(msg.buffer);
  v.setUint32(3 + mimeBytes.length, data.length, true);
  msg.set(data, 7 + mimeBytes.length);
  return msg;
}

/**
 * Build a C2S_CLIENT_FEATURES message.
 *
 * `maxDecodeW`/`maxDecodeH` are the largest frame this browser's video
 * decoder was confirmed to handle, as little-endian u16s.  `0` means "not
 * determined"; the server then holds the client to the H.264 ceiling rather
 * than assume a decoder that advertises AV1 will take a 5K frame.  Servers
 * predating the field ignore the extra bytes. `clientFeatures` negotiates
 * optional frame extensions; bit 0 requests precise surface timestamps.
 */
export function buildClientFeaturesMessage(
  codecSupport: number,
  maxDecodeW: number = 0,
  maxDecodeH: number = 0,
  clientFeatures: number = CLIENT_FEATURE_SURFACE_TIMESTAMP_SUB_US,
): Uint8Array {
  const msg = new Uint8Array(7);
  msg[0] = C2S_CLIENT_FEATURES;
  msg[1] = codecSupport & 0xff;
  const w = maxDecodeW & 0xffff;
  const h = maxDecodeH & 0xffff;
  msg[2] = w & 0xff;
  msg[3] = (w >> 8) & 0xff;
  msg[4] = h & 0xff;
  msg[5] = (h >> 8) & 0xff;
  msg[6] = clientFeatures & 0xff;
  return msg;
}

/**
 * Build a C2S_AUDIO_SUBSCRIBE message.
 * `bitrateKbps`: 0 = server default, otherwise the desired Opus bitrate
 * in kbps (e.g. 64 for 64 kbps).  Sent as a little-endian u16.
 * Can be sent repeatedly to adjust bitrate without unsubscribing first.
 */
export function buildAudioSubscribeMessage(
  bitrateKbps: number = 0,
): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_AUDIO_SUBSCRIBE;
  msg[1] = bitrateKbps & 0xff;
  msg[2] = (bitrateKbps >> 8) & 0xff;
  return msg;
}

export function buildAudioUnsubscribeMessage(): Uint8Array {
  return new Uint8Array([C2S_AUDIO_UNSUBSCRIBE]);
}
