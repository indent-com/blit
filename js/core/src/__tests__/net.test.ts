import { describe, expect, it } from "vitest";
import {
  C2S_NET_ACK,
  C2S_NET_CLOSE,
  C2S_NET_DATA,
  C2S_NET_DGRAM,
  C2S_NET_OPEN,
  NET_CLOSED_EOF,
  NET_CLOSED_RESET,
  NET_CLOSE_WRITE,
  NET_MAX_CHUNK,
  NET_OPEN_INSECURE,
  NET_OPEN_TLS,
  NET_OPEN_UDP,
  NET_STATUS_OK,
  NET_STATUS_PERMISSION,
  NET_WINDOW_BYTES,
  NET_WINDOW_MIN,
  NetStreams,
  buildNetAckMessage,
  buildNetCloseMessage,
  buildNetDataMessage,
  buildNetDgramMessage,
  buildNetOpenMessage,
  isNetMessage,
  netOpenFlags,
  parseNetAckMessage,
  parseNetClosedMessage,
  parseNetOpenedMessage,
  parseNetPayload,
} from "../net";

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Server-side builders, so the tests exercise the real wire rather than the
 *  client's own encoder in both directions. */
function opened(
  streamId: number,
  status: number,
  alpn = "",
  detail = "",
  window?: number,
) {
  const a = enc.encode(alpn);
  const d = enc.encode(detail);
  const tail = window === undefined ? 0 : 8;
  const msg = new Uint8Array(7 + a.length + d.length + tail);
  msg[0] = 0x80;
  msg[1] = streamId & 0xff;
  msg[2] = streamId >> 8;
  msg[3] = status;
  msg[4] = a.length;
  msg.set(a, 5);
  msg[5 + a.length] = d.length & 0xff;
  msg[6 + a.length] = d.length >> 8;
  msg.set(d, 7 + a.length);
  if (window !== undefined) {
    const at = 7 + a.length + d.length;
    const view = new DataView(msg.buffer);
    view.setUint32(at, window % 0x100000000, true);
    view.setUint32(at + 4, Math.floor(window / 0x100000000), true);
  }
  return msg;
}

function s2cData(streamId: number, payload: Uint8Array) {
  const msg = new Uint8Array(3 + payload.length);
  msg[0] = 0x81;
  msg[1] = streamId & 0xff;
  msg[2] = streamId >> 8;
  msg.set(payload, 3);
  return msg;
}

function s2cAck(streamId: number, bytes: number) {
  const msg = new Uint8Array(11);
  msg[0] = 0x82;
  msg[1] = streamId & 0xff;
  msg[2] = streamId >> 8;
  const view = new DataView(msg.buffer);
  view.setUint32(3, bytes % 0x100000000, true);
  view.setUint32(7, Math.floor(bytes / 0x100000000), true);
  return msg;
}

function closed(streamId: number, reason: number, detail = "") {
  const d = enc.encode(detail);
  const msg = new Uint8Array(6 + d.length);
  msg[0] = 0x83;
  msg[1] = streamId & 0xff;
  msg[2] = streamId >> 8;
  msg[3] = reason;
  msg[4] = d.length & 0xff;
  msg[5] = d.length >> 8;
  msg.set(d, 6);
  return msg;
}

describe("net wire", () => {
  it("builds a plain open", () => {
    const msg = buildNetOpenMessage(7, "db.internal", 5432);
    expect(msg[0]).toBe(C2S_NET_OPEN);
    expect(msg[1] | (msg[2] << 8)).toBe(7);
    expect(msg[3]).toBe(0);
    expect(msg[4] | (msg[5] << 8)).toBe(5432);
    const hostLen = msg[6] | (msg[7] << 8);
    expect(dec.decode(msg.subarray(8, 8 + hostLen))).toBe("db.internal");
    expect(msg.length).toBe(8 + hostLen);
  });

  it("appends a TLS block only when TLS is set", () => {
    const plain = buildNetOpenMessage(1, "h", 80);
    expect(plain.length).toBe(8 + 1);

    const tls = buildNetOpenMessage(1, "example.test", 443, {
      tls: true,
      sni: "other.test",
      alpn: ["h2", "http/1.1"],
    });
    expect(tls[3] & NET_OPEN_TLS).toBeTruthy();
    // host, then [sni_len:2][sni][count][len][proto]…
    const hostLen = tls[6] | (tls[7] << 8);
    let at = 8 + hostLen;
    const sniLen = tls[at] | (tls[at + 1] << 8);
    expect(dec.decode(tls.subarray(at + 2, at + 2 + sniLen))).toBe(
      "other.test",
    );
    at += 2 + sniLen;
    expect(tls[at]).toBe(2);
    at += 1;
    expect(tls[at]).toBe(2);
    expect(dec.decode(tls.subarray(at + 1, at + 3))).toBe("h2");
    at += 3;
    expect(tls[at]).toBe(8);
    expect(dec.decode(tls.subarray(at + 1, at + 9))).toBe("http/1.1");
  });

  it("never offers an empty ALPN protocol", () => {
    // A zero-length entry is illegal on the wire and would sink the whole
    // handshake; drop it rather than pass it on.
    const msg = buildNetOpenMessage(1, "h", 443, {
      tls: true,
      alpn: ["", "h2"],
    });
    const hostLen = msg[6] | (msg[7] << 8);
    let at = 8 + hostLen;
    at += 2 + (msg[at] | (msg[at + 1] << 8));
    expect(msg[at]).toBe(1);
  });

  it("maps options to flags", () => {
    expect(netOpenFlags({})).toBe(0);
    expect(netOpenFlags({ tls: true })).toBe(NET_OPEN_TLS);
    expect(netOpenFlags({ tls: true, insecure: true })).toBe(
      NET_OPEN_TLS | NET_OPEN_INSECURE,
    );
    expect(netOpenFlags({ udp: true })).toBe(NET_OPEN_UDP);
  });

  it("round-trips data, datagrams, acks and closes", () => {
    // The parsers hand back views into the frame rather than copies, so
    // compare ids and bytes, not object identity.
    const data = buildNetDataMessage(4, enc.encode("hello"));
    const parsedData = parseNetPayload(data, C2S_NET_DATA)!;
    expect(parsedData.streamId).toBe(4);
    expect(dec.decode(parsedData.payload)).toBe("hello");
    const dgram = buildNetDgramMessage(5, enc.encode("q"));
    const parsedDgram = parseNetPayload(dgram, C2S_NET_DGRAM)!;
    expect(parsedDgram.streamId).toBe(5);
    expect(dec.decode(parsedDgram.payload)).toBe("q");
    // Stream and datagram payloads must never decode as each other.
    expect(parseNetPayload(data, C2S_NET_DGRAM)).toBeNull();

    const ack = buildNetAckMessage(2, 5_000_000_000);
    expect(ack[0]).toBe(C2S_NET_ACK);
    // Read it back through the S2C parser: the layout is identical, and the
    // point is that a count past 2^32 survives.
    ack[0] = 0x82;
    expect(parseNetAckMessage(ack)).toEqual({
      streamId: 2,
      bytes: 5_000_000_000,
    });

    const close = buildNetCloseMessage(3, NET_CLOSE_WRITE);
    expect([close[0], close[3]]).toEqual([C2S_NET_CLOSE, NET_CLOSE_WRITE]);
  });

  it("parses opened and closed replies", () => {
    expect(parseNetOpenedMessage(opened(8, NET_STATUS_OK, "h2"))).toEqual({
      streamId: 8,
      status: NET_STATUS_OK,
      alpn: "h2",
      detail: "",
    });
    expect(
      parseNetOpenedMessage(opened(8, NET_STATUS_OK, "h2", "", 174_762)),
    ).toEqual({
      streamId: 8,
      status: NET_STATUS_OK,
      alpn: "h2",
      detail: "",
      window: 174_762,
    });
    expect(
      parseNetOpenedMessage(opened(8, NET_STATUS_PERMISSION, "", "nope")),
    ).toEqual({
      streamId: 8,
      status: NET_STATUS_PERMISSION,
      alpn: "",
      detail: "nope",
    });
    expect(parseNetClosedMessage(closed(2, NET_CLOSED_EOF))).toEqual({
      streamId: 2,
      reason: NET_CLOSED_EOF,
      detail: "",
    });
  });

  it("rejects truncated replies rather than guessing", () => {
    const full = opened(1, NET_STATUS_OK, "h2", "why");
    for (let cut = 1; cut < full.length; cut++) {
      expect(parseNetOpenedMessage(full.subarray(0, cut))).toBeNull();
    }
    expect(parseNetOpenedMessage(full)).not.toBeNull();

    // A window that did not survive intact is no window, not a smaller one.
    const withWindow = opened(1, NET_STATUS_OK, "h2", "why", NET_WINDOW_BYTES);
    for (let cut = 1; cut < 8; cut++) {
      expect(
        parseNetOpenedMessage(withWindow.subarray(0, withWindow.length - cut))
          ?.window,
      ).toBeUndefined();
    }
    expect(parseNetOpenedMessage(withWindow)?.window).toBe(NET_WINDOW_BYTES);
  });

  it("recognizes its own opcode block only", () => {
    expect(isNetMessage(0x80)).toBe(true);
    expect(isNetMessage(0x84)).toBe(true);
    expect(isNetMessage(0x7f)).toBe(false);
    expect(isNetMessage(0x85)).toBe(false);
  });
});

describe("NetStreams", () => {
  function harness() {
    const sent: Uint8Array[] = [];
    const streams = new NetStreams((msg) => sent.push(msg));
    return { sent, streams };
  }

  it("opens, reads, and ends on EOF", async () => {
    const { sent, streams } = harness();
    const stream = streams.open("localhost", 3000);
    expect(sent[0][0]).toBe(C2S_NET_OPEN);

    streams.handleMessage(opened(stream.streamId, NET_STATUS_OK, "http/1.1"));
    await expect(stream.opened).resolves.toBe("http/1.1");

    streams.handleMessage(s2cData(stream.streamId, enc.encode("part-one ")));
    streams.handleMessage(s2cData(stream.streamId, enc.encode("part-two")));
    streams.handleMessage(closed(stream.streamId, NET_CLOSED_EOF));

    let body = "";
    for await (const chunk of stream.read()) body += dec.decode(chunk);
    expect(body).toBe("part-one part-two");
    expect(streams.openCount).toBe(0);
  });

  it("acks what it has consumed, so a big response keeps flowing", async () => {
    const { sent, streams } = harness();
    const stream = streams.open("h", 80);
    streams.handleMessage(opened(stream.streamId, NET_STATUS_OK));
    streams.handleMessage(s2cData(stream.streamId, new Uint8Array(100)));
    streams.handleMessage(closed(stream.streamId, NET_CLOSED_EOF));
    for await (const _ of stream.read()) {
      // drain
    }
    const acks = sent.filter((m) => m[0] === C2S_NET_ACK);
    expect(acks.length).toBe(1);
    // The counter is cumulative bytes delivered to the consumer.
    acks[0][0] = 0x82;
    expect(parseNetAckMessage(acks[0])!.bytes).toBe(100);
  });

  it("rejects the open with the server's reason", async () => {
    const { streams } = harness();
    const stream = streams.open("10.0.0.1", 80);
    streams.handleMessage(
      opened(stream.streamId, NET_STATUS_PERMISSION, "", "not permitted"),
    );
    await expect(stream.opened).rejects.toThrow(
      /refused by policy: not permitted/,
    );
    // A failed open gets no NET_CLOSED, so the id must be retired here.
    expect(streams.openCount).toBe(0);
  });

  it("surfaces a reset to the reader instead of a silent truncation", async () => {
    const { streams } = harness();
    const stream = streams.open("h", 80);
    streams.handleMessage(opened(stream.streamId, NET_STATUS_OK));
    streams.handleMessage(s2cData(stream.streamId, enc.encode("partial")));
    streams.handleMessage(closed(stream.streamId, NET_CLOSED_RESET, "boom"));
    const read = async () => {
      const out: string[] = [];
      for await (const chunk of stream.read()) out.push(dec.decode(chunk));
      return out;
    };
    await expect(read()).rejects.toThrow(/reset: boom/);
  });

  it("blocks writes at the window and resumes on an ack", async () => {
    const { sent, streams } = harness();
    const stream = streams.open("h", 80);
    streams.handleMessage(
      opened(stream.streamId, NET_STATUS_OK, "", "", NET_WINDOW_BYTES),
    );

    // One full window, in chunk-sized pieces.
    const big = new Uint8Array(NET_WINDOW_BYTES);
    await stream.write(big);
    const dataCount = sent.filter((m) => m[0] === C2S_NET_DATA).length;
    expect(dataCount).toBe(NET_WINDOW_BYTES / NET_MAX_CHUNK);

    // The next byte has no credit: the write must not resolve yet.
    let resolved = false;
    const pending = stream.write(new Uint8Array(1)).then(() => {
      resolved = true;
    });
    await Promise.resolve();
    expect(resolved).toBe(false);

    streams.handleMessage(s2cAck(stream.streamId, NET_WINDOW_BYTES));
    await pending;
    expect(resolved).toBe(true);
  });

  /** The window is a share of the connection's aggregate that only the server
   *  can compute, so a client that assumes the ceiling gets its stream closed
   *  for BUDGET the moment several are open at once. */
  it("sends only the floor until the accept reports its window", async () => {
    const { sent, streams } = harness();
    const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 0));
    const bytesSent = () =>
      sent
        .filter((m) => m[0] === C2S_NET_DATA)
        .reduce((n, m) => n + m.length - 3, 0);
    const stream = streams.open("h", 80);
    let done = false;
    const write = stream.write(new Uint8Array(NET_WINDOW_BYTES)).then(() => {
      done = true;
    });
    await tick();
    expect(bytesSent()).toBe(NET_WINDOW_MIN);
    expect(done).toBe(false);

    // A fifth concurrent socket's share: more than the floor, less than the
    // ceiling the client used to assume. Chunked, so what goes out is every
    // whole chunk that fits.
    const granted = 838_860;
    streams.handleMessage(
      opened(stream.streamId, NET_STATUS_OK, "", "", granted),
    );
    await tick();
    const inflight = Math.floor(granted / NET_MAX_CHUNK) * NET_MAX_CHUNK;
    expect(bytesSent()).toBe(inflight);
    expect(done).toBe(false);

    streams.handleMessage(s2cAck(stream.streamId, inflight));
    await write;
    expect(bytesSent()).toBe(NET_WINDOW_BYTES);
  });

  /** A server too old to report a window still enforces one, and this client's
   *  socket count is the page's business, so its silence means the floor. */
  it("stays at the floor when the server reports no window", async () => {
    const { sent, streams } = harness();
    const stream = streams.open("h", 80);
    streams.handleMessage(opened(stream.streamId, NET_STATUS_OK));
    let done = false;
    void stream.write(new Uint8Array(NET_WINDOW_BYTES)).then(() => {
      done = true;
    });
    await new Promise<void>((resolve) => setTimeout(resolve, 0));
    const bytes = sent
      .filter((m) => m[0] === C2S_NET_DATA)
      .reduce((n, m) => n + m.length - 3, 0);
    expect(bytes).toBe(NET_WINDOW_MIN);
    expect(done).toBe(false);
  });

  it("does not reuse a live id, and reuses a retired one", () => {
    const { streams } = harness();
    const a = streams.open("h", 80);
    const b = streams.open("h", 80);
    expect(a.streamId).not.toBe(b.streamId);
    streams.handleMessage(closed(a.streamId, NET_CLOSED_EOF));
    // Ids advance rather than immediately recycling, so a late message for a
    // just-closed socket cannot land on a fresh one.
    const c = streams.open("h", 80);
    expect(c.streamId).not.toBe(a.streamId);
    expect(c.streamId).not.toBe(b.streamId);
  });

  it("fails every live socket when the connection goes away", async () => {
    const { streams } = harness();
    const stream = streams.open("h", 80);
    streams.handleMessage(opened(stream.streamId, NET_STATUS_OK));
    streams.reset(new Error("disconnected"));
    const read = async () => {
      for await (const _ of stream.read()) {
        // drain
      }
    };
    await expect(read()).rejects.toThrow(/disconnected/);
    expect(streams.openCount).toBe(0);
  });

  it("ignores messages for unknown ids and foreign opcodes", () => {
    const { streams } = harness();
    expect(streams.handleMessage(new Uint8Array([0x70, 0, 0]))).toBe(false);
    // A stale message for a closed id must not throw or resurrect anything.
    expect(streams.handleMessage(s2cData(999, enc.encode("x")))).toBe(true);
    expect(streams.openCount).toBe(0);
  });
});
