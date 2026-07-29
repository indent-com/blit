import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { MuxTransport } from "../transports/mux";

class MockWebSocket {
  static CONNECTING = 0 as const;
  static OPEN = 1 as const;
  static CLOSING = 2 as const;
  static CLOSED = 3 as const;

  static instances: MockWebSocket[] = [];

  readonly url: string;
  binaryType = "blob";
  readyState: number = MockWebSocket.CONNECTING;
  sentData: (string | Uint8Array | ArrayBuffer)[] = [];

  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }

  send(data: string | Uint8Array | ArrayBuffer) {
    this.sentData.push(data);
  }

  close() {
    this.readyState = MockWebSocket.CLOSING;
    const handler = this.onclose;
    this.readyState = MockWebSocket.CLOSED;
    handler?.({} as CloseEvent);
  }

  simulateOpen() {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.({} as Event);
  }

  simulateMessage(data: string | ArrayBuffer) {
    this.onmessage?.({ data } as MessageEvent);
  }
}

class NeverReadyWebTransport {
  static instances: NeverReadyWebTransport[] = [];
  readonly ready = new Promise<void>(() => {});
  readonly closed = new Promise<WebTransportCloseInfo>(() => {});
  closedByClient = false;

  constructor(
    readonly url: string,
    readonly options?: WebTransportOptions,
  ) {
    NeverReadyWebTransport.instances.push(this);
  }

  close() {
    this.closedByClient = true;
  }

  createBidirectionalStream(): Promise<WebTransportBidirectionalStream> {
    throw new Error("not ready");
  }
}

function latestSocket(): MockWebSocket {
  return MockWebSocket.instances[MockWebSocket.instances.length - 1];
}

function controlFrame(opcode: number, ch: number): ArrayBuffer {
  const buf = new Uint8Array(5);
  const view = new DataView(buf.buffer);
  view.setUint16(0, 0xffff, true);
  buf[2] = opcode;
  view.setUint16(3, ch, true);
  return buf.buffer;
}

function decodeControl(data: string | Uint8Array | ArrayBuffer) {
  expect(typeof data).not.toBe("string");
  const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  expect(view.getUint16(0, true)).toBe(0xffff);
  return {
    opcode: bytes[2],
    channelId: view.getUint16(3, true),
  };
}

function sentControlFrames(ws = latestSocket()) {
  return ws.sentData
    .filter(
      (data): data is Uint8Array | ArrayBuffer => typeof data !== "string",
    )
    .map(decodeControl);
}

describe("MuxTransport", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    MockWebSocket.instances = [];
    NeverReadyWebTransport.instances = [];
    vi.stubGlobal("WebSocket", MockWebSocket);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("falls back from WebTransport to WebSocket after the configured timeout", async () => {
    vi.stubGlobal("WebTransport", NeverReadyWebTransport);
    const mux = new MuxTransport("ws://host/mux", "secret", {
      wtUrl: "https://host/mux",
      wtConnectTimeoutMs: 25,
    });

    mux.connect();
    expect(NeverReadyWebTransport.instances).toHaveLength(1);
    expect(MockWebSocket.instances).toHaveLength(0);

    await vi.advanceTimersByTimeAsync(25);
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(latestSocket().url).toBe("ws://host/mux");
    expect(NeverReadyWebTransport.instances[0].closedByClient).toBe(true);

    mux.close();
  });

  // A permanent wtFailed flag meant one bad moment for UDP — a network that
  // blocks QUIC, a gateway still binding its endpoint — cost WebTransport for
  // the life of the page. blit sessions run for days, so that is a lasting
  // downgrade from a three-second event.
  it("probes WebTransport again after the cooldown rather than giving up for good", async () => {
    vi.stubGlobal("WebTransport", NeverReadyWebTransport);
    const mux = new MuxTransport("ws://host/mux", "secret", {
      wtUrl: "https://host/mux",
      wtConnectTimeoutMs: 25,
      wtReprobeMs: 10_000,
      reconnectDelay: 20,
    });

    mux.connect();
    await vi.advanceTimersByTimeAsync(25);
    expect(NeverReadyWebTransport.instances).toHaveLength(1);
    expect(MockWebSocket.instances).toHaveLength(1);

    // Reconnects during the cooldown stay on WebSocket.
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    latestSocket().close();
    await vi.advanceTimersByTimeAsync(20);
    expect(NeverReadyWebTransport.instances).toHaveLength(1);
    expect(MockWebSocket.instances).toHaveLength(2);

    // Once it expires, the next reconnect tries QUIC again.
    await vi.advanceTimersByTimeAsync(10_000);
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    latestSocket().close();
    await vi.advanceTimersByTimeAsync(20);
    expect(NeverReadyWebTransport.instances).toHaveLength(2);

    mux.close();
  });

  // The gateway rotates its self-signed cert every 13 days. A tab open across
  // a rotation holds a stale hash, so every WT attempt fails validation — and
  // before the cooldown existed, that was permanent.
  it("adopts a rotated cert hash and re-enables WebTransport", async () => {
    vi.stubGlobal("WebTransport", NeverReadyWebTransport);
    const oldHash = "aa".repeat(32);
    const newHash = "bb".repeat(32);
    const mux = new MuxTransport("ws://host/mux", "secret", {
      wtUrl: "https://host/mux",
      wtCertHash: oldHash,
      wtConnectTimeoutMs: 25,
      wtReprobeMs: 10_000,
      reconnectDelay: 20,
    });

    mux.connect();
    await vi.advanceTimersByTimeAsync(25);
    expect(NeverReadyWebTransport.instances).toHaveLength(1);

    // Re-announcing the same hash must not reset the cooldown, or a chatty
    // config WS would defeat it.
    mux.updateWtCertHash(oldHash);
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    latestSocket().close();
    await vi.advanceTimersByTimeAsync(20);
    expect(NeverReadyWebTransport.instances).toHaveLength(1);

    // A genuinely new hash says nothing about the old failure — probe at once.
    mux.updateWtCertHash(newHash);
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    latestSocket().close();
    await vi.advanceTimersByTimeAsync(20);
    expect(NeverReadyWebTransport.instances).toHaveLength(2);

    const sent = NeverReadyWebTransport.instances[1].options
      ?.serverCertificateHashes?.[0].value as Uint8Array | undefined;
    expect(sent && Array.from(new Uint8Array(sent))[0]).toBe(0xbb);

    mux.close();
  });

  it("retries a virtual channel open when the gateway never acknowledges it", async () => {
    const mux = new MuxTransport("ws://host/mux", "secret", {
      reconnectDelay: 20,
      channelConnectTimeoutMs: 50,
    });
    const ch = mux.createChannel("remote");
    const statuses: string[] = [];
    ch.addEventListener("statuschange", (s) => statuses.push(s));

    mux.connect();
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    ch.connect();

    expect(ch.status).toBe("connecting");
    expect(sentControlFrames()).toEqual([{ opcode: 0x01, channelId: 0 }]);

    await vi.advanceTimersByTimeAsync(50);
    expect(ch.status).toBe("disconnected");
    expect(ch.lastError).toBe("connect timeout");
    expect(sentControlFrames()).toEqual([
      { opcode: 0x01, channelId: 0 },
      { opcode: 0x02, channelId: 0 },
    ]);
    expect(statuses).toContain("disconnected");

    await vi.advanceTimersByTimeAsync(20);
    expect(ch.status).toBe("connecting");
    expect(sentControlFrames()).toEqual([
      { opcode: 0x01, channelId: 0 },
      { opcode: 0x02, channelId: 0 },
      { opcode: 0x01, channelId: 0 },
    ]);

    latestSocket().simulateMessage(controlFrame(0x81, 0));
    expect(ch.status).toBe("connected");
    expect(ch.lastError).toBeNull();

    await vi.advanceTimersByTimeAsync(200);
    expect(sentControlFrames()).toEqual([
      { opcode: 0x01, channelId: 0 },
      { opcode: 0x02, channelId: 0 },
      { opcode: 0x01, channelId: 0 },
    ]);

    mux.close();
  });

  // "busy" is the throttle refusing a handshake without checking the
  // passphrase. It must behave like an ordinary disconnect: keep the channels
  // queued and reconnect. Treating it as a rejection strands the mux forever,
  // since handleDisconnect() deliberately skips scheduleReconnect() once
  // _authRejected is set.
  it("retries after a throttled handshake instead of giving up", async () => {
    const mux = new MuxTransport("ws://host/mux", "secret", {
      reconnectDelay: 20,
    });
    const ch = mux.createChannel("remote");

    mux.connect();
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    ch.connect();
    latestSocket().simulateMessage(controlFrame(0x81, 0));
    expect(ch.status).toBe("connected");

    // Reconnect, and this time the server throttles us.
    const socketsBefore = MockWebSocket.instances.length;
    latestSocket().close();
    await vi.advanceTimersByTimeAsync(20);
    expect(MockWebSocket.instances.length).toBe(socketsBefore + 1);
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("busy");

    expect(ch.authRejected).toBe(false);

    // The throttled attempt must still lead to another try.
    const socketsAfterBusy = MockWebSocket.instances.length;
    await vi.advanceTimersByTimeAsync(1000);
    expect(MockWebSocket.instances.length).toBeGreaterThan(socketsAfterBusy);

    // And once the throttle clears, the channel comes back.
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    latestSocket().simulateMessage(controlFrame(0x81, 0));
    expect(ch.status).toBe("connected");

    mux.close();
  });

  // A rejection used to be permanent for every channel: nothing ever cleared
  // MuxChannel._authRejected, and handleDisconnect() never queued the channels
  // for re-open. Re-authenticating has to undo both.
  it("recovers channels after a rejection is followed by a successful auth", async () => {
    const mux = new MuxTransport("ws://host/mux", "secret", {
      reconnectDelay: 20,
    });
    const ch = mux.createChannel("remote");

    mux.connect();
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    ch.connect();
    latestSocket().simulateMessage(controlFrame(0x81, 0));
    expect(ch.status).toBe("connected");

    latestSocket().simulateMessage("auth");
    expect(ch.authRejected).toBe(true);

    // A caller-driven reconnect (the Reconnect button, or a fresh page) that
    // now authenticates must clear the rejection and re-open the channel.
    mux.connect();
    latestSocket().simulateOpen();
    latestSocket().simulateMessage("mux");
    expect(ch.authRejected).toBe(false);
    expect(ch.lastError).toBeNull();

    latestSocket().simulateMessage(controlFrame(0x81, 0));
    expect(ch.status).toBe("connected");

    mux.close();
  });
});
