import { describe, expect, it } from "vitest";
import {
  CHANNEL,
  CHANNEL_ACCEPTED,
  CHANNEL_CLOSE_CANCELLED,
  CHANNEL_DATA,
  CHANNEL_EXPECT_LISTENER_TOKEN,
  CHANNEL_NAMES,
  CHANNEL_OPENED,
  CHANNEL_UNWATCH,
  CHANNEL_WATCH,
  CHANNEL_WINDOW_BYTES,
  buildChannelAckMessage,
  buildChannelCloseMessage,
  buildChannelConnectMessage,
  buildChannelDataMessage,
  buildChannelListenMessage,
  buildChannelUnwatchMessage,
  buildChannelWatchMessage,
  parseChannelMessage,
} from "../channel";

const encoder = new TextEncoder();

function opened(
  channelId: number,
  status: number,
  peer: string,
  metadata: Uint8Array,
  detail: string,
): Uint8Array {
  const peerBytes = encoder.encode(peer);
  const detailBytes = encoder.encode(detail);
  const message = new Uint8Array(
    6 + 1 + 8 + 2 + peerBytes.length + 4 + metadata.length + detailBytes.length,
  );
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_OPENED;
  view.setUint32(2, channelId, true);
  message[6] = status;
  view.setBigUint64(7, status === 0 ? CHANNEL_WINDOW_BYTES : 0n, true);
  view.setUint16(15, peerBytes.length, true);
  message.set(peerBytes, 17);
  let offset = 17 + peerBytes.length;
  view.setUint32(offset, metadata.length, true);
  offset += 4;
  message.set(metadata, offset);
  offset += metadata.length;
  message.set(detailBytes, offset);
  return message;
}

function accepted(
  channelId: number,
  listenerId: number,
  peer: string,
  metadata: Uint8Array,
): Uint8Array {
  const peerBytes = encoder.encode(peer);
  const message = new Uint8Array(
    6 + 4 + 8 + 2 + peerBytes.length + 4 + metadata.length,
  );
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_ACCEPTED;
  view.setUint32(2, channelId, true);
  view.setUint32(6, listenerId, true);
  view.setBigUint64(10, CHANNEL_WINDOW_BYTES, true);
  view.setUint16(18, peerBytes.length, true);
  message.set(peerBytes, 20);
  let offset = 20 + peerBytes.length;
  view.setUint32(offset, metadata.length, true);
  offset += 4;
  message.set(metadata, offset);
  return message;
}

function namesMessage(
  channelId: number,
  present: readonly string[],
): Uint8Array {
  const encoded = present.map((name) => encoder.encode(name));
  const message = new Uint8Array(
    9 + encoded.reduce((total, name) => total + 2 + name.length, 0),
  );
  const view = new DataView(message.buffer);
  message[0] = CHANNEL;
  message[1] = CHANNEL_NAMES;
  view.setUint32(2, channelId, true);
  view.setUint16(7, encoded.length, true);
  let offset = 9;
  for (const name of encoded) {
    view.setUint16(offset, name.length, true);
    offset += 2;
    message.set(name, offset);
    offset += name.length;
  }
  return message;
}

describe("native channel wire protocol", () => {
  it("builds LISTEN and token-checked CONNECT envelopes", () => {
    const listen = buildChannelListenMessage(
      2,
      "com.example.builder",
      new Uint8Array([7]),
    );
    expect(listen[0]).toBe(CHANNEL);
    expect(listen[1]).toBe(1);
    expect(new DataView(listen.buffer).getUint32(2, true)).toBe(2);
    expect(listen[6]).toBe(0);

    const token = new Uint8Array(16).fill(9);
    const connect = buildChannelConnectMessage(4, "com.example.builder", {
      metadata: new Uint8Array([8]),
      listenerToken: token,
    });
    expect(connect[6]).toBe(CHANNEL_EXPECT_LISTENER_TOKEN);
    expect(Array.from(connect.subarray(-16))).toEqual(Array.from(token));
  });

  it("builds DATA, ACK, and CLOSE without losing u64 precision", () => {
    const data = buildChannelDataMessage(2, new Uint8Array([1, 2, 3]));
    expect(Array.from(data)).toEqual([
      CHANNEL,
      CHANNEL_DATA,
      2,
      0,
      0,
      0,
      1,
      2,
      3,
    ]);

    const bytes = 0xfedcba9876543210n;
    const ack = buildChannelAckMessage(3, bytes);
    expect(new DataView(ack.buffer).getBigUint64(6, true)).toBe(bytes);
    expect(
      Array.from(buildChannelCloseMessage(2, CHANNEL_CLOSE_CANCELLED)),
    ).toEqual([CHANNEL, 5, 2, 0, 0, 0, CHANNEL_CLOSE_CANCELLED]);
  });

  it("parses OPENED and ACCEPTED metadata", () => {
    expect(
      parseChannelMessage(
        opened(4, 0, "client:0000000000000001", new Uint8Array([1, 2]), ""),
      ),
    ).toEqual({
      kind: "opened",
      channelId: 4,
      status: 0,
      window: CHANNEL_WINDOW_BYTES,
      peer: "client:0000000000000001",
      metadata: new Uint8Array([1, 2]),
      detail: "",
    });
    expect(
      parseChannelMessage(
        accepted(3, 2, "ext:0000000000000002:7", new Uint8Array([3])),
      ),
    ).toEqual({
      kind: "accepted",
      channelId: 3,
      listenerId: 2,
      window: CHANNEL_WINDOW_BYTES,
      peer: "ext:0000000000000002:7",
      metadata: new Uint8Array([3]),
    });
  });

  it("carries a watch's names and reads the answer back", () => {
    const watch = buildChannelWatchMessage(2, [
      "blit.session.v1",
      "blit.systemd.v1",
    ]);
    expect(watch[1]).toBe(CHANNEL_WATCH);
    expect(new DataView(watch.buffer).getUint16(7, true)).toBe(2);
    expect(buildChannelUnwatchMessage(2)).toEqual(
      new Uint8Array([CHANNEL, CHANNEL_UNWATCH, 2, 0, 0, 0]),
    );

    // The reply repeats names rather than answering with a bitmap over the
    // request, so it can be read without remembering what was asked.
    expect(parseChannelMessage(namesMessage(2, ["blit.systemd.v1"]))).toEqual({
      kind: "names",
      channelId: 2,
      names: ["blit.systemd.v1"],
    });
    // Nothing claimed is an answer, not an absent packet.
    expect(parseChannelMessage(namesMessage(2, []))).toEqual({
      kind: "names",
      channelId: 2,
      names: [],
    });
  });

  it("refuses a watch it could not answer unambiguously", () => {
    expect(() => buildChannelWatchMessage(2, [])).toThrow(/at least one/);
    expect(() => buildChannelWatchMessage(3, ["a"])).toThrow(/even/);
    expect(() => buildChannelWatchMessage(2, ["a", "a"])).toThrow(/distinct/);
    expect(() =>
      buildChannelWatchMessage(
        2,
        Array.from({ length: 33 }, (_unused, index) => `name.${index}`),
      ),
    ).toThrow(/32 names/);

    // A count that outruns the body, and a reserved flag byte that is not.
    expect(
      parseChannelMessage(
        new Uint8Array([CHANNEL, CHANNEL_NAMES, 2, 0, 0, 0, 0, 1, 0]),
      ),
    ).toBeNull();
    expect(
      parseChannelMessage(
        new Uint8Array([CHANNEL, CHANNEL_NAMES, 2, 0, 0, 0, 1, 0, 0]),
      ),
    ).toBeNull();
  });

  it("rejects malformed bounds and reserved client values", () => {
    expect(() => buildChannelListenMessage(3, "odd")).toThrow(/even/);
    expect(() => buildChannelListenMessage(2, "bad\nname")).toThrow(/control/);
    expect(() => buildChannelDataMessage(2, new Uint8Array(0))).toThrow(
      /1 byte/,
    );
    expect(() => buildChannelCloseMessage(2, 4)).toThrow(/invalid/);

    expect(
      parseChannelMessage(new Uint8Array([CHANNEL, CHANNEL_DATA, 2, 0, 0, 0])),
    ).toBeNull();
    const badPeer = opened(2, 0, "bad\npeer", new Uint8Array(0), "");
    expect(parseChannelMessage(badPeer)).toBeNull();
  });
});
