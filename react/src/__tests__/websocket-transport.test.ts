import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { WebSocketTransport } from '../transports/websocket';

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
  binaryType = 'blob';
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
    // Trigger onclose asynchronously-ish (synchronously in tests)
    const handler = this.onclose;
    this.readyState = MockWebSocket.CLOSED;
    handler?.({} as CloseEvent);
  }

  // --- helpers for tests ---

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

/** Bring a transport through the full auth handshake. */
function authenticateTransport(transport: WebSocketTransport): MockWebSocket {
  const ws = latestSocket();
  ws.simulateOpen();
  ws.simulateMessage('ok');
  return ws;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe('WebSocketTransport', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    MockWebSocket.instances = [];
    vi.stubGlobal('WebSocket', MockWebSocket);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  // 1
  it('constructor immediately connects and sets status to connecting', () => {
    const transport = new WebSocketTransport('ws://localhost:1234', 'secret');
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(latestSocket().url).toBe('ws://localhost:1234');
    expect(transport.status).toBe('connecting');
    transport.close();
  });

  // 2
  it('sends passphrase on socket open and transitions to authenticating', () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport('ws://host', 'mypass');
    transport.onstatuschange = (s) => statuses.push(s);

    latestSocket().simulateOpen();

    expect(transport.status).toBe('authenticating');
    expect(latestSocket().sentData).toEqual(['mypass']);
    expect(statuses).toContain('authenticating');
    transport.close();
  });

  // 3
  it('transitions to connected when server responds ok', () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport('ws://host', 'pass');
    transport.onstatuschange = (s) => statuses.push(s);

    authenticateTransport(transport);

    expect(transport.status).toBe('connected');
    expect(statuses).toContain('connected');
    transport.close();
  });

  // 4
  it('transitions to error and closes socket on non-ok auth response', () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport('ws://host', 'pass');
    transport.onstatuschange = (s) => statuses.push(s);

    const ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage('denied');

    expect(statuses).toContain('error');
    // socket.close() was called by the transport
    expect(ws.readyState).toBe(MockWebSocket.CLOSED);
    transport.close();
  });

  // 5
  it('forwards binary messages to onmessage after authentication', () => {
    const received: ArrayBuffer[] = [];
    const transport = new WebSocketTransport('ws://host', 'pass');
    transport.onmessage = (data) => received.push(data);

    const ws = authenticateTransport(transport);

    const buf = new ArrayBuffer(4);
    ws.simulateMessage(buf);

    expect(received).toHaveLength(1);
    expect(received[0]).toBe(buf);
    transport.close();
  });

  // 6
  it('ignores binary messages before authentication', () => {
    const received: ArrayBuffer[] = [];
    const transport = new WebSocketTransport('ws://host', 'pass');
    transport.onmessage = (data) => received.push(data);

    const ws = latestSocket();
    ws.simulateOpen();
    // Send binary before auth completes
    ws.simulateMessage(new ArrayBuffer(4));

    expect(received).toHaveLength(0);
    transport.close();
  });

  // 7
  it('schedules reconnect on close after successful auth', () => {
    const transport = new WebSocketTransport('ws://host', 'pass', {
      reconnectDelay: 1000,
    });
    const ws = authenticateTransport(transport);

    const instancesBefore = MockWebSocket.instances.length;
    ws.simulateClose();

    expect(transport.status).toBe('disconnected');

    // Advance past reconnect delay
    vi.advanceTimersByTime(1000);

    expect(MockWebSocket.instances.length).toBe(instancesBefore + 1);
    expect(transport.status).toBe('connecting');
    transport.close();
  });

  // 8
  it('does not reconnect on close before auth', () => {
    const transport = new WebSocketTransport('ws://host', 'pass', {
      reconnectDelay: 500,
    });
    const ws = latestSocket();
    ws.simulateOpen(); // authenticating, not authenticated yet

    const instancesBefore = MockWebSocket.instances.length;
    ws.simulateClose();

    expect(transport.status).toBe('disconnected');

    vi.advanceTimersByTime(5000);
    // No new WebSocket created
    expect(MockWebSocket.instances.length).toBe(instancesBefore);
    transport.close();
  });

  // 9
  it('transitions to error on socket error before auth', () => {
    const statuses: string[] = [];
    const transport = new WebSocketTransport('ws://host', 'pass');
    transport.onstatuschange = (s) => statuses.push(s);

    latestSocket().simulateError();

    expect(transport.status).toBe('error');
    transport.close();
  });

  // 10
  it('send() works when connected', () => {
    const transport = new WebSocketTransport('ws://host', 'pass');
    const ws = authenticateTransport(transport);

    const data = new Uint8Array([1, 2, 3]);
    transport.send(data);

    // sentData[0] is the passphrase, sentData[1] is our data
    expect(ws.sentData).toHaveLength(2);
    expect(ws.sentData[1]).toBe(data);
    transport.close();
  });

  // 11
  it('send() is a no-op when not connected', () => {
    const transport = new WebSocketTransport('ws://host', 'pass');
    const ws = latestSocket();

    // Still in CONNECTING state
    transport.send(new Uint8Array([1, 2, 3]));
    expect(ws.sentData).toHaveLength(0);
    transport.close();
  });

  // 12
  it('close() disposes and prevents reconnect', () => {
    const transport = new WebSocketTransport('ws://host', 'pass', {
      reconnectDelay: 500,
    });
    const ws = authenticateTransport(transport);

    // Simulate disconnect then close
    transport.close();

    const instancesBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(10000);

    // No reconnect attempts
    expect(MockWebSocket.instances.length).toBe(instancesBefore);
    expect(transport.status).toBe('disconnected');
  });

  // 13
  it('reconnect delay increases with backoff', () => {
    // Auth always resets currentDelay, so after a successful session the
    // delay is always initialDelay. This test verifies that repeated
    // auth+close cycles consistently reconnect at initialDelay, proving
    // the reset path works.
    const transport = new WebSocketTransport('ws://host', 'pass', {
      reconnectDelay: 100,
      reconnectBackoff: 2,
      maxReconnectDelay: 10000,
    });

    // First connection + auth + disconnect
    let ws = authenticateTransport(transport);
    ws.simulateClose();

    // First reconnect fires at 100ms
    let countBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(99);
    expect(MockWebSocket.instances.length).toBe(countBefore);
    vi.advanceTimersByTime(1);
    expect(MockWebSocket.instances.length).toBe(countBefore + 1);

    // Auth again, disconnect — delay resets to 100
    ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage('ok');
    ws.simulateClose();

    // Second reconnect still at 100ms
    countBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(99);
    expect(MockWebSocket.instances.length).toBe(countBefore);
    vi.advanceTimersByTime(1);
    expect(MockWebSocket.instances.length).toBe(countBefore + 1);

    transport.close();
  });

  // 14
  it('successful reconnect resets the delay', () => {
    const transport = new WebSocketTransport('ws://host', 'pass', {
      reconnectDelay: 100,
      reconnectBackoff: 2,
      maxReconnectDelay: 10000,
    });

    // Connect, auth, disconnect
    let ws = authenticateTransport(transport);
    ws.simulateClose();

    // First reconnect at 100ms
    vi.advanceTimersByTime(100);
    ws = latestSocket();
    ws.simulateOpen();
    ws.simulateMessage('ok'); // this resets delay back to initialDelay

    // Disconnect again
    ws.simulateClose();

    // Should reconnect at 100ms again (not 200ms), because auth reset the delay
    const countBefore = MockWebSocket.instances.length;
    vi.advanceTimersByTime(100);
    expect(MockWebSocket.instances.length).toBe(countBefore + 1);

    transport.close();
  });

  // 15
  it('reconnect:false disables reconnection', () => {
    const transport = new WebSocketTransport('ws://host', 'pass', {
      reconnect: false,
    });
    const ws = authenticateTransport(transport);

    const countBefore = MockWebSocket.instances.length;
    ws.simulateClose();

    vi.advanceTimersByTime(60000);
    expect(MockWebSocket.instances.length).toBe(countBefore);

    transport.close();
  });
});
