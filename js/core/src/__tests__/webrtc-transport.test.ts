import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { createWebRtcDataChannelTransport } from "../transports/webrtc";
import { C2S_DISPLAY_RATE } from "../types";

// ---------------------------------------------------------------------------
// Minimal WebRTC mocks (jsdom doesn't ship WebRTC APIs)
// ---------------------------------------------------------------------------

class MockRTCDataChannel {
  label: string;
  binaryType = "arraybuffer";
  readyState: RTCDataChannelState = "connecting";
  ordered?: boolean;

  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;
  onclose: ((ev: Event) => void) | null = null;

  sent: Uint8Array[] = [];

  constructor(label: string, _opts?: RTCDataChannelInit) {
    this.label = label;
  }

  send(data: Uint8Array) {
    this.sent.push(new Uint8Array(data));
  }

  close() {
    this.readyState = "closed";
  }

  simulateOpen() {
    this.readyState = "open";
    this.onopen?.(new Event("open"));
  }

  simulateMessage(data: ArrayBuffer) {
    this.onmessage?.(new MessageEvent("message", { data }));
  }

  simulateError() {
    this.onerror?.(new Event("error"));
  }

  simulateClose() {
    this.readyState = "closed";
    this.onclose?.(new Event("close"));
  }
}

class MockRTCPeerConnection {
  connectionState: RTCPeerConnectionState = "new";
  private listeners: Record<string, ((...args: any[]) => void)[]> = {};
  lastChannel: MockRTCDataChannel | null = null;

  createDataChannel(
    label: string,
    opts?: RTCDataChannelInit,
  ): MockRTCDataChannel {
    const ch = new MockRTCDataChannel(label, opts);
    this.lastChannel = ch;
    return ch as any;
  }

  addEventListener(event: string, cb: (...args: any[]) => void) {
    (this.listeners[event] ??= []).push(cb);
  }

  removeEventListener(event: string, cb: (...args: any[]) => void) {
    const arr = this.listeners[event];
    if (arr) {
      const i = arr.indexOf(cb);
      if (i !== -1) arr.splice(i, 1);
    }
  }

  simulateConnectionState(state: RTCPeerConnectionState) {
    this.connectionState = state;
    for (const cb of this.listeners["connectionstatechange"] ?? []) {
      cb(new Event("connectionstatechange"));
    }
  }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function frame(payload: Uint8Array): Uint8Array {
  const len = payload.length;
  const buf = new Uint8Array(4 + len);
  buf[0] = len & 0xff;
  buf[1] = (len >> 8) & 0xff;
  buf[2] = (len >> 16) & 0xff;
  buf[3] = (len >> 24) & 0xff;
  buf.set(payload, 4);
  return buf;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

describe("createWebRtcDataChannelTransport", () => {
  let pc: MockRTCPeerConnection;
  let channel: MockRTCDataChannel;

  beforeEach(() => {
    vi.useFakeTimers();
    pc = new MockRTCPeerConnection();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  function create(
    opts?: Parameters<typeof createWebRtcDataChannelTransport>[1],
  ) {
    const t = createWebRtcDataChannelTransport(pc as any, opts);
    channel = pc.lastChannel!;
    return t;
  }

  it('initial status is "connecting"', () => {
    const t = create();
    expect(t.status).toBe("connecting");
  });

  it('channel open sets status to "connected"', () => {
    const t = create();
    const statusCb = vi.fn();
    t.addEventListener("statuschange", statusCb);
    channel.simulateOpen();
    expect(t.status).toBe("connected");
    expect(statusCb).toHaveBeenCalledWith("connected");
  });

  it("sends C2S_DISPLAY_RATE message on open", () => {
    create();
    channel.simulateOpen();
    expect(channel.sent.length).toBe(1);
    const sent = channel.sent[0];
    expect(sent.length).toBe(7);
    expect(sent[0]).toBe(3);
    expect(sent[1]).toBe(0);
    expect(sent[2]).toBe(0);
    expect(sent[3]).toBe(0);
    expect(sent[4]).toBe(C2S_DISPLAY_RATE);
    expect(sent[5]).toBe(120);
    expect(sent[6]).toBe(0);
  });

  it("send() wraps data in a 4-byte length-prefixed frame", () => {
    const t = create();
    channel.simulateOpen();
    channel.sent = [];

    const payload = new Uint8Array([0xaa, 0xbb, 0xcc]);
    t.send(payload);

    expect(channel.sent.length).toBe(1);
    const sent = channel.sent[0];
    expect(sent.length).toBe(7);
    expect(sent[0]).toBe(3);
    expect(sent[1]).toBe(0);
    expect(sent[2]).toBe(0);
    expect(sent[3]).toBe(0);
    expect(sent.slice(4)).toEqual(payload);
  });

  it("incoming messages are deframed (4-byte envelope removed)", () => {
    const t = create();
    const onmsg = vi.fn();
    t.addEventListener("message", onmsg);
    t.connect();
    channel.simulateOpen();

    const payload = new Uint8Array([1, 2, 3]);
    channel.simulateMessage(frame(payload).buffer as ArrayBuffer);

    expect(onmsg).toHaveBeenCalledTimes(1);
    const received = new Uint8Array(onmsg.mock.calls[0][0]);
    expect(received).toEqual(payload);
  });

  it("reassembles partial frames", () => {
    const t = create();
    const onmsg = vi.fn();
    t.addEventListener("message", onmsg);
    t.connect();
    channel.simulateOpen();

    const payload = new Uint8Array([10, 20, 30, 40, 50]);
    const full = frame(payload);
    const part1 = full.slice(0, 3);
    const part2 = full.slice(3);

    channel.simulateMessage(part1.buffer);
    expect(onmsg).not.toHaveBeenCalled();

    channel.simulateMessage(part2.buffer);
    expect(onmsg).toHaveBeenCalledTimes(1);
    expect(new Uint8Array(onmsg.mock.calls[0][0])).toEqual(payload);
  });

  it("handles multiple frames in one chunk", () => {
    const t = create();
    const onmsg = vi.fn();
    t.addEventListener("message", onmsg);
    t.connect();
    channel.simulateOpen();

    const p1 = new Uint8Array([0xaa]);
    const p2 = new Uint8Array([0xbb, 0xcc]);
    const f1 = frame(p1);
    const f2 = frame(p2);

    const combined = new Uint8Array(f1.length + f2.length);
    combined.set(f1);
    combined.set(f2, f1.length);

    channel.simulateMessage(combined.buffer);

    expect(onmsg).toHaveBeenCalledTimes(2);
    expect(new Uint8Array(onmsg.mock.calls[0][0])).toEqual(p1);
    expect(new Uint8Array(onmsg.mock.calls[1][0])).toEqual(p2);
  });

  it('close() sets status to "closed"', () => {
    const t = create();
    channel.simulateOpen();
    t.close();
    expect(t.status).toBe("closed");
  });

  it('channel error sets status to "error"', () => {
    const t = create();
    const statusCb = vi.fn();
    t.addEventListener("statuschange", statusCb);
    channel.simulateError();
    expect(t.status).toBe("error");
    expect(t.lastError).toBe("Data channel error");
    expect(statusCb).toHaveBeenCalledWith("error");
  });

  it('channel close sets status to "disconnected"', () => {
    const t = create();
    channel.simulateOpen();
    channel.simulateClose();
    expect(t.status).toBe("disconnected");
  });

  it('PeerConnection state "failed" sets status to "disconnected"', () => {
    const t = create();
    channel.simulateOpen();
    pc.simulateConnectionState("failed");
    expect(t.status).toBe("disconnected");
  });

  it('PeerConnection state "closed" sets status to "disconnected"', () => {
    const t = create();
    channel.simulateOpen();
    pc.simulateConnectionState("closed");
    expect(t.status).toBe("disconnected");
  });

  it("waitForSync() resolves on connect", async () => {
    const t = create();
    const p = t.waitForSync();
    channel.simulateOpen();
    await expect(p).resolves.toBeUndefined();
  });

  it("waitForSync() rejects on error", async () => {
    const t = create();
    const p = t.waitForSync();
    channel.simulateError();
    await expect(p).rejects.toThrow("transport error");
  });

  it("waitForSync() resolves immediately if already connected", async () => {
    const t = create();
    channel.simulateOpen();
    await expect(t.waitForSync()).resolves.toBeUndefined();
  });

  it('connect timeout fires and sets status to "error"', () => {
    const t = create({ connectTimeoutMs: 500 });
    const statusCb = vi.fn();
    t.addEventListener("statuschange", statusCb);

    vi.advanceTimersByTime(500);

    expect(t.status).toBe("error");
    expect(t.lastError).toBe("connect timeout");
    expect(statusCb).toHaveBeenCalledWith("error");
  });

  it("disposed transport ignores further events", () => {
    const t = create();
    const statusCb = vi.fn();
    t.addEventListener("statuschange", statusCb);

    t.close();
    statusCb.mockClear();

    channel = pc.lastChannel!;
    channel.onopen?.(new Event("open"));
    channel.onmessage?.(
      new MessageEvent("message", { data: frame(new Uint8Array([1])).buffer }),
    );
    channel.onerror?.(new Event("error"));
    channel.onclose?.(new Event("close"));

    expect(t.status).toBe("closed");
    expect(statusCb).not.toHaveBeenCalled();
  });

  it("uses custom label option", () => {
    create({ label: "my-channel" });
    expect(channel.label).toBe("my-channel");
  });

  it("send() is a no-op when channel is not open", () => {
    const t = create();
    t.send(new Uint8Array([1, 2, 3]));
    expect(channel.sent.length).toBe(0);
  });

  it("removeEventListener stops delivery", () => {
    const t = create();
    const cb = vi.fn();
    t.addEventListener("message", cb);
    t.removeEventListener("message", cb);
    channel.simulateOpen();
    channel.simulateMessage(frame(new Uint8Array([1])).buffer as ArrayBuffer);
    expect(cb).not.toHaveBeenCalled();
  });

  it("multiple message listeners all receive data", () => {
    const cb1 = vi.fn();
    const cb2 = vi.fn();
    const t = create();
    t.addEventListener("message", cb1);
    t.addEventListener("message", cb2);
    t.connect();
    channel.simulateOpen();
    channel.simulateMessage(frame(new Uint8Array([1])).buffer as ArrayBuffer);
    expect(cb1).toHaveBeenCalledTimes(1);
    expect(cb2).toHaveBeenCalledTimes(1);
  });

  it("reconnects automatically after channel close", () => {
    const t = create({ reconnect: true, reconnectDelay: 200 });
    channel.simulateOpen();
    expect(t.status).toBe("connected");

    const channelsBefore = pc.lastChannel;
    channel.simulateClose();
    expect(t.status).toBe("disconnected");

    vi.advanceTimersByTime(200);
    expect(pc.lastChannel).not.toBe(channelsBefore);
    expect(t.status).toBe("connecting");
  });

  it("reconnects with exponential backoff", () => {
    const t = create({
      reconnect: true,
      reconnectDelay: 100,
      reconnectBackoff: 2,
      maxReconnectDelay: 1000,
    });
    channel.simulateOpen();

    channel.simulateClose();
    vi.advanceTimersByTime(100);
    const ch2 = pc.lastChannel!;
    expect(t.status).toBe("connecting");

    ch2.simulateError();
    vi.advanceTimersByTime(100);
    expect(t.status).toBe("error");
    vi.advanceTimersByTime(100);
    expect(pc.lastChannel).not.toBe(ch2);
  });

  it("does not reconnect after close()", () => {
    const t = create({ reconnect: true, reconnectDelay: 100 });
    channel.simulateOpen();

    t.close();
    const channelsAfterClose = pc.lastChannel;
    vi.advanceTimersByTime(500);
    expect(pc.lastChannel).toBe(channelsAfterClose);
  });

  it("does not reconnect when reconnect is disabled", () => {
    const t = create({ reconnect: false });
    channel.simulateOpen();

    const channelBefore = pc.lastChannel;
    channel.simulateClose();
    vi.advanceTimersByTime(10000);
    expect(pc.lastChannel).toBe(channelBefore);
    t.close();
  });

  it("does not reconnect when peer connection is failed", () => {
    const t = create({ reconnect: true, reconnectDelay: 100 });
    channel.simulateOpen();

    pc.simulateConnectionState("failed");
    const channelAfter = pc.lastChannel;
    vi.advanceTimersByTime(500);
    expect(pc.lastChannel).toBe(channelAfter);
    t.close();
  });

  it("reconnect resets delay on successful open", () => {
    const t = create({
      reconnect: true,
      reconnectDelay: 100,
      reconnectBackoff: 2,
    });
    channel.simulateOpen();

    channel.simulateClose();
    vi.advanceTimersByTime(100);
    const ch2 = pc.lastChannel!;
    ch2.readyState = "open";
    ch2.onopen?.(new Event("open"));
    expect(t.status).toBe("connected");

    ch2.onclose?.(new Event("close"));
    vi.advanceTimersByTime(100);
    expect(pc.lastChannel).not.toBe(ch2);
  });

  it("reconnect after connect timeout error", () => {
    const t = create({
      reconnect: true,
      reconnectDelay: 100,
      connectTimeoutMs: 500,
    });
    vi.advanceTimersByTime(500);
    expect(t.status).toBe("error");

    vi.advanceTimersByTime(100);
    expect(t.status).toBe("connecting");
  });
});
