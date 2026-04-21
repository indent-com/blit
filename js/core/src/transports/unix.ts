import { Buffer } from "node:buffer";
import { connect as netConnect, type Socket } from "node:net";

import type {
  BlitTransport,
  BlitTransportOptions,
  ConnectionStatus,
} from "../types";

export interface UnixSocketTransportOptions extends BlitTransportOptions {}

/**
 * Unix-domain-socket transport for the local `blit server` IPC socket.
 *
 * Node-only.  The server frames every message as a little-endian `u32`
 * length prefix followed by the payload (see `crates/server/src/lib.rs`
 * `read_frame`/`write_frame`).  Authentication is filesystem-based:
 * the socket is created with `0700` permissions, so any process that
 * can `connect()` is authorised.
 *
 * On Windows the `path` may be a named pipe (e.g.
 * `\\.\pipe\blit-<user>`).  Node's `net.connect({ path })` handles both.
 */
export class UnixSocketTransport implements BlitTransport {
  private socket: Socket | null = null;
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

  private recvBuf: Buffer = Buffer.alloc(0);

  private readonly path: string;
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
    const socket = this.socket;
    if (!socket || this._status !== "connected") return;
    const header = Buffer.alloc(4);
    header.writeUInt32LE(data.byteLength, 0);
    socket.write(header);
    socket.write(Buffer.from(data.buffer, data.byteOffset, data.byteLength));
  }

  close(): void {
    this.disposed = true;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.teardownSocket();
    this.setStatus("closed");
  }

  reconnect(): void {
    if (this.disposed) return;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    this.teardownSocket();
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

    const socket = netConnect({ path: this.path });
    this.socket = socket;
    this.recvBuf = Buffer.alloc(0);

    this.clearConnectTimer();
    this.connectTimer = setTimeout(() => {
      this.connectTimer = null;
      if (this.socket !== socket || this.disposed) return;
      if (this._status === "connecting") {
        this.lastError = "connect timeout";
        this.setStatus("error");
        socket.destroy();
        this.socket = null;
        this.scheduleReconnect();
      }
    }, this.connectTimeoutMs);

    socket.on("connect", () => {
      if (this.socket !== socket || this.disposed) return;
      this.clearConnectTimer();
      this.lastError = null;
      this.currentDelay = this.initialDelay;
      this.setStatus("connected");
    });

    socket.on("data", (chunk: Buffer) => {
      if (this.socket !== socket || this.disposed) return;
      this.recvBuf =
        this.recvBuf.byteLength === 0
          ? chunk
          : Buffer.concat([this.recvBuf, chunk]);
      while (this.recvBuf.byteLength >= 4) {
        const len = this.recvBuf.readUInt32LE(0);
        if (this.recvBuf.byteLength < 4 + len) break;
        const frame = this.recvBuf.subarray(4, 4 + len);
        this.recvBuf = this.recvBuf.subarray(4 + len);
        const ab = new ArrayBuffer(frame.byteLength);
        new Uint8Array(ab).set(frame);
        for (const l of this.messageListeners) l(ab);
      }
    });

    socket.on("error", (err: Error) => {
      if (this.socket !== socket || this.disposed) return;
      this.lastError = err.message;
      this.setStatus("error");
    });

    socket.on("close", () => {
      if (this.socket !== socket || this.disposed) return;
      this.clearConnectTimer();
      this.socket = null;
      this.recvBuf = Buffer.alloc(0);
      this.setStatus("disconnected");
      this.scheduleReconnect();
    });
  }

  private teardownSocket(): void {
    const socket = this.socket;
    if (socket) {
      socket.removeAllListeners();
      socket.destroy();
      this.socket = null;
    }
    this.recvBuf = Buffer.alloc(0);
  }

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
