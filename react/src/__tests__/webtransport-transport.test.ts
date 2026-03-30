import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WebTransportTransport } from "../transports/webtransport";

class MockReader {
  private readonly chunks: Uint8Array[];
  private index = 0;

  constructor(chunks: Uint8Array[]) {
    this.chunks = chunks;
  }

  async read(): Promise<ReadableStreamReadResult<Uint8Array>> {
    if (this.index >= this.chunks.length) {
      return { done: true, value: undefined };
    }
    return { done: false, value: this.chunks[this.index++] };
  }
}

class MockWriter {
  writes: Uint8Array[] = [];

  async write(data: Uint8Array): Promise<void> {
    this.writes.push(new Uint8Array(data));
  }
}

class MockWebTransport {
  static instances: MockWebTransport[] = [];
  static queuedChunks: Uint8Array[][] = [];

  readonly writer = new MockWriter();
  readonly reader: MockReader;
  readonly ready = Promise.resolve();
  readonly closed = new Promise<void>(() => {});

  constructor(_url: string, _opts?: WebTransportOptions) {
    this.reader = new MockReader(MockWebTransport.queuedChunks.shift() ?? []);
    MockWebTransport.instances.push(this);
  }

  static queueConnection(...chunks: Uint8Array[]) {
    MockWebTransport.queuedChunks.push(chunks);
  }

  async createBidirectionalStream() {
    return {
      readable: { getReader: () => this.reader },
      writable: { getWriter: () => this.writer },
    } as unknown as WebTransportBidirectionalStream;
  }

  close() {}
}

async function flushPromises(): Promise<void> {
  // Enough microtask ticks for all async steps in connectInternal to complete
  for (let i = 0; i < 10; i++) {
    await Promise.resolve();
  }
}

function frame(payload: Uint8Array): Uint8Array {
  const bytes = new Uint8Array(4 + payload.length);
  bytes[0] = payload.length & 0xff;
  bytes[1] = (payload.length >> 8) & 0xff;
  bytes[2] = (payload.length >> 16) & 0xff;
  bytes[3] = (payload.length >> 24) & 0xff;
  bytes.set(payload, 4);
  return bytes;
}

describe("WebTransportTransport", () => {
  beforeEach(() => {
    MockWebTransport.instances = [];
    MockWebTransport.queuedChunks = [];
    vi.stubGlobal("WebTransport", MockWebTransport);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("connect() is idempotent while a connection attempt is in flight", async () => {
    MockWebTransport.queueConnection(new Uint8Array([1]));
    const transport = new WebTransportTransport(
      "https://example.test",
      "secret",
    );

    transport.connect();
    transport.connect();

    await flushPromises();

    expect(MockWebTransport.instances).toHaveLength(1);
  });

  it("does not drop the first frame when it arrives with the auth response", async () => {
    const payload = new Uint8Array([9, 8, 7]);
    const firstChunk = new Uint8Array(1 + 4 + payload.length);
    firstChunk[0] = 1;
    firstChunk.set(frame(payload), 1);
    MockWebTransport.queueConnection(firstChunk);

    const transport = new WebTransportTransport(
      "https://example.test",
      "secret",
    );
    const messages: Uint8Array[] = [];
    transport.addEventListener("message", (data) => {
      messages.push(new Uint8Array(data));
    });

    transport.connect();
    await flushPromises();

    expect(messages).toEqual([payload]);
  });

  it("sets authRejected and lastError on auth failure", async () => {
    MockWebTransport.queueConnection(new Uint8Array([0]));
    const transport = new WebTransportTransport(
      "https://example.test",
      "wrong",
    );
    const statuses: string[] = [];
    transport.addEventListener("statuschange", (s) => statuses.push(s));

    transport.connect();
    await flushPromises();

    expect(transport.authRejected).toBe(true);
    expect(transport.lastError).toBe("Authentication failed");
    expect(statuses).toContain("error");
  });

  it("clears authRejected and lastError on successful auth", async () => {
    MockWebTransport.queueConnection(new Uint8Array([1]));
    const transport = new WebTransportTransport(
      "https://example.test",
      "secret",
    );

    transport.connect();
    await flushPromises();

    expect(transport.authRejected).toBe(false);
    expect(transport.lastError).toBeNull();
    expect(transport.status).toBe("connected");
  });

  it("uses configurable connectTimeoutMs", async () => {
    vi.useFakeTimers();
    const neverReady = new Promise<void>(() => {});
    vi.stubGlobal(
      "WebTransport",
      class {
        static instances: any[] = [];
        ready = neverReady;
        closed = new Promise<void>(() => {});
        close() {}
        async createBidirectionalStream() {
          return {
            readable: { getReader: () => new MockReader([]) },
            writable: { getWriter: () => new MockWriter() },
          } as unknown as WebTransportBidirectionalStream;
        }
      },
    );

    const transport = new WebTransportTransport(
      "https://example.test",
      "secret",
      {
        connectTimeoutMs: 3000,
        reconnect: false,
      },
    );

    transport.connect();
    await flushPromises();
    expect(transport.status).toBe("connecting");

    vi.advanceTimersByTime(3000);
    await flushPromises();

    expect(transport.status).toBe("error");
    expect(transport.lastError).toBe("connect timeout");
    transport.close();
    vi.useRealTimers();
  });
});
