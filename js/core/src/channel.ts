/** Native bidirectional channels (`docs/design/extensions.md`). */

export const CHANNEL = 0x95;
export const FEATURE_CHANNEL = 1 << 12;
/** Its own bit: a `WATCH` an older server does not know is skipped in silence,
 *  which reads exactly like a name nobody serves. */
export const FEATURE_CHANNEL_WATCH = 1 << 26;

export const CHANNEL_LISTEN = 1;
export const CHANNEL_CONNECT = 2;
export const CHANNEL_DATA = 3;
export const CHANNEL_ACK = 4;
export const CHANNEL_CLOSE = 5;
export const CHANNEL_WATCH = 6;
export const CHANNEL_UNWATCH = 7;

export const CHANNEL_OPENED = 1;
export const CHANNEL_ACCEPTED = 2;
export const CHANNEL_CLOSED = 5;
export const CHANNEL_NAMES = 6;

export const CHANNEL_EXPECT_LISTENER_TOKEN = 1 << 0;

export const CHANNEL_CLOSE_NORMAL = 0;
export const CHANNEL_CLOSE_CANCELLED = 1;
export const CHANNEL_CLOSE_PEER_GONE = 2;
export const CHANNEL_CLOSE_PROTOCOL_VIOLATION = 3;
export const CHANNEL_CLOSE_SERVER_SHUTDOWN = 4;

export const CHANNEL_MAX_NAME = 255;
export const CHANNEL_MAX_PEER = 255;
export const CHANNEL_MAX_METADATA = 64 * 1024;
export const CHANNEL_MAX_PAYLOAD = 1024 * 1024;
export const CHANNEL_MAX_DETAIL = 4 * 1024;
export const CHANNEL_WINDOW_BYTES = 1024n * 1024n;
export const CHANNEL_MAX_UNCONSUMED_MESSAGES = 1024;
/** Names one watch may declare. A watch names what it cares about so its
 *  traffic cannot scale with churn it has no interest in. */
export const CHANNEL_MAX_WATCH_NAMES = 32;

const encoder = new TextEncoder();
const fatalDecoder = new TextDecoder("utf-8", { fatal: true });

export type ChannelMessage =
  | {
      kind: "opened";
      channelId: number;
      status: number;
      window: bigint;
      peer: string;
      metadata: Uint8Array;
      detail: string;
    }
  | {
      kind: "accepted";
      channelId: number;
      listenerId: number;
      window: bigint;
      peer: string;
      metadata: Uint8Array;
    }
  | { kind: "data"; channelId: number; payload: Uint8Array }
  | { kind: "ack"; channelId: number; bytes: bigint }
  | {
      kind: "closed";
      channelId: number;
      reason: number;
      detail: string;
    }
  | {
      /** Which of a watch's declared names have a listener right now. A name
       *  that was asked about and is missing here has none, so an empty list
       *  is an answer rather than nothing to say. */
      kind: "names";
      channelId: number;
      names: string[];
    };

export interface ChannelConnectOptions {
  metadata?: Uint8Array;
  /** Optimistically require one exact listener generation. */
  listenerToken?: Uint8Array;
}

function envelope(
  kind: number,
  channelId: number,
  bodyLength: number,
): Uint8Array {
  if (!Number.isInteger(channelId) || channelId < 0 || channelId > 0xffffffff) {
    throw new RangeError("channel id must be a u32");
  }
  const message = new Uint8Array(6 + bodyLength);
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = kind;
  view.setUint32(2, channelId, true);
  return message;
}

function clientId(channelId: number): void {
  if ((channelId & 1) !== 0) {
    throw new RangeError("client-created channel id must be even");
  }
}

function channelName(name: string): Uint8Array {
  if ([...name].some((character) => /\p{Cc}/u.test(character))) {
    throw new Error("channel name contains a control character");
  }
  const bytes = encoder.encode(name);
  if (bytes.length === 0 || bytes.length > CHANNEL_MAX_NAME) {
    throw new RangeError("channel name must contain 1 to 255 UTF-8 bytes");
  }
  return bytes;
}

function metadataBytes(metadata: Uint8Array | undefined): Uint8Array {
  const bytes = metadata ?? new Uint8Array(0);
  if (bytes.length > CHANNEL_MAX_METADATA) {
    throw new RangeError("channel metadata exceeds 64 KiB");
  }
  return bytes;
}

function buildOpen(
  kind: number,
  channelId: number,
  name: string,
  metadata: Uint8Array,
  listenerToken?: Uint8Array,
): Uint8Array {
  clientId(channelId);
  const nameBytes = channelName(name);
  if (metadata.length > CHANNEL_MAX_METADATA) {
    throw new RangeError("channel metadata exceeds 64 KiB");
  }
  if (listenerToken !== undefined && listenerToken.length !== 16) {
    throw new RangeError("listener token must contain 16 bytes");
  }
  const tokenLength = listenerToken?.length ?? 0;
  const message = envelope(
    kind,
    channelId,
    1 + 2 + nameBytes.length + 4 + metadata.length + tokenLength,
  );
  const view = new DataView(message.buffer);
  let offset = 6;
  message[offset++] = listenerToken ? CHANNEL_EXPECT_LISTENER_TOKEN : 0;
  view.setUint16(offset, nameBytes.length, true);
  offset += 2;
  message.set(nameBytes, offset);
  offset += nameBytes.length;
  view.setUint32(offset, metadata.length, true);
  offset += 4;
  message.set(metadata, offset);
  offset += metadata.length;
  if (listenerToken) message.set(listenerToken, offset);
  return message;
}

export function buildChannelListenMessage(
  channelId: number,
  name: string,
  metadata?: Uint8Array,
): Uint8Array {
  return buildOpen(CHANNEL_LISTEN, channelId, name, metadataBytes(metadata));
}

export function buildChannelConnectMessage(
  channelId: number,
  name: string,
  options: ChannelConnectOptions = {},
): Uint8Array {
  return buildOpen(
    CHANNEL_CONNECT,
    channelId,
    name,
    metadataBytes(options.metadata),
    options.listenerToken,
  );
}

export function buildChannelDataMessage(
  channelId: number,
  payload: Uint8Array,
): Uint8Array {
  if (payload.length === 0 || payload.length > CHANNEL_MAX_PAYLOAD) {
    throw new RangeError("channel data must contain 1 byte to 1 MiB");
  }
  const message = envelope(CHANNEL_DATA, channelId, payload.length);
  message.set(payload, 6);
  return message;
}

export function buildChannelAckMessage(
  channelId: number,
  bytes: bigint,
): Uint8Array {
  if (bytes < 0n || bytes > 0xffffffffffffffffn) {
    throw new RangeError("channel ACK must be a u64");
  }
  const message = envelope(CHANNEL_ACK, channelId, 8);
  new DataView(message.buffer).setBigUint64(6, bytes, true);
  return message;
}

export function buildChannelCloseMessage(
  channelId: number,
  reason = CHANNEL_CLOSE_NORMAL,
): Uint8Array {
  if (reason !== CHANNEL_CLOSE_NORMAL && reason !== CHANNEL_CLOSE_CANCELLED) {
    throw new RangeError("client channel close reason is invalid");
  }
  const message = envelope(CHANNEL_CLOSE, channelId, 1);
  message[6] = reason;
  return message;
}

/**
 * Follow which of `names` currently have a listener.
 *
 * The ID is a client-created channel ID that carries no stream: it shares the
 * channel ID space so a watch and a channel can never be confused for one
 * another, and it is released by `buildChannelUnwatchMessage`.
 */
export function buildChannelWatchMessage(
  channelId: number,
  names: readonly string[],
): Uint8Array {
  clientId(channelId);
  if (names.length === 0) {
    throw new RangeError("a channel watch must declare at least one name");
  }
  if (names.length > CHANNEL_MAX_WATCH_NAMES) {
    throw new RangeError(`a channel watch declares at most 32 names`);
  }
  if (new Set(names).size !== names.length) {
    throw new RangeError("channel watch names must be distinct");
  }
  return buildNameList(CHANNEL_WATCH, channelId, names);
}

export function buildChannelUnwatchMessage(channelId: number): Uint8Array {
  clientId(channelId);
  return envelope(CHANNEL_UNWATCH, channelId, 0);
}

function buildNameList(
  kind: number,
  channelId: number,
  names: readonly string[],
): Uint8Array {
  const encoded = names.map(channelName);
  const body = encoded.reduce((total, name) => total + 2 + name.length, 3);
  const message = envelope(kind, channelId, body);
  const view = new DataView(message.buffer);
  let offset = 6;
  message[offset++] = 0;
  view.setUint16(offset, encoded.length, true);
  offset += 2;
  for (const name of encoded) {
    view.setUint16(offset, name.length, true);
    offset += 2;
    message.set(name, offset);
    offset += name.length;
  }
  return message;
}

function decodeNameList(bytes: Uint8Array): string[] | null {
  if (bytes.length < 9 || bytes[6] !== 0) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const count = view.getUint16(7, true);
  if (count > CHANNEL_MAX_WATCH_NAMES) return null;
  const names: string[] = [];
  let offset = 9;
  for (let index = 0; index < count; index += 1) {
    if (bytes.length < offset + 2) return null;
    const length = view.getUint16(offset, true);
    offset += 2;
    if (length === 0 || length > CHANNEL_MAX_NAME) return null;
    if (bytes.length < offset + length) return null;
    const name = decodeUtf8(bytes.subarray(offset, offset + length));
    if (name === null || /\p{Cc}/u.test(name)) return null;
    names.push(name);
    offset += length;
  }
  return offset === bytes.length ? names : null;
}

function decodeUtf8(bytes: Uint8Array): string | null {
  try {
    return fatalDecoder.decode(bytes);
  } catch {
    return null;
  }
}

function decodePeerMetadata(
  bytes: Uint8Array,
  offset: number,
): { peer: string; metadata: Uint8Array; offset: number } | null {
  if (bytes.length < offset + 2) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const peerLength = view.getUint16(offset, true);
  offset += 2;
  if (peerLength > CHANNEL_MAX_PEER || bytes.length < offset + peerLength + 4) {
    return null;
  }
  const peerBytes = bytes.subarray(offset, offset + peerLength);
  if (peerBytes.some((byte) => byte < 0x20 || byte > 0x7e)) return null;
  const peer = decodeUtf8(peerBytes);
  if (peer === null) return null;
  offset += peerLength;
  const metadataLength = view.getUint32(offset, true);
  offset += 4;
  if (
    metadataLength > CHANNEL_MAX_METADATA ||
    bytes.length < offset + metadataLength
  ) {
    return null;
  }
  const metadata = bytes.subarray(offset, offset + metadataLength);
  return { peer, metadata, offset: offset + metadataLength };
}

/** Decode one server-to-client channel packet. Unknown or malformed packets return null. */
export function parseChannelMessage(bytes: Uint8Array): ChannelMessage | null {
  if (bytes.length < 6 || bytes[0] !== CHANNEL) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const kind = bytes[1];
  const channelId = view.getUint32(2, true);
  switch (kind) {
    case CHANNEL_OPENED: {
      if (bytes.length < 21) return null;
      const decoded = decodePeerMetadata(bytes, 15);
      if (!decoded || bytes.length - decoded.offset > CHANNEL_MAX_DETAIL)
        return null;
      const detail = decodeUtf8(bytes.subarray(decoded.offset));
      if (detail === null) return null;
      return {
        kind: "opened",
        channelId,
        status: bytes[6],
        window: view.getBigUint64(7, true),
        peer: decoded.peer,
        metadata: decoded.metadata,
        detail,
      };
    }
    case CHANNEL_ACCEPTED: {
      if (bytes.length < 24) return null;
      const decoded = decodePeerMetadata(bytes, 18);
      if (!decoded || decoded.offset !== bytes.length) return null;
      return {
        kind: "accepted",
        channelId,
        listenerId: view.getUint32(6, true),
        window: view.getBigUint64(10, true),
        peer: decoded.peer,
        metadata: decoded.metadata,
      };
    }
    case CHANNEL_DATA: {
      const payload = bytes.subarray(6);
      if (payload.length === 0 || payload.length > CHANNEL_MAX_PAYLOAD)
        return null;
      return { kind: "data", channelId, payload };
    }
    case CHANNEL_ACK:
      if (bytes.length !== 14) return null;
      return {
        kind: "ack",
        channelId,
        bytes: view.getBigUint64(6, true),
      };
    case CHANNEL_CLOSED: {
      if (bytes.length < 7 || bytes.length - 7 > CHANNEL_MAX_DETAIL)
        return null;
      const detail = decodeUtf8(bytes.subarray(7));
      if (detail === null) return null;
      return {
        kind: "closed",
        channelId,
        reason: bytes[6],
        detail,
      };
    }
    case CHANNEL_NAMES: {
      const names = decodeNameList(bytes);
      return names === null ? null : { kind: "names", channelId, names };
    }
    default:
      return null;
  }
}

/**
 * Send-credit bookkeeping for one connected channel.
 *
 * The peer's window is the only backpressure a channel has: the server
 * refuses a DATA message that would put more than `window` unacknowledged
 * bytes on the wire, and a refusal closes the channel. Track the counters
 * here so a caller can ask before it builds a payload it cannot send.
 */
export class ChannelCredit {
  #window: bigint;
  #sent = 0n;
  #acked = 0n;
  #received = 0n;

  constructor(window: bigint) {
    this.#window = window;
  }

  get window(): bigint {
    return this.#window;
  }

  /** Bytes this side may still send before the peer acknowledges more. */
  get available(): bigint {
    const outstanding = this.#sent - this.#acked;
    return outstanding >= this.#window ? 0n : this.#window - outstanding;
  }

  /** Cumulative bytes received, which is what an ACK must carry. */
  get received(): bigint {
    return this.#received;
  }

  fits(length: number): boolean {
    return BigInt(length) <= this.available;
  }

  /** Record an outgoing DATA payload. Returns false when credit is short and
   *  nothing was recorded, so the caller can drop or queue instead. */
  charge(length: number): boolean {
    if (!this.fits(length)) return false;
    this.#sent += BigInt(length);
    return true;
  }

  /** Apply a peer ACK. Cumulative and monotonic; a replay is ignored. */
  acknowledge(bytes: bigint): void {
    if (bytes > this.#acked)
      this.#acked = bytes > this.#sent ? this.#sent : bytes;
  }

  /** Record an incoming DATA payload and return the new cumulative total. */
  receive(length: number): bigint {
    this.#received += BigInt(length);
    return this.#received;
  }
}

/** How a caller observes one connected channel. */
export interface ChannelOpenOptions extends ChannelConnectOptions {
  /** One complete message from the peer. Already acknowledged. */
  onData?(payload: Uint8Array): void;
  /** The peer acknowledged bytes; `available` is the new send credit. */
  onCredit?(available: bigint): void;
  /** Final closure, from either side or from transport loss. */
  onClosed?(reason: number, detail: string): void;
}

/**
 * A live watch over a set of channel names.
 *
 * Obtain one from `BlitConnection.watchChannelNames`. It resolves once the
 * server has answered with the state the registry is in, so a caller never has
 * to decide what an unanswered watch means.
 */
export interface ChannelNamesWatch {
  /** The declared names that have a listener, as of the last answer. */
  readonly present: ReadonlySet<string>;
  /** Stop watching. Idempotent, and safe after the transport is gone. */
  stop(): void;
}

/** A live channel. Obtain one from `BlitConnection.connectChannel`. */
export interface ChannelHandle {
  readonly channelId: number;
  readonly name: string;
  /** Server-assigned label for the peer, e.g. `ext:<id>:<attempt>`. */
  readonly peer: string;
  readonly metadata: Uint8Array;
  /** Bytes that may be sent before the peer acknowledges more. */
  readonly availableCredit: bigint;
  /** Send one message. Returns false when credit is short or the channel is
   *  gone; throws only on a payload the protocol cannot carry. */
  send(payload: Uint8Array | string): boolean;
  close(reason?: number): void;
}
