import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { FEATURE_NET } from "@blit-sh/core";
import { S2C_HELLO, S2C_PING, S2C_READY } from "@blit-sh/core/types";
import { RELAY_INACTIVITY_TIMEOUT_MS, RelayPool } from "../conn";

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
  closeCalls = 0;
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  open(): void {
    this.readyState = FakeWebSocket.OPEN;
    this.onopen?.();
  }

  message(data: string | ArrayBuffer): void {
    this.onmessage?.({ data } as MessageEvent);
  }

  send(data: unknown): void {
    this.sent.push(data);
  }

  close(): void {
    if (this.readyState === FakeWebSocket.CLOSED) return;
    this.closeCalls++;
    this.readyState = FakeWebSocket.CLOSED;
    this.onclose?.();
  }
}

function authenticate(socket: FakeWebSocket): void {
  socket.open();
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

describe("RelayPool connection liveness", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    FakeWebSocket.instances = [];
    vi.stubGlobal("WebSocket", FakeWebSocket);
    Object.assign(FakeWebSocket, {
      CONNECTING: 0,
      OPEN: 1,
      CLOSING: 2,
      CLOSED: 3,
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("fails pending streams and replaces a silent half-open socket", async () => {
    const pool = new RelayPool();
    pool.setPassphrase("secret");

    const firstOpen = pool.open("local", "127.0.0.1", 8080);
    const firstSocket = FakeWebSocket.instances[0]!;
    authenticate(firstSocket);
    const firstStream = await firstOpen;

    await vi.advanceTimersByTimeAsync(RELAY_INACTIVITY_TIMEOUT_MS);

    await expect(firstStream.opened).rejects.toThrow(
      "relay connection inactive",
    );
    expect(firstSocket.closeCalls).toBe(1);

    const secondOpen = pool.open("local", "127.0.0.1", 8081);
    await vi.advanceTimersByTimeAsync(0);
    expect(FakeWebSocket.instances).toHaveLength(2);
    authenticate(FakeWebSocket.instances[1]!);
    await secondOpen;
  });

  // A restarted worker holds no credential; the pool must ask before failing,
  // for every caller — patching individual call sites is how the WebSocket
  // path got left out and previewed apps stopped recovering.
  it("asks for a credential on demand instead of failing an empty pool", async () => {
    const pool: RelayPool = new RelayPool(async () =>
      pool.setPassphrase("secret"),
    );
    const opening = pool.open("local", "127.0.0.1", 8080);
    await vi.advanceTimersByTimeAsync(0);
    expect(FakeWebSocket.instances).toHaveLength(1);
    authenticate(FakeWebSocket.instances[0]!);
    await opening;
  });

  it("still fails legibly when the ask produces nothing", async () => {
    const pool = new RelayPool(async () => {});
    await expect(pool.open("local", "127.0.0.1", 8080)).rejects.toThrow(
      "no passphrase yet",
    );
    expect(FakeWebSocket.instances).toHaveLength(0);
  });

  it("refreshes the inactivity deadline on server pings", async () => {
    const pool = new RelayPool();
    pool.setPassphrase("secret");

    const opening = pool.open("local", "127.0.0.1", 8080);
    const socket = FakeWebSocket.instances[0]!;
    authenticate(socket);
    await opening;

    await vi.advanceTimersByTimeAsync(RELAY_INACTIVITY_TIMEOUT_MS - 5_000);
    socket.message(new Uint8Array([S2C_PING]).buffer);
    await vi.advanceTimersByTimeAsync(RELAY_INACTIVITY_TIMEOUT_MS - 5_000);
    expect(socket.closeCalls).toBe(0);

    await vi.advanceTimersByTimeAsync(5_000);
    expect(socket.closeCalls).toBe(1);
  });
});
