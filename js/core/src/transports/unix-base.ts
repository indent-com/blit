import type {
  BlitTransport,
  BlitTransportOptions,
  ConnectionStatus,
} from "../types";

export interface UnixSocketTransportOptions extends BlitTransportOptions {}

/**
 * Shared implementation of the blit local-IPC framing protocol
 * (little-endian `u32` length prefix, see `crates/server/src/lib.rs`
 * `read_frame`/`write_frame`).
 *
 * Concrete subclasses plug in a socket backend (Node's `net` module,
 * Bun's `Bun.connect`, ...) via {@link openRawSocket}.  They receive
 * bytes through {@link ingestChunk} and must report lifecycle events
 * via {@link onRawConnect}, {@link onRawClose} and {@link onRawError}.
 */
export abstract class AbstractUnixSocketTransport implements BlitTransport {
  private _status: ConnectionStatus = "disconnected";
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private connectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;
  private messageListeners = new Set<(data: ArrayBuffer) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  /** Local sockets have no passphrase authentication. */
  authRejected = false;
  lastError: string | null = null;

  /** Incoming byte accumulator. */
  private recvBuf = new Uint8Array(0);
  /** Sentinel that tracks which socket attempt events belong to. */
  protected currentAttempt: symbol | null = null;

  protected readonly path: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;
  private readonly connectTimeoutMs: number;

  constructor(path: string, options?: UnixSocketTransportOptions) {
    this.path = path;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.connectTimeoutMs = options?.connectTimeoutMs ?? 10000;
    this.currentDelay = this.initialDelay;
  }

  get status(): ConnectionStatus {
    return this._status;
  }

  send(data: Uint8Array): void {
    if (this._status !== "connected") return;
    const header = new Uint8Array(4);
    const len = data.byteLength;
    header[0] = len & 0xff;
    header[1] = (len >> 8) & 0xff;
    header[2] = (len >> 16) & 0xff;
    header[3] = (len >>> 24) & 0xff;
    this.writeRaw(header);
    this.writeRaw(data);
  }

  close(): void {
    this.disposed = true;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.destroyRawSocket();
    this.recvBuf = new Uint8Array(0);
    this.setStatus("closed");
  }

  reconnect(): void {
    if (this.disposed) return;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.destroyRawSocket();
    this.recvBuf = new Uint8Array(0);
    this.currentDelay = this.initialDelay;
    this.setStatus("disconnected");
    this.connect();
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

  connect(): void {
    if (this.disposed) return;
    if (this.reconnectTimer !== null) {
      this.clearReconnectTimer();
      this.currentDelay = this.initialDelay;
    }
    if (
      this._status === "connecting" ||
      this._status === "authenticating" ||
      this._status === "connected"
    ) {
      return;
    }
    this.setStatus("connecting");
    this.recvBuf = new Uint8Array(0);

    const attempt = Symbol("unix-attempt");
    this.currentAttempt = attempt;

    this.clearConnectTimer();
    this.connectTimer = setTimeout(() => {
      this.connectTimer = null;
      if (this.currentAttempt !== attempt || this.disposed) return;
      if (this._status === "connecting") {
        this.lastError = "connect timeout";
        this.setStatus("error");
        this.destroyRawSocket();
        this.currentAttempt = null;
        this.scheduleReconnect();
      }
    }, this.connectTimeoutMs);

    this.openRawSocket(attempt);
  }

  // ---- concrete-subclass API ----

  /**
   * Open a backend-specific socket connected to {@link path}.  The
   * subclass must call {@link onRawConnect} on successful connect,
   * {@link ingestChunk} on every chunk, {@link onRawClose} on peer
   * close and {@link onRawError} on error.  The {@link attempt}
   * argument must be echoed back so late events from a superseded
   * socket can be discarded.
   */
  protected abstract openRawSocket(attempt: symbol): void;

  /** Write raw bytes to the currently open socket, if any. */
  protected abstract writeRaw(data: Uint8Array): void;

  /** Close and discard the current socket without firing events. */
  protected abstract destroyRawSocket(): void;

  // ---- helpers for subclasses ----

  protected onRawConnect(attempt: symbol): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    this.clearConnectTimer();
    this.lastError = null;
    this.currentDelay = this.initialDelay;
    this.setStatus("connected");
  }

  protected ingestChunk(attempt: symbol, chunk: Uint8Array): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    if (chunk.byteLength === 0) return;
    if (this.recvBuf.byteLength === 0) {
      const copy = new Uint8Array(chunk.byteLength);
      copy.set(chunk);
      this.recvBuf = copy;
    } else {
      const merged = new Uint8Array(this.recvBuf.byteLength + chunk.byteLength);
      merged.set(this.recvBuf, 0);
      merged.set(chunk, this.recvBuf.byteLength);
      this.recvBuf = merged;
    }
    while (this.recvBuf.byteLength >= 4) {
      const b = this.recvBuf;
      const len = b[0]! | (b[1]! << 8) | (b[2]! << 16) | (b[3]! * 0x01000000);
      if (this.recvBuf.byteLength < 4 + len) break;
      const ab = new ArrayBuffer(len);
      new Uint8Array(ab).set(this.recvBuf.subarray(4, 4 + len));
      this.recvBuf = this.recvBuf.subarray(4 + len);
      for (const l of this.messageListeners) l(ab);
    }
  }

  protected onRawError(attempt: symbol, message: string): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    this.lastError = message;
    this.setStatus("error");
  }

  protected onRawClose(attempt: symbol): void {
    if (this.currentAttempt !== attempt || this.disposed) return;
    this.clearConnectTimer();
    this.currentAttempt = null;
    this.recvBuf = new Uint8Array(0);
    this.setStatus("disconnected");
    this.scheduleReconnect();
  }

  // ---- internals ----

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    this._status = status;
    for (const l of this.statusListeners) l(status);
  }

  private clearConnectTimer(): void {
    if (this.connectTimer !== null) {
      clearTimeout(this.connectTimer);
      this.connectTimer = null;
    }
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this._reconnect) return;
    this.clearReconnectTimer();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) this.connect();
    }, this.currentDelay);
    this.currentDelay = Math.min(
      this.currentDelay * this.backoff,
      this.maxDelay,
    );
  }
}
