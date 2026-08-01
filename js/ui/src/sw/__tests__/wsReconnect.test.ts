/**
 * A relayed WebSocket must survive worker restarts. The browser terminates an
 * idle service worker (and caps `waitUntil`), so the worker that accepted a
 * socket is routinely not the one that sees the reconnect — and the restarted
 * worker's pool holds no passphrase. The HTTP path self-heals by asking a page
 * (`requestPassphrase`); these tests pin the WebSocket path to the same
 * behavior, because an app that only speaks WebSocket once loaded — blit
 * itself in a preview pane — would otherwise reconnect into "no passphrase
 * yet" forever.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FEATURE_NET, type PreviewTarget } from "@blit-sh/core";
import { S2C_HELLO, S2C_READY } from "@blit-sh/core/types";
import { pipeWebSocket } from "../index";

class FakeWebSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  static instances: FakeWebSocket[] = [];

  readonly url: string;
  binaryType = "";
  readyState = FakeWebSocket.CONNECTING;
  sent: unknown[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  message(data: string | ArrayBuffer): void {
    this.onmessage?.({ data } as MessageEvent);
  }

  send(data: unknown): void {
    this.sent.push(data);
  }

  close(): void {
    if (this.readyState === FakeWebSocket.CLOSED) return;
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.();
  }
}

function authenticate(socket: FakeWebSocket): void {
  socket.readyState = FakeWebSocket.OPEN;
  socket.onopen?.();
  socket.message("ok");
  socket.message(
    new Uint8Array([
      S2C_HELLO,
      0,
      0,
      FEATURE_NET & 0xff,
      (FEATURE_NET >>> 8) & 0xff,
      (FEATURE_NET >>> 16) & 0xff,
      (FEATURE_NET >>> 24) & 0xff,
    ]).buffer,
  );
  socket.message(new Uint8Array([S2C_READY]).buffer);
}

const target: PreviewTarget = {
  dest: "local",
  scheme: "http",
  host: "localhost",
  port: 7777,
};

/** The shim's end of the pipe, capturing what the worker tells it. */
function shimPort(): { received: unknown[]; port: MessagePort } {
  const received: unknown[] = [];
  return {
    received,
    port: {
      postMessage: (message: unknown) => received.push(message),
      close: () => {},
      onmessage: null,
    } as unknown as MessagePort,
  };
}

function closedSentinel(received: unknown[]): boolean {
  return received.some(
    (m) => !!(m as { blitClosed?: boolean } | null)?.blitClosed,
  );
}

describe("relayed WebSocket after a worker restart", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  // The module-level pool starts unauthenticated, exactly like a restarted
  // worker. This test must run before any that authenticates it: setting the
  // credential is one-way.
  it("asks a page for the credential, and still reports close when none answers", async () => {
    const asked: unknown[] = [];
    vi.stubGlobal("clients", {
      matchAll: async () => [
        { postMessage: (message: unknown) => asked.push(message) },
      ],
    });

    const { received, port } = shimPort();
    const done = pipeWebSocket(target, port);
    await vi.advanceTimersByTimeAsync(0);
    expect(
      asked.some(
        (m) =>
          (m as { type?: string } | null)?.type === "blit-need-passphrase",
      ),
      "the worker must ask rather than fail outright",
    ).toBe(true);

    // No page answers (e.g. every tab is gone): the shim must still be told,
    // or its socket sits in CONNECTING until its own timeout.
    await vi.advanceTimersByTimeAsync(2_000);
    await done;
    expect(closedSentinel(received)).toBe(true);
    expect(FakeWebSocket.instances).toHaveLength(0);
  });

  it("recovers once a page answers with the passphrase", async () => {
    vi.stubGlobal("clients", {
      matchAll: async () => [
        {
          postMessage: () => {
            // The page's `watchPreviewWorker` handler, inlined: it answers
            // blit-need-passphrase by posting the credential back, which
            // arrives as a message event on the worker's global scope.
            self.dispatchEvent(
              new MessageEvent("message", {
                data: { type: "blit-passphrase", passphrase: "secret" },
              }),
            );
          },
        },
      ],
    });

    const { received, port } = shimPort();
    void pipeWebSocket(target, port);
    await vi.advanceTimersByTimeAsync(100);

    // The credential arrived, so the reconnect dials the relay instead of
    // dying on an empty pool.
    expect(FakeWebSocket.instances).toHaveLength(1);
    const socket = FakeWebSocket.instances[0]!;
    expect(socket.url).toContain("/d/local");
    authenticate(socket);
    await vi.advanceTimersByTimeAsync(0);
    expect(closedSentinel(received)).toBe(false);
    // NET_OPEN went out for the shim's stream.
    expect(socket.sent.length).toBeGreaterThan(1);
  });
});
