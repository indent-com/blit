/** One authenticated blit connection per destination, owned by the service worker (docs/design/net.md § Where the connection lives). */

import {
  FEATURE_NET,
  NetStreams,
  type NetOpenOptions,
  type NetStream,
} from "@blit-sh/core";
// Imported, never redeclared: a hand-copied opcode is a bug that presents as a connection that hangs until it times out, with nothing on the wire to blame.
import { S2C_FRAGMENT, S2C_HELLO, S2C_READY } from "@blit-sh/core/types";

const FRAGMENT_FLAG_LAST = 1 << 0;

const AUTH_TIMEOUT_MS = 10_000;

// The server sends an application-level ping every 10 seconds. Allow three
// missed pings plus a little scheduling jitter before declaring the relay dead.
export const RELAY_INACTIVITY_TIMEOUT_MS = 35_000;
// A service worker timer can resume before WebSocket messages queued while the
// browser was suspended. Give one ping interval (plus jitter) for one to arrive.
export const RELAY_RESUME_GRACE_MS = 15_000;
const WATCHDOG_LATE_BY_MS = 1_000;

export class RelayUnavailable extends Error {}

/** A live connection to one destination. */
class DestConnection {
  readonly streams: NetStreams;
  private readonly socket: WebSocket;
  private pending = new Uint8Array(0);
  private failed = false;
  private watchdog: ReturnType<typeof setTimeout> | null = null;
  private watchdogDueAt = 0;
  private resumeGrace = false;

  private constructor(socket: WebSocket) {
    this.socket = socket;
    this.streams = new NetStreams((msg) => {
      // A view into a larger buffer would send the whole buffer; copy so the frame is exactly the message.
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(new Uint8Array(msg).buffer as ArrayBuffer);
      }
    });
    socket.onmessage = (event) => this.onFrame(event);
    socket.onclose = () => this.fail(new Error("relay connection lost"));
    socket.onerror = () => this.fail(new Error("relay connection lost"));
    this.armWatchdog(RELAY_INACTIVITY_TIMEOUT_MS);
  }

  private onFrame(event: MessageEvent): void {
    if (this.failed) return;
    // Any server frame proves transport liveness. In particular, the standard
    // server sends S2C_PING every 10 seconds, which NetStreams safely ignores.
    this.resumeGrace = false;
    this.armWatchdog(RELAY_INACTIVITY_TIMEOUT_MS);
    if (typeof event.data === "string") return;
    const bytes = new Uint8Array(event.data as ArrayBuffer);
    if (bytes.length === 0) return;
    if (bytes[0] === S2C_FRAGMENT) {
      if (bytes.length < 2) return;
      const merged = new Uint8Array(this.pending.length + bytes.length - 2);
      merged.set(this.pending, 0);
      merged.set(bytes.subarray(2), this.pending.length);
      this.pending = merged;
      if (bytes[1] & FRAGMENT_FLAG_LAST) {
        const message = this.pending;
        this.pending = new Uint8Array(0);
        this.streams.handleMessage(message);
      }
      return;
    }
    this.streams.handleMessage(bytes);
  }

  private armWatchdog(delay: number): void {
    if (this.failed) return;
    if (this.watchdog !== null) clearTimeout(this.watchdog);
    this.watchdogDueAt = Date.now() + delay;
    this.watchdog = setTimeout(() => {
      this.watchdog = null;
      const lateBy = Date.now() - this.watchdogDueAt;
      if (lateBy >= WATCHDOG_LATE_BY_MS && !this.resumeGrace) {
        // Background throttling/suspension can delay both this timer and an
        // already queued ping. A timely second check distinguishes that from a
        // genuinely half-open socket without keeping it indefinitely.
        this.resumeGrace = true;
        this.armWatchdog(RELAY_RESUME_GRACE_MS);
        return;
      }
      this.fail(new Error("relay connection inactive"));
    }, delay);
  }

  private fail(err: Error): void {
    if (this.failed) return;
    this.failed = true;
    if (this.watchdog !== null) {
      clearTimeout(this.watchdog);
      this.watchdog = null;
    }
    this.streams.reset(err);
    if (
      this.socket.readyState !== WebSocket.CLOSING &&
      this.socket.readyState !== WebSocket.CLOSED
    ) {
      try {
        this.socket.close();
      } catch {
        // Already gone.
      }
    }
  }

  get closed(): boolean {
    return (
      this.failed ||
      this.socket.readyState === WebSocket.CLOSING ||
      this.socket.readyState === WebSocket.CLOSED
    );
  }

  close(): void {
    this.fail(new Error("relay connection closed"));
  }

  open(host: string, port: number, options: NetOpenOptions): NetStream {
    // Keep this check adjacent to NetStreams.open: service-worker callbacks do
    // not interleave synchronous code, so a watchdog cannot fail the socket
    // between this check and sending NET_OPEN.
    if (this.closed) throw new Error("relay connection lost");
    return this.streams.open(host, port, options);
  }

  /** Connect, authenticate, and refuse early if the server has no relay — an old server drops `NET_OPEN` silently and every request would hang. */
  static connect(url: string, passphrase: string): Promise<DestConnection> {
    return new Promise((resolve, reject) => {
      let socket: WebSocket;
      try {
        socket = new WebSocket(url);
      } catch (err) {
        reject(err);
        return;
      }
      socket.binaryType = "arraybuffer";
      let settled = false;
      let features = 0;
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        socket.close();
        reject(new Error("timed out connecting to the relay"));
      }, AUTH_TIMEOUT_MS);
      const finish = (err: Error | null, conn?: DestConnection) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        if (err) {
          socket.close();
          reject(err);
        } else {
          resolve(conn!);
        }
      };
      socket.onopen = () => socket.send(passphrase);
      socket.onerror = () => finish(new Error("relay connection failed"));
      socket.onclose = () => finish(new Error("relay closed the connection"));
      socket.onmessage = (event) => {
        if (typeof event.data === "string") {
          if (event.data === "ok") return;
          if (event.data === "busy") {
            finish(
              new Error("relay busy — too many recent connection attempts"),
            );
            return;
          }
          finish(
            new Error(
              event.data === "auth"
                ? "authentication failed"
                : event.data.replace(/^error:/, ""),
            ),
          );
          return;
        }
        const bytes = new Uint8Array(event.data as ArrayBuffer);
        if (bytes.length === 0) return;
        if (bytes[0] === S2C_HELLO && bytes.length >= 7) {
          features =
            bytes[3] | (bytes[4] << 8) | (bytes[5] << 16) | (bytes[6] << 24);
          return;
        }
        if (bytes[0] === S2C_READY) {
          if ((features & FEATURE_NET) === 0) {
            finish(
              new RelayUnavailable(
                "this blit server has no relay — upgrade it to preview web apps",
              ),
            );
            return;
          }
          // Hand the socket over: from here the DestConnection owns its handlers, and everything is a NET message.
          finish(null, new DestConnection(socket));
        }
      };
    });
  }
}

/** Connections by destination, opened on demand and dropped when they die. */
export class RelayPool {
  private passphrase: string | null = null;
  private readonly conns = new Map<string, Promise<DestConnection>>();

  /** `requestCredential` is consulted whenever a connection is needed and no
   *  passphrase is held — a restarted worker asking a page for it, rather than
   *  failing callers until one happens to re-arm it. It lives here and not
   *  with the callers because every relayed request and socket needs it: the
   *  one path that lacked it (relayed WebSockets) answered every reconnect of
   *  a previewed app with "no passphrase yet" after a worker restart. */
  constructor(private readonly requestCredential?: () => Promise<unknown>) {}

  setPassphrase(passphrase: string): void {
    if (passphrase && passphrase !== this.passphrase) {
      this.passphrase = passphrase;
      // A changed credential invalidates every socket authenticated with the old one.
      for (const [, conn] of this.conns) {
        conn.then((c) => c.close()).catch(() => {});
      }
      this.conns.clear();
    }
  }

  get authenticated(): boolean {
    return this.passphrase !== null;
  }

  /** Open a relayed socket to `host:port` on `dest`. */
  async open(
    dest: string,
    host: string,
    port: number,
    options: NetOpenOptions = {},
  ): Promise<NetStream> {
    const conn = await this.connection(dest);
    return conn.open(host, port, options);
  }

  private async connection(dest: string): Promise<DestConnection> {
    const existing = this.conns.get(dest);
    if (existing) {
      try {
        const conn = await existing;
        if (!conn.closed) return conn;
      } catch {
        // Fall through and retry once; a stale rejection must not be sticky.
      }
      this.conns.delete(dest);
    }
    if (!this.passphrase && this.requestCredential) {
      // A failed ask is not an error of its own: the message below says what
      // is actually missing.
      await this.requestCredential().catch(() => {});
    }
    if (!this.passphrase) {
      throw new Error("no passphrase yet — open the blit UI in a tab");
    }
    const url = destUrl(dest);
    const attempt = DestConnection.connect(url, this.passphrase);
    this.conns.set(dest, attempt);
    try {
      return await attempt;
    } catch (err) {
      this.conns.delete(dest);
      throw err;
    }
  }
}

/** The gateway's per-destination WebSocket path (`/d/{name}`). */
function destUrl(dest: string): string {
  const proto = self.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${self.location.host}/d/${encodeURIComponent(dest)}`;
}
