import { describe, expect, it } from "vitest";
import { BlitConnection } from "../BlitConnection";
import { MockTransport } from "./mock-transport";
import type { BlitWasmModule } from "../TerminalStore";
import {
  CHANNEL,
  CHANNEL_CLOSED,
  CHANNEL_CLOSE_NORMAL,
  CHANNEL_CLOSE_PEER_GONE,
  CHANNEL_CONNECT,
  CHANNEL_DATA,
  CHANNEL_ACK,
  CHANNEL_OPENED,
  FEATURE_CHANNEL,
  parseChannelMessage,
} from "../channel";
import { STATUS_INVALID, STATUS_OK } from "../types";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/** A server `CHANNEL_OPENED` for a client-created channel. */
function opened(
  channelId: number,
  status: number,
  window: bigint,
  peer: string,
  detail = "",
): Uint8Array {
  const peerBytes = encoder.encode(peer);
  const detailBytes = encoder.encode(detail);
  const message = new Uint8Array(
    6 + 1 + 8 + 2 + peerBytes.length + 4 + detailBytes.length,
  );
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_OPENED;
  view.setUint32(2, channelId, true);
  message[6] = status;
  view.setBigUint64(7, window, true);
  view.setUint16(15, peerBytes.length, true);
  message.set(peerBytes, 17);
  view.setUint32(17 + peerBytes.length, 0, true);
  message.set(detailBytes, 21 + peerBytes.length);
  return message;
}

function data(channelId: number, payload: string): Uint8Array {
  const bytes = encoder.encode(payload);
  const message = new Uint8Array(6 + bytes.length);
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_DATA;
  view.setUint32(2, channelId, true);
  message.set(bytes, 6);
  return message;
}

function ack(channelId: number, bytes: bigint): Uint8Array {
  const message = new Uint8Array(14);
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_ACK;
  view.setUint32(2, channelId, true);
  view.setBigUint64(6, bytes, true);
  return message;
}

function closed(channelId: number, reason: number, detail = ""): Uint8Array {
  const detailBytes = encoder.encode(detail);
  const message = new Uint8Array(7 + detailBytes.length);
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_CLOSED;
  view.setUint32(2, channelId, true);
  message[6] = reason;
  message.set(detailBytes, 7);
  return message;
}

function connect(features = FEATURE_CHANNEL): {
  transport: MockTransport;
  connection: BlitConnection;
} {
  const transport = new MockTransport();
  const connection = new BlitConnection({
    id: "test",
    transport,
    wasm: {} as BlitWasmModule,
  });
  transport.pushHello(1, features);
  transport.pushReady();
  return { transport, connection };
}

/** The last packet the client sent for `channelId`, decoded. */
function lastChannelMessage(transport: MockTransport) {
  for (let index = transport.sent.length - 1; index >= 0; index--) {
    const message = transport.sent[index]!;
    if (message[0] === CHANNEL) return message;
  }
  return null;
}

describe("BlitConnection native channels", () => {
  it("opens a channel and resolves with the peer label", async () => {
    const { transport, connection } = connect();
    const pending = connection.connectChannel("blit.systemd.v1");
    const request = lastChannelMessage(transport)!;
    expect(request[1]).toBe(CHANNEL_CONNECT);
    const channelId = new DataView(request.buffer).getUint32(2, true);
    // Client-created ids are even; the server owns the odd ones.
    expect(channelId % 2).toBe(0);

    transport.push(opened(channelId, STATUS_OK, 1024n, "ext:7:1"));
    const channel = await pending;
    expect(channel.peer).toBe("ext:7:1");
    expect(channel.availableCredit).toBe(1024n);
  });

  it("rejects a refused open with the server's detail", async () => {
    const { transport, connection } = connect();
    const pending = connection.connectChannel("nope");
    const request = lastChannelMessage(transport)!;
    const channelId = new DataView(request.buffer).getUint32(2, true);
    transport.push(opened(channelId, STATUS_INVALID, 0n, "", "no listener"));
    await expect(pending).rejects.toThrow(/no listener/);
  });

  it("rejects when the server refuses by closing instead of opening", async () => {
    const { transport, connection } = connect();
    const pending = connection.connectChannel("nope");
    const request = lastChannelMessage(transport)!;
    const channelId = new DataView(request.buffer).getUint32(2, true);
    transport.push(closed(channelId, CHANNEL_CLOSE_PEER_GONE, "gone"));
    await expect(pending).rejects.toThrow(/gone/);
  });

  it("refuses to open without the server feature", async () => {
    const { connection } = connect(0);
    await expect(connection.connectChannel("blit.systemd.v1")).rejects.toThrow(
      /does not support native channels/,
    );
  });

  it("delivers data and acknowledges it cumulatively", async () => {
    const { transport, connection } = connect();
    const received: string[] = [];
    const pending = connection.connectChannel("blit.systemd.v1", {
      onData: (payload) => received.push(decoder.decode(payload)),
    });
    const channelId = new DataView(
      lastChannelMessage(transport)!.buffer,
    ).getUint32(2, true);
    transport.push(opened(channelId, STATUS_OK, 1024n, "ext:7:1"));
    await pending;

    transport.push(data(channelId, "one"));
    transport.push(data(channelId, "two!"));
    expect(received).toEqual(["one", "two!"]);
    const parsed = parseChannelMessage(lastChannelMessage(transport)!);
    expect(parsed).toMatchObject({ kind: "ack", channelId, bytes: 7n });
  });

  it("holds sends inside the peer's window and releases them on ACK", async () => {
    const { transport, connection } = connect();
    const pending = connection.connectChannel("blit.systemd.v1");
    const channelId = new DataView(
      lastChannelMessage(transport)!.buffer,
    ).getUint32(2, true);
    transport.push(opened(channelId, STATUS_OK, 8n, "ext:7:1"));
    const channel = await pending;

    expect(channel.send("12345678")).toBe(true);
    expect(channel.availableCredit).toBe(0n);
    // Overshooting the window is a protocol violation, so it must not go out.
    expect(channel.send("9")).toBe(false);
    transport.push(ack(channelId, 8n));
    expect(channel.availableCredit).toBe(8n);
    expect(channel.send("9")).toBe(true);
  });

  it("reports a peer close once and stops sending", async () => {
    const { transport, connection } = connect();
    const closures: [number, string][] = [];
    const pending = connection.connectChannel("blit.systemd.v1", {
      onClosed: (reason, detail) => closures.push([reason, detail]),
    });
    const channelId = new DataView(
      lastChannelMessage(transport)!.buffer,
    ).getUint32(2, true);
    transport.push(opened(channelId, STATUS_OK, 1024n, "ext:7:1"));
    const channel = await pending;

    transport.push(closed(channelId, CHANNEL_CLOSE_NORMAL, "done"));
    expect(closures).toEqual([[CHANNEL_CLOSE_NORMAL, "done"]]);
    expect(channel.send("late")).toBe(false);
    transport.push(closed(channelId, CHANNEL_CLOSE_NORMAL, "again"));
    expect(closures).toHaveLength(1);
  });

  it("closes every channel when the transport drops", async () => {
    const { transport, connection } = connect();
    const closures: number[] = [];
    const pending = connection.connectChannel("blit.systemd.v1", {
      onClosed: (reason) => closures.push(reason),
    });
    const channelId = new DataView(
      lastChannelMessage(transport)!.buffer,
    ).getUint32(2, true);
    transport.push(opened(channelId, STATUS_OK, 1024n, "ext:7:1"));
    const channel = await pending;

    transport.setStatus("disconnected");
    expect(closures).toEqual([CHANNEL_CLOSE_PEER_GONE]);
    expect(channel.send("after")).toBe(false);
  });
});
