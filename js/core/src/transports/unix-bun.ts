/// <reference types="bun" />

import {
  AbstractUnixSocketTransport,
  type UnixSocketTransportOptions,
} from "./unix-base";

export type { UnixSocketTransportOptions } from "./unix-base";

type BunSocket = import("bun").Socket<{ attempt: symbol }>;

/**
 * Bun-native unix-domain-socket transport for `blit server` IPC.
 *
 * Uses `Bun.connect({ unix })` directly — lower overhead than the
 * Node adapter and no `node:*` imports.  See {@link NodeUnixSocketTransport}
 * for the Node.js equivalent.
 */
export class BunUnixSocketTransport extends AbstractUnixSocketTransport {
  private socket: BunSocket | null = null;

  constructor(path: string, options?: UnixSocketTransportOptions) {
    super(path, options);
  }

  protected openRawSocket(attempt: symbol): void {
    // Bun.connect() is async; we kick it off and wire events through the
    // shared base class.  Any error before the socket resolves is
    // reported via onRawError + onRawClose.
    Bun.connect<{ attempt: symbol }>({
      unix: this.path,
      data: { attempt },
      socket: {
        open: (sock) => {
          if (this.currentAttempt !== attempt) {
            sock.end();
            return;
          }
          this.socket = sock;
          this.onRawConnect(attempt);
        },
        data: (_sock, chunk: Uint8Array) => this.ingestChunk(attempt, chunk),
        error: (_sock, err: Error) => this.onRawError(attempt, err.message),
        close: (sock) => {
          if (this.socket === sock) this.socket = null;
          this.onRawClose(attempt);
        },
      },
    }).catch((err: unknown) => {
      const msg = err instanceof Error ? err.message : String(err);
      this.onRawError(attempt, msg);
      this.onRawClose(attempt);
    });
  }

  protected writeRaw(data: Uint8Array): void {
    this.socket?.write(data);
  }

  protected destroyRawSocket(): void {
    const socket = this.socket;
    if (!socket) return;
    this.socket = null;
    socket.end();
  }
}
