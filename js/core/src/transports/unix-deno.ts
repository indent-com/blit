import {
  AbstractUnixSocketTransport,
  type UnixSocketTransportOptions,
} from "./unix-base";

export type { UnixSocketTransportOptions } from "./unix-base";

// Minimal structural typing for the subset of the Deno global we use, so
// this file compiles under Node/Bun tsconfigs without @types/deno.
interface DenoUnixConn {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array>;
  close(): void;
}
interface DenoLike {
  connect(opts: { transport: "unix"; path: string }): Promise<DenoUnixConn>;
}
declare const Deno: DenoLike | undefined;

/**
 * Deno-native unix-domain-socket transport for `blit server` IPC.
 *
 * Uses `Deno.connect({ transport: "unix" })` directly — no `node:*`
 * imports, no `npm:` shim.  See {@link NodeUnixSocketTransport} and
 * {@link BunUnixSocketTransport} for the other runtimes.
 *
 * Requires the `--unstable-net` (or `--unstable`) flag on Deno < 2 and
 * the `--allow-read --allow-write` permissions for the socket path.
 */
export class DenoUnixSocketTransport extends AbstractUnixSocketTransport {
  private conn: DenoUnixConn | null = null;
  private writer: WritableStreamDefaultWriter<Uint8Array> | null = null;

  constructor(path: string, options?: UnixSocketTransportOptions) {
    super(path, options);
  }

  protected openRawSocket(attempt: symbol): void {
    if (typeof Deno === "undefined") {
      this.onRawError(attempt, "Deno global not available");
      this.onRawClose(attempt);
      return;
    }
    Deno.connect({ transport: "unix", path: this.path })
      .then((conn) => {
        if (this.currentAttempt !== attempt) {
          conn.close();
          return;
        }
        this.conn = conn;
        this.writer = conn.writable.getWriter();
        this.onRawConnect(attempt);
        this.pump(attempt, conn.readable.getReader()).catch((err: unknown) => {
          const msg = err instanceof Error ? err.message : String(err);
          this.onRawError(attempt, msg);
          this.onRawClose(attempt);
        });
      })
      .catch((err: unknown) => {
        const msg = err instanceof Error ? err.message : String(err);
        this.onRawError(attempt, msg);
        this.onRawClose(attempt);
      });
  }

  private async pump(
    attempt: symbol,
    reader: ReadableStreamDefaultReader<Uint8Array>,
  ): Promise<void> {
    try {
      while (true) {
        const { value, done } = await reader.read();
        if (done) break;
        if (this.currentAttempt !== attempt) break;
        if (value) this.ingestChunk(attempt, value);
      }
    } finally {
      try {
        reader.releaseLock();
      } catch {
        /* ignore */
      }
      this.onRawClose(attempt);
    }
  }

  protected writeRaw(data: Uint8Array): void {
    // Writer is async but we fire-and-forget to match the Node/Bun backends.
    // Errors are surfaced via the reader loop / close handlers.
    void this.writer?.write(data).catch(() => {});
  }

  protected destroyRawSocket(): void {
    const conn = this.conn;
    if (!conn) return;
    this.conn = null;
    try {
      this.writer?.releaseLock();
    } catch {
      /* ignore */
    }
    this.writer = null;
    try {
      conn.close();
    } catch {
      /* ignore */
    }
  }
}
