import {
  C2S_ACK,
  C2S_CLIENT_METRICS,
  C2S_DISPLAY_RATE,
  C2S_INPUT,
  C2S_MOUSE,
  C2S_RESTART,
  C2S_RESIZE,
  C2S_SCROLL,
  C2S_FOCUS,
  C2S_CLOSE,
  C2S_SUBSCRIBE,
  C2S_UNSUBSCRIBE,
  C2S_SEARCH,
  C2S_CREATE2,
  CREATE2_HAS_SRC_PTY,
  CREATE2_HAS_COMMAND,
} from "./types";

const textEncoder = new TextEncoder();

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

export function buildInputMessage(
  ptyId: number,
  data: Uint8Array,
): Uint8Array {
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
  const msg = new Uint8Array(7);
  msg[0] = C2S_RESIZE;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  msg[3] = rows & 0xff;
  msg[4] = (rows >> 8) & 0xff;
  msg[5] = cols & 0xff;
  msg[6] = (cols >> 8) & 0xff;
  return msg;
}

export function buildScrollMessage(
  ptyId: number,
  offset: number,
): Uint8Array {
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
  options?: { tag?: string; command?: string; srcPtyId?: number },
): Uint8Array {
  const tagBytes = options?.tag
    ? textEncoder.encode(options.tag)
    : new Uint8Array(0);
  let features = 0;
  const hasSrc = options?.srcPtyId != null;
  const cmdText = options?.command?.trim() ?? "";
  const hasCmd = cmdText.length > 0;
  if (hasSrc) features |= CREATE2_HAS_SRC_PTY;
  if (hasCmd) features |= CREATE2_HAS_COMMAND;
  const cmdBytes = hasCmd ? textEncoder.encode(cmdText) : new Uint8Array(0);
  const msg = new Uint8Array(
    10 + tagBytes.length + (hasSrc ? 2 : 0) + cmdBytes.length,
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
