import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { WebSocketTransport } from "../transports/websocket";

// ---------------------------------------------------------------------------
// Mock WebSocket
// ---------------------------------------------------------------------------

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

  simulateError() {
    this.onerror?.({} as Event);
  }

  simulateClose() {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.({} as CloseEvent);
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function latestSocket(): MockWebSocket {
  return MockWebSocket.instances[MockWebSocket.instances.length - 1];
}

function authenticateTransport(transport: WebSocketTransport): MockWebSocket {
  const ws = latestSocket();
  ws.simulateOpen();
  ws.simulateMessage("ok");
  return ws;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("WebSocketTransport", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    MockWebSocket.instances = [];
    vi.stubGlobal("WebSocket", MockWebSocket);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("starts as disconnected and connects on connect()", () => {
    const transport = new WebSocketTransport("ws://localhost:1234", "secret");
    expect(transport.status).toBe("disconnected");
    transport.connect();
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(latestSocket().url).toBe("ws://localhost:1234");
    expect(transport.status).toBe("connecting");
    transport.close();
  });

  it("sends passphrase on socket open and transitions to authenticating", () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport("ws://host", "mypass");
    transport.addEventListener("statuschange", (s) => statuses.push(s));

    transport.connect();
    latestSocket().simulateOpen();

    expect(transport.status).toBe("authenticating");
    expect(latestSocket().sentData).toEqual(["mypass"]);
    expect(statuses).toContain("authenticating");
    transport.close();
  });

  it("transitions to connected when server responds ok", () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.addEventListener("statuschange", (s) => statuses.push(s));

    transport.connect();
    authenticateTransport(transport);

    expect(transport.status).toBe("connected");
    expect(statuses).toContain("connected");
    transport.close();
  });

  it("transitions to error on auth rejection", () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.addEventListener("statuschange", (s) => statuses.push(s));

    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage("auth");

    expect(statuses).toContain("error");
    expect(transport.authRejected).toBe(true);
    expect(transport.lastError).toBe("Authentication failed");
    expect(ws.readyState).toBe(MockWebSocket.CLOSED);
    transport.close();
  });

  it("does not reconnect after auth rejection", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 500,
    });
    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage("auth");

    expect(transport.authRejected).toBe(true);

    const instancesBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(60000);
    expect(MockWebSocket.instances.length).toBe(instancesBefore);
    transport.close();
  });

  it("transitions to error on server error and retries", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 500,
    });
    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage("error:cannot connect to blit-server");

    expect(transport.authRejected).toBe(false);
    expect(transport.lastError).toBe("cannot connect to blit-server");

    // Server errors trigger reconnect (unlike auth rejection)
    const instancesBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(500);
    expect(MockWebSocket.instances.length).toBe(instancesBefore + 1);
    transport.close();
  });

  it("forwards binary messages to message listeners after authentication", () => {
    const received: ArrayBuffer[] = [];
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.addEventListener("message", (data) => received.push(data));

    transport.connect();
    const ws = authenticateTransport(transport);

    const buf = new ArrayBuffer(4);
    ws.simulateMessage(buf);

    expect(received).toHaveLength(1);
    expect(received[0]).toBe(buf);
    transport.close();
  });

  it("ignores binary messages before authentication", () => {
    const received: ArrayBuffer[] = [];
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.addEventListener("message", (data) => received.push(data));

    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage(new ArrayBuffer(4));

    expect(received).toHaveLength(0);
    transport.close();
  });

  it("schedules reconnect on close after successful auth", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 1000,
    });
    transport.connect();
    const ws = authenticateTransport(transport);

    const instancesBefore = MockWebSocket.instances.length;
    ws.simulateClose();

    expect(transport.status).toBe("disconnected");

    vi.advanceTimersByTime(1000);

    expect(MockWebSocket.instances.length).toBe(instancesBefore + 1);
    expect(transport.status).toBe("connecting");
    transport.close();
  });

  it("reconnects on close before auth (not rejected)", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 500,
    });
    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();

    const instancesBefore = MockWebSocket.instances.length;
    ws.simulateClose();

    // Close before auth response is treated as transient — reconnects
    expect(transport.status).toBe("disconnected");

    vi.advanceTimersByTime(500);
    expect(MockWebSocket.instances.length).toBe(instancesBefore + 1);
    transport.close();
  });

  it("transitions to error on socket error before auth", () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.addEventListener("statuschange", (s) => statuses.push(s));

    transport.connect();
    latestSocket().simulateError();

    expect(transport.status).toBe("error");
    transport.close();
  });

  it("send() works when connected", () => {
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.connect();
    const ws = authenticateTransport(transport);

    const data = new Uint8Array([1, 2, 3]);
    transport.send(data);

    expect(ws.sentData).toHaveLength(2);
    expect(ws.sentData[1]).toBe(data);
    transport.close();
  });

  it("send() is a no-op when not connected", () => {
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.connect();
    const ws = latestSocket();

    transport.send(new Uint8Array([1, 2, 3]));
    expect(ws.sentData).toHaveLength(0);
    transport.close();
  });

  it("close() disposes and prevents reconnect", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 500,
    });
    transport.connect();
    authenticateTransport(transport);

    transport.close();

    const instancesBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(10000);

    expect(MockWebSocket.instances.length).toBe(instancesBefore);
    expect(transport.status).toBe("closed");
  });

  it("reconnect delay increases with backoff", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 100,
      reconnectBackoff: 2,
      maxReconnectDelay: 10000,
    });

    transport.connect();
    let ws = authenticateTransport(transport);
    ws.simulateClose();

    let countBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(99);
    expect(MockWebSocket.instances.length).toBe(countBefore);
    vi.advanceTimersByTime(1);
    expect(MockWebSocket.instances.length).toBe(countBefore + 1);

    ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage("ok");
    ws.simulateClose();

    countBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(99);
    expect(MockWebSocket.instances.length).toBe(countBefore);
    vi.advanceTimersByTime(1);
    expect(MockWebSocket.instances.length).toBe(countBefore + 1);

    transport.close();
  });

  it("successful reconnect resets the delay", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnectDelay: 100,
      reconnectBackoff: 2,
      maxReconnectDelay: 10000,
    });

    transport.connect();
    let ws = authenticateTransport(transport);
    ws.simulateClose();

    vi.advanceTimersByTime(100);
    ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage("ok");

    ws.simulateClose();

    const countBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(100);
    expect(MockWebSocket.instances.length).toBe(countBefore + 1);

    transport.close();
  });

  it("reconnect:false disables reconnection", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      reconnect: false,
    });
    transport.connect();
    const ws = authenticateTransport(transport);

    const countBefore = MockWebSocket.instances.length;
    ws.simulateClose();

    vi.advanceTimersByTime(60000);
    expect(MockWebSocket.instances.length).toBe(countBefore);

    transport.close();
  });

  it("removeEventListener stops delivery", () => {
    const transport = new WebSocketTransport("ws://host", "pass");
    const cb = vi.fn();
    transport.addEventListener("message", cb);
    transport.removeEventListener("message", cb);

    transport.connect();
    const ws = authenticateTransport(transport);
    ws.simulateMessage(new ArrayBuffer(4));

    expect(cb).not.toHaveBeenCalled();
    transport.close();
  });

  it("multiple message listeners all receive data", () => {
    const cb1 = vi.fn();
    const cb2 = vi.fn();
    const transport = new WebSocketTransport("ws://host", "pass");
    transport.addEventListener("message", cb1);
    transport.addEventListener("message", cb2);

    transport.connect();
    const ws = authenticateTransport(transport);
    ws.simulateMessage(new ArrayBuffer(4));

    expect(cb1).toHaveBeenCalledTimes(1);
    expect(cb2).toHaveBeenCalledTimes(1);
    transport.close();
  });

  it("connect timeout fires when auth does not complete in time", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      connectTimeoutMs: 2000,
    });
    const statuses: string[] = [];
    transport.addEventListener("statuschange", (s) => statuses.push(s));

    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();
    expect(transport.status).toBe("authenticating");

    vi.advanceTimersByTime(2000);
    expect(transport.status).toBe("error");
    expect(transport.lastError).toBe("connect timeout");
    expect(ws.readyState).toBe(MockWebSocket.CLOSED);
    transport.close();
  });

  it("connect timeout is cleared on successful auth", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      connectTimeoutMs: 2000,
    });

    transport.connect();
    const ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage("ok");
    expect(transport.status).toBe("connected");

    vi.advanceTimersByTime(5000);
    expect(transport.status).toBe("connected");
    transport.close();
  });

  it("connect timeout triggers reconnect when enabled", () => {
    const transport = new WebSocketTransport("ws://host", "pass", {
      connectTimeoutMs: 1000,
      reconnect: true,
    });

    transport.connect();
    vi.advanceTimersByTime(1000);
    expect(transport.status).toBe("error");

    const instancesBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(500);
    expect(MockWebSocket.instances.length).toBeGreaterThan(instancesBefore);
    transport.close();
  });
});
