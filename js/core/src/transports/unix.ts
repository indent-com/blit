import { connect as netConnect, type Socket } from "node:net";

import {
  AbstractUnixSocketTransport,
  type UnixSocketTransportOptions,
} from "./unix-base";

export type { UnixSocketTransportOptions } from "./unix-base";

/**
 * Node.js unix-domain-socket transport for `blit server` IPC.
 *
 * Uses `node:net` so it works in Node and in any runtime that ships a
 * `node:net` compatibility layer (Deno, edge runtimes with Node compat).
 * Authentication is filesystem-based (the server creates the socket with
 * `0700` permissions).  On Windows the `path` may be a named pipe
 * (e.g. `\\.\pipe\blit-<user>`) — `net.connect({ path })` handles both.
 *
 * If you're running on Bun, prefer {@link BunUnixSocketTransport} which
 * uses `Bun.connect` directly and avoids `node:net`.
 */
export class NodeUnixSocketTransport extends AbstractUnixSocketTransport {
  private socket: Socket | null = null;

  constructor(path: string, options?: UnixSocketTransportOptions) {
    super(path, options);
  }

  protected openRawSocket(attempt: symbol): void {
    const socket = netConnect({ path: this.path });
    this.socket = socket;

    socket.on("connect", () => this.onRawConnect(attempt));
    socket.on("data", (chunk: Uint8Array) => this.ingestChunk(attempt, chunk));
    socket.on("error", (err: Error) => this.onRawError(attempt, err.message));
    socket.on("close", () => {
      if (this.socket === socket) this.socket = null;
      this.onRawClose(attempt);
    });
  }

  protected writeRaw(data: Uint8Array): void {
    this.socket?.write(data);
  }

  protected destroyRawSocket(): void {
    const socket = this.socket;
    if (!socket) return;
    socket.removeAllListeners();
    socket.destroy();
    this.socket = null;
  }
}

