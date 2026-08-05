import {
  C2S_ACK,
  C2S_CLIENT_METRICS,
  C2S_CLIPBOARD_SET,
  C2S_DISPLAY_RATE,
  C2S_INPUT,
  C2S_KILL,
  C2S_TERM_CWD,
  S2C_TERM_CWD,
  S2C_TERM_CWD_EVENT,
  C2S_MOUSE,
  C2S_RESTART,
  C2S_RESIZE,
  C2S_SCROLL,
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
  C2S_SURFACE_TEXT,
  C2S_AUDIO_SUBSCRIBE,
  C2S_AUDIO_UNSUBSCRIBE,
  CREATE2_HAS_SRC_PTY,
  CREATE2_HAS_COMMAND,
  CREATE2_HAS_CWD,
  CREATE2_WANT_STATUS,
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
  const msg = new Uint8Array(3);
  msg[0] = C2S_DISPLAY_RATE;
  msg[1] = fps & 0xff;
  msg[2] = (fps >> 8) & 0xff;
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

export function buildCreate2Message(
  nonce: number,
  rows: number,
  cols: number,
  options?: {
    tag?: string;
    command?: string;
    srcPtyId?: number;
    cwd?: string;
    /** Only pass this when the server advertised `FEATURE_CREATE_STATUS`. */
    wantStatus?: boolean;
  },
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
  const cmdText = options?.command?.trim() ?? "";
  const hasCmd = cmdText.length > 0;
  if (hasSrc) features |= CREATE2_HAS_SRC_PTY;
  if (hasCwd) features |= CREATE2_HAS_CWD;
  if (hasCmd) features |= CREATE2_HAS_COMMAND;
  if (options?.wantStatus) features |= CREATE2_WANT_STATUS;
  const cmdBytes = hasCmd ? textEncoder.encode(cmdText) : new Uint8Array(0);
  const msg = new Uint8Array(
    10 +
      tagBytes.length +
      (hasSrc ? 2 : 0) +
      (hasCwd ? 2 + cwdBytes.length : 0) +
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
  if (cmdBytes.length) msg.set(cmdBytes, cursor);
  return msg;
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

export function buildKillMessage(ptyId: number, signal: number): Uint8Array {
  const msg = new Uint8Array(7);
  msg[0] = C2S_KILL;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  const view = new DataView(msg.buffer);
  view.setInt32(3, signal, true);
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
): Uint8Array {
  const msg = new Uint8Array(8);
  msg[0] = C2S_SURFACE_INPUT;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  const v = new DataView(msg.buffer);
  v.setUint32(3, keycode, true);
  msg[7] = pressed ? 1 : 0;
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

export const SURFACE_POINTER_DOWN = 0;
export const SURFACE_POINTER_UP = 1;
export const SURFACE_POINTER_MOVE = 2;

export function buildSurfacePointerMessage(
  surfaceId: number,
  type: number,
  button: number,
  x: number,
  y: number,
): Uint8Array {
  const msg = new Uint8Array(9);
  msg[0] = C2S_SURFACE_POINTER;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  msg[3] = type;
  msg[4] = button;
  msg[5] = x & 0xff;
  msg[6] = (x >> 8) & 0xff;
  msg[7] = y & 0xff;
  msg[8] = (y >> 8) & 0xff;
  return msg;
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
  const msg = new Uint8Array(16);
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
  return msg;
}

/**
 * @param scale120 DPR in 1/120th units (Wayland convention):
 *                 120 = 1×, 180 = 1.5×, 240 = 2×.
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
): Uint8Array {
  const cs = (codecSupport ?? 0) & 0xff;
  const bw = (bandwidth ?? 0) & 0xff;
  const sp = (speed ?? 0) & 0xff;
  const w = (width ?? 0) & 0xffff;
  const h = (height ?? 0) & 0xffff;
  // The size lives at bytes 6..10, so asking for one forces the long form
  // even when all three preference bytes are at their defaults.
  const hasScaled = w !== 0 && h !== 0;
  const hasExtended = hasScaled || cs !== 0 || bw !== 0 || sp !== 0;
  const len = hasScaled ? 10 : hasExtended ? 6 : 3;
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
  return msg;
}

export function buildSurfaceUnsubscribeMessage(surfaceId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SURFACE_UNSUBSCRIBE;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  return msg;
}

export function buildSurfaceAckMessage(surfaceId: number): Uint8Array {
  const msg = new Uint8Array(3);
  msg[0] = C2S_SURFACE_ACK;
  msg[1] = surfaceId & 0xff;
  msg[2] = (surfaceId >> 8) & 0xff;
  return msg;
}

export function buildClipboardMessage(
  mimeType: string,
  data: Uint8Array,
): Uint8Array {
  const mimeBytes = textEncoder.encode(mimeType);
  const msg = new Uint8Array(7 + mimeBytes.length + data.length);
  msg[0] = C2S_CLIPBOARD_SET;
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
 * predating the field ignore the extra bytes.
 */
export function buildClientFeaturesMessage(
  codecSupport: number,
  maxDecodeW: number = 0,
  maxDecodeH: number = 0,
): Uint8Array {
  const msg = new Uint8Array(6);
  msg[0] = C2S_CLIENT_FEATURES;
  msg[1] = codecSupport & 0xff;
  const w = maxDecodeW & 0xffff;
  const h = maxDecodeH & 0xffff;
  msg[2] = w & 0xff;
  msg[3] = (w >> 8) & 0xff;
  msg[4] = h & 0xff;
  msg[5] = (h >> 8) & 0xff;
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
