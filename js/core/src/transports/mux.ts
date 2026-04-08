/**
 * Multiplexed WebSocket transport.
 *
 * A single WebSocket connection to the gateway carries traffic for all
 * destinations.  Each destination gets a lightweight "channel" that
 * implements {@link BlitTransport} so it can be handed directly to a
 * {@link BlitConnection}.
 *
 * Wire format (after authentication):
 *
 *   Data frame:    [channel_id:2 LE][blit_payload:N]
 *   Control frame: [0xFFFF][opcode:1][...]
 *
 * Control opcodes:
 *   C2S  OPEN  0x01  [ch:2][name_len:2][name:N]
 *   C2S  CLOSE 0x02  [ch:2]
 *   S2C  OPENED 0x81 [ch:2]
 *   S2C  CLOSED 0x82 [ch:2]
 *   S2C  ERROR  0x83 [ch:2][msg_len:2][msg:N]
 */

import type {
  BlitTransport,
  BlitTransportOptions,
  ConnectionStatus,
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

export interface MuxTransportOptions extends BlitTransportOptions {}

/**
 * Manages a single multiplexed WebSocket and exposes per-destination
 * channels that each implement {@link BlitTransport}.
 */
export class MuxTransport {
  private ws: WebSocket | null = null;
  private _status: ConnectionStatus = "disconnected";
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;

  private readonly url: string;
  private readonly passphrase: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;

  /** All channels keyed by channel ID. */
  private readonly channels = new Map<number, MuxChannel>();
  /** Next channel ID to assign. */
  private nextChannelId = 0;
  /** Channels that were open/opening when the WS dropped — need re-open on reconnect. */
  private readonly pendingReopen = new Set<MuxChannel>();
  /** Per-channel reconnect timers for channels that received S2C_CLOSED/ERROR. */
  private readonly channelReconnectTimers = new Map<
    number,
    ReturnType<typeof setTimeout>
  >();

  constructor(url: string, passphrase: string, options?: MuxTransportOptions) {
    this.url = url;
    this.passphrase = passphrase;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.currentDelay = this.initialDelay;
  }

  /** Current WebSocket-level status. */
  get status(): ConnectionStatus {
    return this._status;
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

    const socket = new WebSocket(this.url);
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
          authenticated = true;
          this.setStatus("connected");
          this.currentDelay = this.initialDelay;
          // Re-open channels that were active before a disconnect.
          this.reopenChannels();
        } else if (e.data === "auth") {
          // Propagate auth failure to all channels.
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
        this.handleBinaryFrame(e.data);
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
      // Cancel per-channel reconnect timers — the WS-level reconnect
      // will re-open all channels via reopenChannels().
      for (const timer of this.channelReconnectTimers.values()) {
        clearTimeout(timer);
      }
      this.channelReconnectTimers.clear();
      // Queue ALL non-closed channels for reopen (not just connected/
      // connecting ones — a channel that was already "disconnected" from
      // a prior S2C_CLOSED also needs to be retried once the WS is back).
      for (const ch of this.channels.values()) {
        if (ch._internalStatus !== "closed") {
          this.pendingReopen.add(ch);
        }
        ch._setStatus("disconnected");
      }
      this.setStatus("disconnected");
      this.scheduleReconnect();
    };
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
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.onopen = null;
      this.ws.close();
      this.ws = null;
    }
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
   * Remove a channel.  Sends CLOSE if the underlying WebSocket is open.
   * Called internally by {@link MuxChannel.close}.
   */
  _removeChannel(ch: MuxChannel): void {
    this.channels.delete(ch.channelId);
    this.pendingReopen.delete(ch);
    this._cancelChannelReconnect(ch.channelId);
    if (this._status === "connected") {
      this.sendClose(ch.channelId);
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

  /** Send a raw binary frame over the WebSocket.  Used by MuxChannel. */
  _sendRaw(data: Uint8Array): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data as Uint8Array<ArrayBuffer>);
    }
  }

  /** Send an OPEN control message for a channel. */
  _sendOpen(ch: MuxChannel): void {
    if (this._status !== "connected") {
      // Queue for when the WS connects.
      this.pendingReopen.add(ch);
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

  // -- Internal -------------------------------------------------------------

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    this._status = status;
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this._reconnect) return;
    if (this.reconnectTimer !== null) return;
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
        if (ch._internalStatus === "closed") return;
        if (this._status === "connected") {
          ch._setStatus("connecting");
          this._sendOpen(ch);
        } else {
          // WS not connected — queue for when it reconnects.
          this.pendingReopen.add(ch);
        }
      }, delay),
    );
  }

  private sendClose(channelId: number): void {
    const buf = new Uint8Array(5);
    const view = new DataView(buf.buffer);
    view.setUint16(0, MUX_CONTROL, true);
    buf[2] = MUX_C2S_CLOSE;
    view.setUint16(3, channelId, true);
    this._sendRaw(buf);
  }

  private reopenChannels(): void {
    // Reopen channels that were active before the disconnect.
    for (const ch of this.pendingReopen) {
      ch._setStatus("connecting");
      this._sendOpen(ch);
    }
    this.pendingReopen.clear();
  }

  private handleBinaryFrame(data: ArrayBuffer): void {
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
        if (ch) {
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
