/**
 * Multiplexed transport (WebSocket with optional WebTransport upgrade).
 *
 * A single connection to the gateway carries traffic for all destinations.
 * Each destination gets a lightweight "channel" that implements
 * {@link BlitTransport} so it can be handed directly to a
 * {@link BlitConnection}.
 *
 * When a `wtUrl` is provided and the browser supports WebTransport, the
 * transport will try QUIC first and fall back to WebSocket on failure.
 *
 * Wire format (after authentication):
 *
 *   Data frame:    [channel_id:2 LE][blit_payload:N]
 *   Control frame: [0xFFFF][opcode:1][...]
 *
 * Over WebSocket each frame is a single binary message.
 * Over WebTransport frames are length-prefixed on a bidirectional stream:
 *   [frame_len:4 LE][mux_frame]
 *
 * Control opcodes:
 *   C2S  OPEN  0x01  [ch:2][name_len:2][name:N]
 *   C2S  CLOSE 0x02  [ch:2]
 *   S2C  OPENED 0x81 [ch:2]
 *   S2C  CLOSED 0x82 [ch:2]
 *   S2C  ERROR  0x83 [ch:2][msg_len:2][msg:N]
 */

import {
  noopDebug,
  type BlitDebug,
  type BlitTransport,
  type BlitTransportOptions,
  type ConnectionStatus,
} from "../types";

// -- Protocol constants -----------------------------------------------------

const MUX_CONTROL = 0xffff;
const MUX_C2S_OPEN = 0x01;
const MUX_C2S_CLOSE = 0x02;
const MUX_S2C_OPENED = 0x81;
const MUX_S2C_CLOSED = 0x82;
const MUX_S2C_ERROR = 0x83;

const textDecoder = new TextDecoder();

// -- MuxTransport -----------------------------------------------------------

export interface MuxTransportOptions extends BlitTransportOptions {
  /** WebTransport URL (e.g. `https://host:3264/mux`).  When set and the
   *  browser supports WebTransport, QUIC is tried first. */
  wtUrl?: string;
  /** SHA-256 cert hash (hex) for self-signed WebTransport certs. */
  wtCertHash?: string;
  /** Optional debug logger for connection diagnostics. */
  debug?: BlitDebug;
}

/**
 * Manages a single multiplexed connection and exposes per-destination
 * channels that each implement {@link BlitTransport}.
 */
export class MuxTransport {
  private ws: WebSocket | null = null;
  // WebTransport state
  private wt: WebTransport | null = null;
  private wtWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private wtReadAbort: AbortController | null = null;

  private _status: ConnectionStatus = "disconnected";
  private _authRejected = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;
  /** True while an async WT connect attempt is in progress. */
  private wtConnecting = false;

  private readonly wsUrl: string;
  private readonly passphrase: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;
  private readonly wtUrl: string | undefined;
  private readonly wtCertHash: Uint8Array | undefined;
  /** Set to true after the first WT failure so we stop retrying WT. */
  private wtFailed = false;
  private readonly dbg: BlitDebug;

  /** All channels keyed by channel ID. */
  private readonly channels = new Map<number, MuxChannel>();
  /** Next channel ID to assign. */
  private nextChannelId = 0;
  /** Channels that were open/opening when the connection dropped — need re-open on reconnect. */
  private readonly pendingReopen = new Set<MuxChannel>();
  /** Per-channel reconnect timers for channels that received S2C_CLOSED/ERROR. */
  private readonly channelReconnectTimers = new Map<
    number,
    ReturnType<typeof setTimeout>
  >();

  constructor(
    wsUrl: string,
    passphrase: string,
    options?: MuxTransportOptions,
  ) {
    this.wsUrl = wsUrl;
    this.passphrase = passphrase;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.currentDelay = this.initialDelay;
    this.dbg = options?.debug ?? noopDebug;
    if (options?.wtUrl) {
      this.wtUrl = options.wtUrl;
    }
    if (options?.wtCertHash) {
      this.wtCertHash = hexToBytes(options.wtCertHash);
    }
  }

  /** Current transport-level status. */
  get status(): ConnectionStatus {
    return this._status;
  }

  /** True when connected over WebTransport (QUIC) rather than WebSocket. */
  get isWebTransport(): boolean {
    return this.wt !== null && this._status === "connected";
  }

  // -- Lifecycle ------------------------------------------------------------

  connect(): void {
    if (this.disposed) return;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
      this.currentDelay = this.initialDelay;
    }
    if (
      this._status === "connecting" ||
      this._status === "authenticating" ||
      this._status === "connected"
    )
      return;

    this.setStatus("connecting");

    // Try WebTransport first if available.
    if (this.shouldTryWt()) {
      this.dbg.log("attempting WebTransport to %s", this.wtUrl);
      this.connectWt();
      return;
    }

    this.dbg.log(
      "skipping WT (failed=%s, url=%s, api=%s), using WebSocket to %s",
      this.wtFailed,
      !!this.wtUrl,
      typeof WebTransport !== "undefined",
      this.wsUrl,
    );
    this.connectWs();
  }

  close(): void {
    this.disposed = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    for (const timer of this.channelReconnectTimers.values()) {
      clearTimeout(timer);
    }
    this.channelReconnectTimers.clear();
    for (const ch of this.channels.values()) {
      ch._setStatus("closed");
    }
    this.channels.clear();
    this.pendingReopen.clear();
    this.cleanupWs();
    this.cleanupWt();
    this.setStatus("closed");
  }

  // -- Channel management ---------------------------------------------------

  /**
   * Create a channel for the given destination name.  The channel is not
   * opened until its `connect()` method is called (which happens
   * automatically when a {@link BlitConnection} is created with
   * `autoConnect: true`).
   */
  createChannel(destName: string): MuxChannel {
    const id = this.nextChannelId++;
    const ch = new MuxChannel(this, id, destName, this.initialDelay);
    this.channels.set(id, ch);
    return ch;
  }

  /**
   * Remove a channel.  Sends CLOSE if the underlying connection is open.
   * Called internally by {@link MuxChannel.close}.
   */
  _removeChannel(ch: MuxChannel): void {
    this.channels.delete(ch.channelId);
    this.pendingReopen.delete(ch);
    this._cancelChannelReconnect(ch.channelId);
    if (this._status === "connected") {
      this._sendClose(ch.channelId);
    }
  }

  /** @internal Cancel any pending per-channel reconnect timer. */
  _cancelChannelReconnect(channelId: number): void {
    const timer = this.channelReconnectTimers.get(channelId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.channelReconnectTimers.delete(channelId);
    }
  }

  /** Send a raw mux frame.  Over WS this is a single binary message;
   *  over WT it is length-prefixed on the bidirectional stream. */
  _sendRaw(data: Uint8Array): void {
    if (this.wtWriter) {
      // Length-prefixed: [len:4 LE][data]
      const frame = new Uint8Array(4 + data.length);
      frame[0] = data.length & 0xff;
      frame[1] = (data.length >> 8) & 0xff;
      frame[2] = (data.length >> 16) & 0xff;
      frame[3] = (data.length >> 24) & 0xff;
      frame.set(data, 4);
      this.wtWriter.write(frame).catch(() => {});
      return;
    }
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data as Uint8Array<ArrayBuffer>);
    }
  }

  /** Send an OPEN control message for a channel. */
  _sendOpen(ch: MuxChannel): void {
    if (this._status !== "connected") {
      this.pendingReopen.add(ch);
      this.connect();
      return;
    }
    const nameBytes = new TextEncoder().encode(ch.destName);
    const buf = new Uint8Array(2 + 1 + 2 + 2 + nameBytes.length);
    const view = new DataView(buf.buffer);
    view.setUint16(0, MUX_CONTROL, true);
    buf[2] = MUX_C2S_OPEN;
    view.setUint16(3, ch.channelId, true);
    view.setUint16(5, nameBytes.length, true);
    buf.set(nameBytes, 7);
    this._sendRaw(buf);
  }

  // -- Internal: WebSocket --------------------------------------------------

  private connectWs(): void {
    this.dbg.log("opening WebSocket to %s", this.wsUrl);
    const socket = new WebSocket(this.wsUrl);
    socket.binaryType = "arraybuffer";

    if (this.ws && this.ws !== socket) {
      try {
        this.ws.onclose = null;
        this.ws.close();
      } catch {
        /* ignore */
      }
    }
    this.ws = socket;

    let authenticated = false;

    socket.onopen = () => {
      if (this.ws !== socket || this.disposed) return;
      this.setStatus("authenticating");
      socket.send(this.passphrase);
    };

    socket.onmessage = (e: MessageEvent) => {
      if (this.ws !== socket || this.disposed) return;

      if (typeof e.data === "string") {
        if (e.data === "mux") {
          this.dbg.log("WebSocket authenticated");
          authenticated = true;
          this.setStatus("connected");
          this.currentDelay = this.initialDelay;
          this.reopenChannels();
        } else if (e.data === "auth") {
          this.dbg.warn("WebSocket auth rejected");
          this._authRejected = true;
          for (const ch of this.channels.values()) {
            ch._setAuthRejected();
          }
          this.setStatus("error");
          socket.close();
        } else {
          this.setStatus("error");
          socket.close();
        }
        return;
      }

      if (authenticated && e.data instanceof ArrayBuffer) {
        this.handleMuxFrame(e.data);
      }
    };

    socket.onerror = () => {
      if (this.ws !== socket || this.disposed) return;
      if (!authenticated) {
        this.setStatus("error");
      }
    };

    socket.onclose = () => {
      if (this.ws !== socket || this.disposed) return;
      this.ws = null;
      this.handleDisconnect();
    };
  }

  private cleanupWs(): void {
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.onopen = null;
      this.ws.close();
      this.ws = null;
    }
  }

  // -- Internal: WebTransport -----------------------------------------------

  private shouldTryWt(): boolean {
    return (
      !this.wtFailed && !!this.wtUrl && typeof WebTransport !== "undefined"
    );
  }

  private connectWt(): void {
    if (this.wtConnecting) return;
    this.wtConnecting = true;
    this.connectWtAsync()
      .catch(() => {})
      .finally(() => {
        this.wtConnecting = false;
      });
  }

  private async connectWtAsync(): Promise<void> {
    if (this.disposed || !this.wtUrl) return;

    try {
      const opts: WebTransportOptions = {};
      if (this.wtCertHash) {
        opts.serverCertificateHashes = [
          {
            algorithm: "sha-256",
            value: this.wtCertHash.buffer as ArrayBuffer,
          },
        ];
      }

      const wt = new WebTransport(this.wtUrl, opts);
      await Promise.race([
        wt.ready,
        new Promise((_, reject) =>
          setTimeout(() => reject(new Error("WT connect timeout")), 10_000),
        ),
      ]);

      if (this.disposed) {
        wt.close();
        return;
      }

      this.wt = wt;

      // Open a bidirectional stream for the mux protocol.
      const stream = await wt.createBidirectionalStream();
      const writer = stream.writable.getWriter();
      const reader = stream.readable.getReader();

      // Authenticate: [pass_len:2 LE][passphrase] → [1/0]
      this.setStatus("authenticating");
      const passBytes = new TextEncoder().encode(this.passphrase);
      const authMsg = new Uint8Array(2 + passBytes.length);
      authMsg[0] = passBytes.length & 0xff;
      authMsg[1] = (passBytes.length >> 8) & 0xff;
      authMsg.set(passBytes, 2);
      await writer.write(authMsg);

      // Read 1-byte auth response.
      const authResp = await readExact(reader, 1);
      if (!authResp || authResp[0] !== 1) {
        this.dbg.warn(
          "WebTransport auth rejected (resp=%s)",
          authResp ? authResp[0] : "EOF",
        );
        this._authRejected = true;
        for (const ch of this.channels.values()) {
          ch._setAuthRejected();
        }
        this.setStatus("error");
        wt.close();
        this.wt = null;
        return;
      }

      if (this.disposed) {
        wt.close();
        this.wt = null;
        return;
      }

      this.dbg.log("WebTransport connected and authenticated");
      this.wtWriter = writer;
      this._authRejected = false;
      this.currentDelay = this.initialDelay;
      this.setStatus("connected");
      this.reopenChannels();

      // Start read loop in background.
      const abort = new AbortController();
      this.wtReadAbort = abort;
      this.wtReadLoop(reader, wt, abort.signal);

      // Handle connection close.
      wt.closed
        .then(() => {
          if (this.wt !== wt || this.disposed) return;
          this.cleanupWt();
          this.handleDisconnect();
        })
        .catch(() => {
          if (this.wt !== wt || this.disposed) return;
          this.cleanupWt();
          this.handleDisconnect();
        });
    } catch (err) {
      // WT failed — mark as failed and fall back to WS.
      this.dbg.warn(
        "WebTransport failed, falling back to WebSocket: %s",
        err instanceof Error ? err.message : String(err),
      );
      this.wtFailed = true;
      this.cleanupWt();
      if (this.disposed) return;
      if (this._authRejected) {
        this.setStatus("error");
        return;
      }
      // Fall back to WS immediately (don't schedule reconnect — we haven't
      // been connected yet).
      this.connectWs();
    }
  }

  private wtReadLoop(
    reader: ReadableStreamDefaultReader<Uint8Array>,
    wt: WebTransport,
    signal: AbortSignal,
  ): void {
    // Run async read loop; on exit, close the WT session.
    (async () => {
      let buffer = new Uint8Array(0);
      try {
        while (!signal.aborted) {
          // Parse length-prefixed frames from buffer.
          while (buffer.length >= 4) {
            const len =
              buffer[0] |
              (buffer[1] << 8) |
              (buffer[2] << 16) |
              (buffer[3] << 24);
            if (len < 0 || len > 16 * 1024 * 1024) {
              wt.close();
              return;
            }
            if (buffer.length < 4 + len) break;
            const frame = buffer.slice(4, 4 + len);
            buffer = buffer.subarray(4 + len);
            this.handleMuxFrame(frame.buffer);
          }

          const { value, done } = await reader.read();
          if (done || signal.aborted || this.wt !== wt) break;
          if (!value || value.length === 0) continue;

          const newBuf = new Uint8Array(buffer.length + value.length);
          newBuf.set(buffer);
          newBuf.set(value, buffer.length);
          buffer = newBuf;
        }
      } catch {
        // Stream closed or error — handled by wt.closed handler.
      }
    })();
  }

  private cleanupWt(): void {
    this.wtWriter = null;
    if (this.wtReadAbort) {
      this.wtReadAbort.abort();
      this.wtReadAbort = null;
    }
    if (this.wt) {
      try {
        this.wt.close();
      } catch {}
      this.wt = null;
    }
  }

  // -- Internal: shared -----------------------------------------------------

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    const prev = this._status;
    this._status = status;
    this.dbg.log("mux status %s → %s", prev, status);
  }

  private handleDisconnect(): void {
    // Cancel per-channel reconnect timers — the transport-level reconnect
    // will re-open all channels via reopenChannels().
    for (const timer of this.channelReconnectTimers.values()) {
      clearTimeout(timer);
    }
    this.channelReconnectTimers.clear();
    if (this._authRejected) {
      this.setStatus("disconnected");
      return;
    }
    for (const ch of this.channels.values()) {
      if (ch._internalStatus !== "closed") {
        this.pendingReopen.add(ch);
      }
      ch._setStatus("disconnected");
    }
    this.setStatus("disconnected");
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this._reconnect) return;
    if (this.reconnectTimer !== null) return;
    this.dbg.log("scheduling reconnect in %dms", this.currentDelay);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) {
        this.connect();
      }
    }, this.currentDelay);
    this.currentDelay = Math.min(
      this.currentDelay * this.backoff,
      this.maxDelay,
    );
  }

  /**
   * Schedule a re-open attempt for a single channel after it received
   * S2C_CLOSED or S2C_ERROR.  Uses per-channel exponential backoff.
   */
  private scheduleChannelReconnect(ch: MuxChannel): void {
    if (this.disposed || !this._reconnect) return;
    if (ch._internalStatus === "closed") return;
    if (this.channelReconnectTimers.has(ch.channelId)) return;
    const delay = ch._reconnectDelay;
    ch._reconnectDelay = Math.min(delay * this.backoff, this.maxDelay);
    this.channelReconnectTimers.set(
      ch.channelId,
      setTimeout(() => {
        this.channelReconnectTimers.delete(ch.channelId);
        if (this.disposed || !this.channels.has(ch.channelId)) return;
        if (
          ch._internalStatus === "closed" ||
          ch._internalStatus === "connected" ||
          ch._internalStatus === "connecting"
        )
          return;
        if (this._status === "connected") {
          ch._setStatus("connecting");
          this._sendOpen(ch);
        } else {
          // Not connected — queue for when it reconnects.
          this.pendingReopen.add(ch);
        }
      }, delay),
    );
  }

  /** @internal */
  _sendClose(channelId: number): void {
    const buf = new Uint8Array(5);
    const view = new DataView(buf.buffer);
    view.setUint16(0, MUX_CONTROL, true);
    buf[2] = MUX_C2S_CLOSE;
    view.setUint16(3, channelId, true);
    this._sendRaw(buf);
  }

  private reopenChannels(): void {
    for (const ch of this.pendingReopen) {
      ch._setStatus("connecting");
      this._sendOpen(ch);
    }
    this.pendingReopen.clear();
  }

  private handleMuxFrame(data: ArrayBuffer): void {
    if (data.byteLength < 2) return;
    const bytes = new Uint8Array(data);
    const chId = bytes[0] | (bytes[1] << 8);

    if (chId === MUX_CONTROL) {
      this.handleControl(bytes);
    } else {
      const ch = this.channels.get(chId);
      if (ch) {
        // Deliver the payload (without the 2-byte channel prefix).
        ch._deliverMessage(data.slice(2));
      }
    }
  }

  private handleControl(bytes: Uint8Array): void {
    if (bytes.length < 5) return;
    const opcode = bytes[2];
    const chId = bytes[3] | (bytes[4] << 8);
    const ch = this.channels.get(chId);

    switch (opcode) {
      case MUX_S2C_OPENED:
        if (ch) {
          ch._lastError = null;
          ch._reconnectDelay = this.initialDelay;
          ch._setStatus("connected");
        }
        break;

      case MUX_S2C_CLOSED:
        if (ch && ch._internalStatus !== "connecting") {
          ch._setStatus("disconnected");
          this.scheduleChannelReconnect(ch);
        }
        break;

      case MUX_S2C_ERROR: {
        if (bytes.length < 7) break;
        const msgLen = bytes[5] | (bytes[6] << 8);
        const msg =
          bytes.length >= 7 + msgLen
            ? textDecoder.decode(bytes.subarray(7, 7 + msgLen))
            : "unknown error";
        if (ch) {
          ch._lastError = msg;
          ch._setStatus("error");
          this.scheduleChannelReconnect(ch);
        }
        break;
      }
    }
  }
}

// -- MuxChannel -------------------------------------------------------------

/**
 * A single virtual channel on a {@link MuxTransport}.
 * Implements {@link BlitTransport} so it can be used directly by
 * {@link BlitConnection}.
 */
export class MuxChannel implements BlitTransport {
  /** @internal */ _internalStatus: ConnectionStatus = "disconnected";
  /** @internal */ _lastError: string | null = null;
  /** @internal Per-channel backoff delay for reconnect scheduling. */
  /** @internal */ _reconnectDelay: number;

  private readonly mux: MuxTransport;
  readonly channelId: number;
  readonly destName: string;
  private _authRejected = false;
  private messageListeners = new Set<(data: ArrayBuffer) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();

  constructor(
    mux: MuxTransport,
    channelId: number,
    destName: string,
    initialDelay: number,
  ) {
    this.mux = mux;
    this.channelId = channelId;
    this.destName = destName;
    this._reconnectDelay = initialDelay;
  }

  get status(): ConnectionStatus {
    return this._internalStatus;
  }

  get authRejected(): boolean {
    return this._authRejected;
  }

  get lastError(): string | null {
    return this._lastError;
  }

  connect(): void {
    if (
      this._internalStatus === "connecting" ||
      this._internalStatus === "connected" ||
      this._internalStatus === "closed"
    )
      return;
    this.mux._cancelChannelReconnect(this.channelId);
    this._setStatus("connecting");
    this.mux._sendOpen(this);
  }

  reconnect(): void {
    if (this._internalStatus === "closed") return;
    this.mux._cancelChannelReconnect(this.channelId);
    // Ask the server to tear down the existing channel.
    if (
      this._internalStatus === "connected" ||
      this._internalStatus === "connecting"
    ) {
      this.mux._sendClose(this.channelId);
    }
    this._setStatus("disconnected");
    // Immediately reopen.
    this._setStatus("connecting");
    this.mux._sendOpen(this);
  }

  send(data: Uint8Array): void {
    if (this._internalStatus !== "connected") return;
    // Prepend the 2-byte channel ID.
    const frame = new Uint8Array(2 + data.length);
    frame[0] = this.channelId & 0xff;
    frame[1] = (this.channelId >> 8) & 0xff;
    frame.set(data, 2);
    this.mux._sendRaw(frame);
  }

  close(): void {
    if (this._internalStatus === "closed") return;
    this.mux._removeChannel(this);
    this._setStatus("closed");
  }

  addEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  addEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  addEventListener(type: string, listener: (...args: never[]) => void): void {
    if (type === "message") {
      this.messageListeners.add(listener as (data: ArrayBuffer) => void);
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  removeEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  removeEventListener(
    type: string,
    listener: (...args: never[]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.delete(listener as (data: ArrayBuffer) => void);
    } else if (type === "statuschange") {
      this.statusListeners.delete(
        listener as (status: ConnectionStatus) => void,
      );
    }
  }

  // -- Internal (called by MuxTransport) ------------------------------------

  /** @internal */
  _setStatus(status: ConnectionStatus): void {
    if (this._internalStatus === status) return;
    if (this._internalStatus === "closed") return; // terminal
    this._internalStatus = status;
    for (const l of this.statusListeners) l(status);
  }

  /** @internal */
  _setAuthRejected(): void {
    this._authRejected = true;
    this._lastError = "Authentication failed";
    this._setStatus("error");
  }

  /** @internal */
  _deliverMessage(data: ArrayBuffer): void {
    for (const l of this.messageListeners) l(data);
  }
}

// -- Helpers ----------------------------------------------------------------

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[^0-9a-fA-F]/g, "");
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/** Read exactly `n` bytes from a ReadableStreamDefaultReader, buffering
 *  partial reads.  Returns null on EOF before `n` bytes. */
async function readExact(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  n: number,
): Promise<Uint8Array | null> {
  const buf = new Uint8Array(n);
  let offset = 0;
  while (offset < n) {
    const { value, done } = await reader.read();
    if (done || !value) return null;
    const take = Math.min(value.length, n - offset);
    buf.set(value.subarray(0, take), offset);
    offset += take;
    // If we got more than we needed, that's a problem — but for the 1-byte
    // auth response this won't happen.  The read loop handles buffering for
    // the data path.
  }
  return buf;
}
