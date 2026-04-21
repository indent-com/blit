import { mkdtempSync, rmSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { BunUnixSocketTransport } from "../transports/unix-bun";
import { NodeUnixSocketTransport } from "../transports/unix";
import type { AbstractUnixSocketTransport } from "../transports/unix-base";
import type { UnixSocketTransportOptions } from "../transports/unix-base";

// These tests exercise real AF_UNIX sockets on POSIX.  The transport is
// intentionally local-only, so we skip on platforms where
// `net.connect({ path })` does not talk to a unix socket (Windows uses
// named pipes, not covered by these tests).
const skipOnWin32 = process.platform === "win32";

type Factory = (
  path: string,
  opts?: UnixSocketTransportOptions,
) => AbstractUnixSocketTransport;

// Bun's socket API is only available under the Bun runtime.  Skip the
// suite when running under plain Node/vitest.
const hasBun = typeof (globalThis as { Bun?: unknown }).Bun !== "undefined";

const suites: Array<{ name: string; skip: boolean; make: Factory }> = [
  {
    name: "NodeUnixSocketTransport",
    skip: skipOnWin32,
    make: (p, o) => new NodeUnixSocketTransport(p, o),
  },
  {
    name: "BunUnixSocketTransport",
    skip: skipOnWin32 || !hasBun,
    make: (p, o) => new BunUnixSocketTransport(p, o),
  },
];

for (const { name, skip, make } of suites) {
  const d = skip ? describe.skip : describe;

  d(name, () => {
    let tmp: string;
    let sockPath: string;
    let server: Server;
    let lastClient: Socket | null = null;
    const clientMessages: Buffer[] = [];

    beforeEach(async () => {
      tmp = mkdtempSync(join(tmpdir(), "blit-unix-transport-"));
      sockPath = join(tmp, "blit.sock");
      clientMessages.length = 0;
      lastClient = null;
      server = createServer((socket) => {
        lastClient = socket;
        let buf = Buffer.alloc(0);
        socket.on("data", (chunk: Buffer) => {
          buf = buf.byteLength === 0 ? chunk : Buffer.concat([buf, chunk]);
          while (buf.byteLength >= 4) {
            const len = buf.readUInt32LE(0);
            if (buf.byteLength < 4 + len) break;
            clientMessages.push(Buffer.from(buf.subarray(4, 4 + len)));
            buf = buf.subarray(4 + len);
          }
        });
      });
      await new Promise<void>((resolve) => server.listen(sockPath, resolve));
    });

    afterEach(async () => {
      await new Promise<void>((resolve) => server.close(() => resolve()));
      rmSync(tmp, { recursive: true, force: true });
    });

    function framed(payload: Uint8Array): Buffer {
      const out = Buffer.alloc(4 + payload.byteLength);
      out.writeUInt32LE(payload.byteLength, 0);
      out.set(payload, 4);
      return out;
    }

    async function waitFor<T>(
      probe: () => T | null | undefined,
      timeoutMs = 1000,
    ): Promise<T> {
      const start = Date.now();
      while (Date.now() - start < timeoutMs) {
        const v = probe();
        if (v !== null && v !== undefined && v !== false) return v as T;
        await new Promise((r) => setTimeout(r, 5));
      }
      throw new Error("timeout");
    }

    it("starts disconnected", () => {
      const t = make(sockPath);
      expect(t.status).toBe("disconnected");
      t.close();
    });

    it("connects and reports connected", async () => {
      const t = make(sockPath);
      const statuses: string[] = [];
      t.addEventListener("statuschange", (s) => statuses.push(s));
      t.connect();
      await waitFor(() => t.status === "connected");
      expect(statuses).toContain("connecting");
      expect(statuses).toContain("connected");
      t.close();
    });

    it("sends length-prefixed frames", async () => {
      const t = make(sockPath);
      t.connect();
      await waitFor(() => t.status === "connected");
      t.send(new Uint8Array([1, 2, 3]));
      await waitFor(() => clientMessages.length > 0);
      expect(Array.from(clientMessages[0]!)).toEqual([1, 2, 3]);
      t.close();
    });

    it("decodes length-prefixed frames from the server", async () => {
      const t = make(sockPath);
      const received: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => received.push(d));
      t.connect();
      await waitFor(() => t.status === "connected");
      await waitFor(() => lastClient !== null);
      lastClient!.write(framed(new Uint8Array([0xaa, 0xbb])));
      lastClient!.write(framed(new Uint8Array([0xcc])));
      await waitFor(() => received.length === 2);
      expect(Array.from(new Uint8Array(received[0]!))).toEqual([0xaa, 0xbb]);
      expect(Array.from(new Uint8Array(received[1]!))).toEqual([0xcc]);
      t.close();
    });

    it("handles split reads across frame boundaries", async () => {
      const t = make(sockPath);
      const received: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => received.push(d));
      t.connect();
      await waitFor(() => t.status === "connected");
      await waitFor(() => lastClient !== null);
      const whole = framed(new Uint8Array([1, 2, 3, 4, 5]));
      // Deliver one byte at a time — the framer must wait for the full frame.
      for (let i = 0; i < whole.byteLength; i++) {
        lastClient!.write(whole.subarray(i, i + 1));
      }
      await waitFor(() => received.length === 1);
      expect(Array.from(new Uint8Array(received[0]!))).toEqual([1, 2, 3, 4, 5]);
      t.close();
    });

    it("send() is a no-op before connected", async () => {
      const t = make(sockPath);
      // Intentionally do not call connect().
      t.send(new Uint8Array([1]));
      await new Promise((r) => setTimeout(r, 20));
      expect(clientMessages).toHaveLength(0);
      t.close();
    });

    it("close() prevents reconnect", async () => {
      const t = make(sockPath, { reconnectDelay: 20 });
      t.connect();
      await waitFor(() => t.status === "connected");
      t.close();
      expect(t.status).toBe("closed");
      // Wait past the reconnect interval and confirm no new connection.
      const before = server.connections ?? 0;
      await new Promise((r) => setTimeout(r, 100));
      const after = server.connections ?? 0;
      expect(after).toBeLessThanOrEqual(before);
    });

    it("reconnects after peer close when reconnect enabled", async () => {
      const t = make(sockPath, { reconnectDelay: 20 });
      t.connect();
      await waitFor(() => t.status === "connected");
      lastClient!.destroy();
      await waitFor(() => t.status === "disconnected");
      await waitFor(() => t.status === "connected", 2000);
      t.close();
    });

    it("reconnect:false disables reconnection", async () => {
      const t = make(sockPath, {
        reconnect: false,
        reconnectDelay: 10,
      });
      t.connect();
      await waitFor(() => t.status === "connected");
      const firstClient = lastClient!;
      firstClient.destroy();
      await waitFor(() => t.status === "disconnected");
      await new Promise((r) => setTimeout(r, 100));
      expect(t.status).toBe("disconnected");
      t.close();
    });

    it("reports error for unreachable socket path", async () => {
      const missing = join(tmp, "does-not-exist.sock");
      const t = make(missing, {
        reconnect: false,
        connectTimeoutMs: 200,
      });
      t.connect();
      await waitFor(() => t.status === "error" || t.status === "disconnected");
      expect(t.lastError).not.toBeNull();
      t.close();
    });
  });
}
