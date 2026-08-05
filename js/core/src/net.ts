/** TCP and UDP relay (docs/design/net.md): wire constants, message builders and parsers, and a transport-agnostic stream table. */

// -- Opcodes ----------------------------------------------------------------

/** Open a socket: [0x80][stream_id:2][flags:1][port:2][host_len:2][host:N] + TLS block */
export const C2S_NET_OPEN = 0x80;
/** Stream payload, TCP only: [0x81][stream_id:2][data:N] */
export const C2S_NET_DATA = 0x81;
/** Cumulative byte-window credit, TCP only: [0x82][stream_id:2][bytes:8] */
export const C2S_NET_ACK = 0x82;
/** Close or half-close: [0x83][stream_id:2][flags:1] */
export const C2S_NET_CLOSE = 0x83;
/** One datagram, UDP only: [0x84][stream_id:2][payload:N] */
export const C2S_NET_DGRAM = 0x84;

/** Open result: [0x80][stream_id:2][status:1][alpn_len:1][alpn:N][detail_len:2][detail:N] + optional [window:8] */
export const S2C_NET_OPENED = 0x80;
/** Stream payload, TCP only: [0x81][stream_id:2][data:N] */
export const S2C_NET_DATA = 0x81;
/** Cumulative byte-window credit, TCP only: [0x82][stream_id:2][bytes:8] */
export const S2C_NET_ACK = 0x82;
/** Socket ended: [0x83][stream_id:2][reason:1][detail_len:2][detail:N] */
export const S2C_NET_CLOSED = 0x83;
/** One datagram, UDP only: [0x84][stream_id:2][payload:N] */
export const S2C_NET_DGRAM = 0x84;

/** `S2C_HELLO` feature bit: the server supports the `NET_*` family. */
export const FEATURE_NET = 1 << 10;

// -- Flags ------------------------------------------------------------------

/** Terminate TLS toward the target; relayed bytes are plaintext. */
export const NET_OPEN_TLS = 1 << 0;
/** Skip certificate verification. */
export const NET_OPEN_INSECURE = 1 << 1;
/** Open a UDP datagram flow rather than a TCP stream. */
export const NET_OPEN_UDP = 1 << 2;

/** Shut down only the client's write side, leaving the stream readable. */
export const NET_CLOSE_WRITE = 1 << 0;

// -- Statuses and reasons ---------------------------------------------------

export const NET_STATUS_OK = 0;
export const NET_STATUS_UNKNOWN_ID = 1;
export const NET_STATUS_NOT_FOUND = 2;
export const NET_STATUS_REFUSED = 3;
export const NET_STATUS_PERMISSION = 4;
export const NET_STATUS_TLS = 5;
export const NET_STATUS_BUDGET = 6;
export const NET_STATUS_INVALID = 7;
export const NET_STATUS_OTHER = 9;

export function netStatusText(status: number): string {
  switch (status) {
    case NET_STATUS_OK:
      return "ok";
    case NET_STATUS_UNKNOWN_ID:
      return "unknown stream id";
    case NET_STATUS_NOT_FOUND:
      return "host did not resolve";
    case NET_STATUS_REFUSED:
      return "connection refused";
    case NET_STATUS_PERMISSION:
      return "refused by policy";
    case NET_STATUS_TLS:
      return "TLS failed";
    case NET_STATUS_BUDGET:
      return "budget exhausted";
    case NET_STATUS_INVALID:
      return "invalid request";
    default:
      return "error";
  }
}

export const NET_CLOSED_EOF = 0;
export const NET_CLOSED_RESET = 1;
export const NET_CLOSED_TIMEOUT = 2;
export const NET_CLOSED_POLICY = 3;
export const NET_CLOSED_BUDGET = 4;
export const NET_CLOSED_SHUTDOWN = 5;

export function netClosedText(reason: number): string {
  switch (reason) {
    case NET_CLOSED_EOF:
      return "closed";
    case NET_CLOSED_RESET:
      return "reset";
    case NET_CLOSED_TIMEOUT:
      return "idle timeout";
    case NET_CLOSED_POLICY:
      return "refused by policy";
    case NET_CLOSED_BUDGET:
      return "budget exceeded";
    case NET_CLOSED_SHUTDOWN:
      return "server going away";
    default:
      return "ended";
  }
}

// -- Limits -----------------------------------------------------------------

/** Maximum `NET_DATA`/`NET_DGRAM` payload. */
export const NET_MAX_CHUNK = 64 * 1024;
/** Largest per-stream unacked-byte window, and the one a stream gets when it is the only one open. */
export const NET_WINDOW_BYTES = 1024 * 1024;
/**
 * Smallest per-stream unacked-byte window the server ever grants.
 *
 * The real window is a share of the connection's aggregate, reported on the
 * accept; until it arrives this much is safe at any concurrency.
 */
export const NET_WINDOW_MIN = 2 * NET_MAX_CHUNK;
/** Maximum concurrent sockets per connection. */
export const NET_MAX_SOCKETS = 256;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: false });

// -- Builders and parsers ---------------------------------------------------

export interface NetOpenOptions {
  /** Terminate TLS toward the target (TCP only). */
  tls?: boolean;
  /** Skip certificate verification; the server must also permit it. */
  insecure?: boolean;
  /** Open a datagram flow instead of a stream. */
  udp?: boolean;
  /** SNI to present; empty or omitted uses `host`. */
  sni?: string;
  /** ALPN protocols to offer, in order. */
  alpn?: readonly string[];
}

export function netOpenFlags(options: NetOpenOptions = {}): number {
  let flags = 0;
  if (options.tls) flags |= NET_OPEN_TLS;
  if (options.insecure) flags |= NET_OPEN_INSECURE;
  if (options.udp) flags |= NET_OPEN_UDP;
  return flags;
}

export function buildNetOpenMessage(
  streamId: number,
  host: string,
  port: number,
  options: NetOpenOptions = {},
): Uint8Array {
  const hostBytes = textEncoder.encode(host);
  const flags = netOpenFlags(options);
  const tlsBlock: Uint8Array[] = [];
  let tlsLen = 0;
  if (flags & NET_OPEN_TLS) {
    const sni = textEncoder.encode(options.sni ?? "");
    const head = new Uint8Array(2 + sni.length + 1);
    head[0] = sni.length & 0xff;
    head[1] = (sni.length >> 8) & 0xff;
    head.set(sni, 2);
    const protos = (options.alpn ?? []).filter((p) => p.length > 0);
    head[2 + sni.length] = protos.length & 0xff;
    tlsBlock.push(head);
    tlsLen += head.length;
    for (const proto of protos) {
      const pb = textEncoder.encode(proto);
      const entry = new Uint8Array(1 + pb.length);
      entry[0] = pb.length & 0xff;
      entry.set(pb, 1);
      tlsBlock.push(entry);
      tlsLen += entry.length;
    }
  }
  const msg = new Uint8Array(8 + hostBytes.length + tlsLen);
  msg[0] = C2S_NET_OPEN;
  msg[1] = streamId & 0xff;
  msg[2] = (streamId >> 8) & 0xff;
  msg[3] = flags;
  msg[4] = port & 0xff;
  msg[5] = (port >> 8) & 0xff;
  msg[6] = hostBytes.length & 0xff;
  msg[7] = (hostBytes.length >> 8) & 0xff;
  msg.set(hostBytes, 8);
  let offset = 8 + hostBytes.length;
  for (const part of tlsBlock) {
    msg.set(part, offset);
    offset += part.length;
  }
  return msg;
}

function buildPayloadMessage(
  opcode: number,
  streamId: number,
  payload: Uint8Array,
): Uint8Array {
  const msg = new Uint8Array(3 + payload.length);
  msg[0] = opcode;
  msg[1] = streamId & 0xff;
  msg[2] = (streamId >> 8) & 0xff;
  msg.set(payload, 3);
  return msg;
}

export function buildNetDataMessage(
  streamId: number,
  data: Uint8Array,
): Uint8Array {
  return buildPayloadMessage(C2S_NET_DATA, streamId, data);
}

export function buildNetDgramMessage(
  streamId: number,
  payload: Uint8Array,
): Uint8Array {
  return buildPayloadMessage(C2S_NET_DGRAM, streamId, payload);
}

export function buildNetAckMessage(
  streamId: number,
  bytes: number,
): Uint8Array {
  const msg = new Uint8Array(11);
  msg[0] = C2S_NET_ACK;
  msg[1] = streamId & 0xff;
  msg[2] = (streamId >> 8) & 0xff;
  // 64-bit cumulative counter.
  new DataView(msg.buffer).setUint32(3, bytes % 0x100000000, true);
  new DataView(msg.buffer).setUint32(7, Math.floor(bytes / 0x100000000), true);
  return msg;
}

export function buildNetCloseMessage(streamId: number, flags = 0): Uint8Array {
  const msg = new Uint8Array(4);
  msg[0] = C2S_NET_CLOSE;
  msg[1] = streamId & 0xff;
  msg[2] = (streamId >> 8) & 0xff;
  msg[3] = flags;
  return msg;
}

export interface NetOpenedMessage {
  streamId: number;
  status: number;
  alpn: string;
  detail: string;
  /** The send window the server granted, absent from a server that does not report one. */
  window?: number;
}

export function parseNetOpenedMessage(
  bytes: Uint8Array,
): NetOpenedMessage | null {
  if (bytes.length < 7 || bytes[0] !== S2C_NET_OPENED) return null;
  const streamId = bytes[1] | (bytes[2] << 8);
  const status = bytes[3];
  const alpnLen = bytes[4];
  if (bytes.length < 5 + alpnLen + 2) return null;
  const alpn = textDecoder.decode(bytes.subarray(5, 5 + alpnLen));
  const rest = 5 + alpnLen;
  const detailLen = bytes[rest] | (bytes[rest + 1] << 8);
  if (bytes.length < rest + 2 + detailLen) return null;
  const detail = textDecoder.decode(
    bytes.subarray(rest + 2, rest + 2 + detailLen),
  );
  const tail = rest + 2 + detailLen;
  const parsed: NetOpenedMessage = { streamId, status, alpn, detail };
  // Absent rather than guessed: a server that predates the field sends nothing
  // here, and a partial tail is no more of a number than no tail at all.
  if (bytes.length >= tail + 8) {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    parsed.window =
      view.getUint32(tail, true) + view.getUint32(tail + 4, true) * 0x100000000;
  }
  return parsed;
}

export interface NetClosedMessage {
  streamId: number;
  reason: number;
  detail: string;
}

export function parseNetClosedMessage(
  bytes: Uint8Array,
): NetClosedMessage | null {
  if (bytes.length < 6 || bytes[0] !== S2C_NET_CLOSED) return null;
  const streamId = bytes[1] | (bytes[2] << 8);
  const reason = bytes[3];
  const detailLen = bytes[4] | (bytes[5] << 8);
  if (bytes.length < 6 + detailLen) return null;
  return {
    streamId,
    reason,
    detail: textDecoder.decode(bytes.subarray(6, 6 + detailLen)),
  };
}

/** Split `[stream_id:2][payload:N]` off a data or datagram message. */
export function parseNetPayload(
  bytes: Uint8Array,
  opcode: number,
): { streamId: number; payload: Uint8Array } | null {
  if (bytes.length < 3 || bytes[0] !== opcode) return null;
  return {
    streamId: bytes[1] | (bytes[2] << 8),
    payload: bytes.subarray(3),
  };
}

export function parseNetAckMessage(
  bytes: Uint8Array,
): { streamId: number; bytes: number } | null {
  if (bytes.length < 11 || bytes[0] !== S2C_NET_ACK) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const lo = view.getUint32(3, true);
  const hi = view.getUint32(7, true);
  return { streamId: bytes[1] | (bytes[2] << 8), bytes: hi * 0x100000000 + lo };
}

/** True for any S2C opcode in the family's `0x80` block. */
export function isNetMessage(opcode: number): boolean {
  return opcode >= S2C_NET_OPENED && opcode <= S2C_NET_DGRAM;
}

// -- Stream table -----------------------------------------------------------

/** One relayed socket, from the client's side. */
export interface NetStream {
  readonly streamId: number;
  /** Resolves with the negotiated ALPN once the socket is open, rejects with the server's reason if it never opened. */
  readonly opened: Promise<string>;
  /** Queue bytes toward the target, respecting the server's credit. */
  write(data: Uint8Array): Promise<void>;
  /** Shut down the write side, leaving the stream readable — the FIN some protocols use to signal end of input. */
  shutdownWrite(): void;
  /** Abort in both directions. */
  close(): void;
  /** Payload from the target, in order, until the socket ends. */
  read(): AsyncGenerator<Uint8Array, void, void>;
}

interface StreamState {
  streamId: number;
  opened: Promise<string>;
  resolveOpened: (alpn: string) => void;
  rejectOpened: (err: Error) => void;
  settled: boolean;
  /** Bytes the server has confirmed writing to the target. */
  acked: number;
  /** Bytes handed to the server. */
  sent: number;
  /**
   * How much may be unacked. The server grants a share of the connection's
   * aggregate and closes a stream that exceeds it, so this starts at the
   * smallest share it ever grants and rises to whatever the accept reports.
   */
  window: number;
  /** Woken when credit frees up. */
  creditWaiters: Array<() => void>;
  /** Payload waiting to be read, and the reader waiting for it. */
  inbox: Uint8Array[];
  inboxWaiters: Array<() => void>;
  /** Bytes delivered to the consumer, for our own cumulative ack. */
  received: number;
  ended: boolean;
  endedError: Error | null;
}

/** Every relayed socket on one connection: id allocation, credit, and demux. */
export class NetStreams {
  private readonly send: (msg: Uint8Array) => void;
  private readonly streams = new Map<number, StreamState>();
  private nextId = 1;

  constructor(send: (msg: Uint8Array) => void) {
    this.send = send;
  }

  get openCount(): number {
    return this.streams.size;
  }

  /** Open a relayed socket. */
  open(host: string, port: number, options: NetOpenOptions = {}): NetStream {
    if (this.streams.size >= NET_MAX_SOCKETS) {
      throw new Error("too many relayed sockets");
    }
    const streamId = this.allocId();
    let resolveOpened!: (alpn: string) => void;
    let rejectOpened!: (err: Error) => void;
    const opened = new Promise<string>((resolve, reject) => {
      resolveOpened = resolve;
      rejectOpened = reject;
    });
    // Swallow the rejection for bookkeeping purposes: a caller that never looks at `opened` (it closed the stream, or only cares about `read`) must not turn a refused open into an unhandled rejection, while a caller that does look still sees the error.
    opened.catch(() => {});
    const state: StreamState = {
      streamId,
      opened,
      resolveOpened,
      rejectOpened,
      settled: false,
      acked: 0,
      sent: 0,
      window: NET_WINDOW_MIN,
      creditWaiters: [],
      inbox: [],
      inboxWaiters: [],
      received: 0,
      ended: false,
      endedError: null,
    };
    this.streams.set(streamId, state);
    // Nothing waits a round trip for an id we chose: the open and the first bytes can go out together.
    this.send(buildNetOpenMessage(streamId, host, port, options));
    return this.stream(state);
  }

  private allocId(): number {
    for (let i = 0; i <= 0xffff; i++) {
      const id = this.nextId;
      this.nextId = (this.nextId + 1) & 0xffff;
      if (this.nextId === 0) this.nextId = 1;
      if (!this.streams.has(id)) return id;
    }
    throw new Error("no free stream id");
  }

  private stream(state: StreamState): NetStream {
    const self = this;
    return {
      streamId: state.streamId,
      opened: state.opened,
      async write(data: Uint8Array): Promise<void> {
        for (let offset = 0; offset < data.length; offset += NET_MAX_CHUNK) {
          const chunk = data.subarray(offset, offset + NET_MAX_CHUNK);
          // Respect the window: the server acks bytes it has written to the target, and outrunning that does not throttle the stream, it closes it.
          while (
            !state.ended &&
            state.sent - state.acked + chunk.length > state.window
          ) {
            await new Promise<void>((resolve) =>
              state.creditWaiters.push(resolve),
            );
          }
          if (state.ended) return;
          state.sent += chunk.length;
          self.send(buildNetDataMessage(state.streamId, chunk));
        }
      },
      shutdownWrite(): void {
        if (!state.ended) {
          self.send(buildNetCloseMessage(state.streamId, NET_CLOSE_WRITE));
        }
      },
      close(): void {
        if (!state.ended) self.send(buildNetCloseMessage(state.streamId, 0));
        self.finish(state, null);
      },
      read: async function* () {
        for (;;) {
          while (state.inbox.length > 0) {
            const chunk = state.inbox.shift()!;
            state.received += chunk.length;
            // Acks advance the server's window; without them a large response stalls after one window's worth.
            self.send(buildNetAckMessage(state.streamId, state.received));
            yield chunk;
          }
          if (state.ended) {
            if (state.endedError) throw state.endedError;
            return;
          }
          await new Promise<void>((resolve) =>
            state.inboxWaiters.push(resolve),
          );
        }
      },
    };
  }

  /** Feed one S2C message in. */
  handleMessage(bytes: Uint8Array): boolean {
    if (bytes.length === 0 || !isNetMessage(bytes[0])) return false;
    switch (bytes[0]) {
      case S2C_NET_OPENED: {
        const parsed = parseNetOpenedMessage(bytes);
        if (!parsed) return true;
        const state = this.streams.get(parsed.streamId);
        if (!state || state.settled) return true;
        state.settled = true;
        if (parsed.status === NET_STATUS_OK) {
          // A server that reports nothing is older than the field; it enforces
          // the same shrinking window without naming it, and this client's
          // socket count is the page's business — a service worker holds dozens
          // — so its silence has to be read as the smallest share it grants.
          // The alternative is a stream closed for BUDGET mid-upload.
          state.window = parsed.window ?? NET_WINDOW_MIN;
          this.wake(state.creditWaiters);
          state.resolveOpened(parsed.alpn);
        } else {
          const detail = parsed.detail
            ? `${netStatusText(parsed.status)}: ${parsed.detail}`
            : netStatusText(parsed.status);
          const err = new Error(detail);
          state.rejectOpened(err);
          // A failed open produces no NET_CLOSED — nothing was ever open — so retire the id here or it leaks.
          this.finish(state, err);
        }
        return true;
      }
      case S2C_NET_DATA:
      case S2C_NET_DGRAM: {
        const parsed = parseNetPayload(bytes, bytes[0]);
        if (!parsed) return true;
        const state = this.streams.get(parsed.streamId);
        if (!state) return true;
        // Copy: the caller's buffer may be a view into a reused frame.
        state.inbox.push(new Uint8Array(parsed.payload));
        this.wake(state.inboxWaiters);
        return true;
      }
      case S2C_NET_ACK: {
        const parsed = parseNetAckMessage(bytes);
        if (!parsed) return true;
        const state = this.streams.get(parsed.streamId);
        if (!state) return true;
        if (parsed.bytes > state.acked) {
          state.acked = parsed.bytes;
          this.wake(state.creditWaiters);
        }
        return true;
      }
      case S2C_NET_CLOSED: {
        const parsed = parseNetClosedMessage(bytes);
        if (!parsed) return true;
        const state = this.streams.get(parsed.streamId);
        if (!state) return true;
        // EOF is an ordinary end of stream; anything else is an error the consumer should see rather than a silent truncation.
        const err =
          parsed.reason === NET_CLOSED_EOF
            ? null
            : new Error(
                parsed.detail
                  ? `${netClosedText(parsed.reason)}: ${parsed.detail}`
                  : netClosedText(parsed.reason),
              );
        if (!state.settled) {
          state.settled = true;
          state.rejectOpened(err ?? new Error("closed before opening"));
        }
        this.finish(state, err);
        return true;
      }
      default:
        return true;
    }
  }

  /** Fail every live socket, for a connection that has gone away. */
  reset(err: Error): void {
    for (const state of [...this.streams.values()]) {
      if (!state.settled) {
        state.settled = true;
        state.rejectOpened(err);
      }
      this.finish(state, err);
    }
  }

  private finish(state: StreamState, err: Error | null): void {
    if (!state.ended) {
      state.ended = true;
      state.endedError = err;
    }
    this.streams.delete(state.streamId);
    this.wake(state.inboxWaiters);
    this.wake(state.creditWaiters);
  }

  private wake(waiters: Array<() => void>): void {
    const pending = waiters.splice(0, waiters.length);
    for (const resolve of pending) resolve();
  }
}
