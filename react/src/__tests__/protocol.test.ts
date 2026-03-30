import { describe, it, expect } from "vitest";
import {
  buildAckMessage,
  buildClearResizeBatchMessage,
  buildClearResizeMessage,
  buildResizeBatchMessage,
  buildClientMetricsMessage,
  buildInputMessage,
  buildResizeMessage,
  buildScrollMessage,
  buildFocusMessage,
  buildCloseMessage,
  buildSubscribeMessage,
  buildUnsubscribeMessage,
  buildSearchMessage,
  buildCreate2Message,
} from "../protocol";
import {
  C2S_ACK,
  C2S_CLIENT_METRICS,
  C2S_INPUT,
  C2S_RESIZE,
  C2S_SCROLL,
  C2S_FOCUS,
  C2S_CLOSE,
  C2S_SUBSCRIBE,
  C2S_UNSUBSCRIBE,
  C2S_SEARCH,
  C2S_CREATE2,
  CREATE2_HAS_SRC_PTY,
  CREATE2_HAS_COMMAND,
} from "../types";

const textDecoder = new TextDecoder();

describe("protocol message builders", () => {
  it("buildAckMessage", () => {
    const msg = buildAckMessage();
    expect(msg).toEqual(new Uint8Array([C2S_ACK]));
  });

  it("buildClientMetricsMessage", () => {
    const msg = buildClientMetricsMessage(3, 5, 27);
    expect(msg[0]).toBe(C2S_CLIENT_METRICS);
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3] | (msg[4] << 8)).toBe(5);
    expect(msg[5] | (msg[6] << 8)).toBe(27);
  });

  it("buildInputMessage", () => {
    const data = new Uint8Array([0x68, 0x69]); // "hi"
    const msg = buildInputMessage(5, data);
    expect(msg[0]).toBe(C2S_INPUT);
    expect(msg[1] | (msg[2] << 8)).toBe(5);
    expect(Array.from(msg.subarray(3))).toEqual([0x68, 0x69]);
  });

  it("buildInputMessage with high ptyId", () => {
    const msg = buildInputMessage(0x1234, new Uint8Array([0x41]));
    expect(msg[1]).toBe(0x34);
    expect(msg[2]).toBe(0x12);
  });

  it("buildResizeMessage", () => {
    const msg = buildResizeMessage(3, 40, 120);
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3] | (msg[4] << 8)).toBe(40);
    expect(msg[5] | (msg[6] << 8)).toBe(120);
    expect(msg.length).toBe(7);
  });

  it("buildResizeBatchMessage", () => {
    const msg = buildResizeBatchMessage([
      { ptyId: 3, rows: 40, cols: 120 },
      { ptyId: 7, rows: 24, cols: 80 },
    ]);
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3] | (msg[4] << 8)).toBe(40);
    expect(msg[5] | (msg[6] << 8)).toBe(120);
    expect(msg[7] | (msg[8] << 8)).toBe(7);
    expect(msg[9] | (msg[10] << 8)).toBe(24);
    expect(msg[11] | (msg[12] << 8)).toBe(80);
    expect(msg.length).toBe(13);
  });

  it("buildClearResizeMessage", () => {
    const msg = buildClearResizeMessage(3);
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3] | (msg[4] << 8)).toBe(0);
    expect(msg[5] | (msg[6] << 8)).toBe(0);
  });

  it("buildClearResizeBatchMessage", () => {
    const msg = buildClearResizeBatchMessage([3, 7]);
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3] | (msg[4] << 8)).toBe(0);
    expect(msg[5] | (msg[6] << 8)).toBe(0);
    expect(msg[7] | (msg[8] << 8)).toBe(7);
    expect(msg[9] | (msg[10] << 8)).toBe(0);
    expect(msg[11] | (msg[12] << 8)).toBe(0);
  });

  it("buildScrollMessage", () => {
    const msg = buildScrollMessage(2, 100);
    expect(msg[0]).toBe(C2S_SCROLL);
    expect(msg[1] | (msg[2] << 8)).toBe(2);
    const offset = msg[3] | (msg[4] << 8) | (msg[5] << 16) | (msg[6] << 24);
    expect(offset).toBe(100);
    expect(msg.length).toBe(7);
  });

  it("buildScrollMessage with large offset", () => {
    const msg = buildScrollMessage(1, 0x00abcdef);
    const offset =
      (msg[3] | (msg[4] << 8) | (msg[5] << 16) | (msg[6] << 24)) >>> 0;
    expect(offset).toBe(0x00abcdef);
  });

  it("buildFocusMessage", () => {
    const msg = buildFocusMessage(9);
    expect(msg).toEqual(new Uint8Array([C2S_FOCUS, 9, 0]));
  });

  it("buildCloseMessage", () => {
    const msg = buildCloseMessage(4);
    expect(msg).toEqual(new Uint8Array([C2S_CLOSE, 4, 0]));
  });

  it("buildSubscribeMessage", () => {
    const msg = buildSubscribeMessage(7);
    expect(msg).toEqual(new Uint8Array([C2S_SUBSCRIBE, 7, 0]));
  });

  it("buildUnsubscribeMessage", () => {
    const msg = buildUnsubscribeMessage(7);
    expect(msg).toEqual(new Uint8Array([C2S_UNSUBSCRIBE, 7, 0]));
  });

  it("buildSearchMessage", () => {
    const msg = buildSearchMessage(42, "hello");
    expect(msg[0]).toBe(C2S_SEARCH);
    expect(msg[1] | (msg[2] << 8)).toBe(42);
    expect(textDecoder.decode(msg.subarray(3))).toBe("hello");
  });

  it("buildSearchMessage with unicode", () => {
    const msg = buildSearchMessage(1, "cafe\u0301");
    expect(textDecoder.decode(msg.subarray(3))).toBe("cafe\u0301");
  });

  describe("buildCreate2Message", () => {
    it("minimal (no options)", () => {
      const msg = buildCreate2Message(1, 24, 80);
      expect(msg[0]).toBe(C2S_CREATE2);
      expect(msg[1] | (msg[2] << 8)).toBe(1); // nonce
      expect(msg[3] | (msg[4] << 8)).toBe(24); // rows
      expect(msg[5] | (msg[6] << 8)).toBe(80); // cols
      expect(msg[7]).toBe(0); // features
      expect(msg[8] | (msg[9] << 8)).toBe(0); // tag length
      expect(msg.length).toBe(10);
    });

    it("with tag", () => {
      const msg = buildCreate2Message(0, 24, 80, { tag: "shell" });
      expect(msg[7]).toBe(0); // no special features
      const tagLen = msg[8] | (msg[9] << 8);
      expect(tagLen).toBe(5);
      expect(textDecoder.decode(msg.subarray(10, 10 + tagLen))).toBe("shell");
      expect(msg.length).toBe(15);
    });

    it("with command", () => {
      const msg = buildCreate2Message(0, 24, 80, { command: "vim" });
      expect(msg[7]).toBe(CREATE2_HAS_COMMAND);
      const tagLen = msg[8] | (msg[9] << 8);
      expect(tagLen).toBe(0);
      expect(textDecoder.decode(msg.subarray(10))).toBe("vim");
    });

    it("with tag and command", () => {
      const msg = buildCreate2Message(5, 30, 120, {
        tag: "dev",
        command: "make build",
      });
      expect(msg[1] | (msg[2] << 8)).toBe(5);
      expect(msg[3] | (msg[4] << 8)).toBe(30);
      expect(msg[5] | (msg[6] << 8)).toBe(120);
      expect(msg[7]).toBe(CREATE2_HAS_COMMAND);
      const tagLen = msg[8] | (msg[9] << 8);
      expect(tagLen).toBe(3);
      expect(textDecoder.decode(msg.subarray(10, 13))).toBe("dev");
      expect(textDecoder.decode(msg.subarray(13))).toBe("make build");
    });

    it("with srcPtyId", () => {
      const msg = buildCreate2Message(0, 24, 80, { srcPtyId: 7 });
      expect(msg[7]).toBe(CREATE2_HAS_SRC_PTY);
      const tagLen = msg[8] | (msg[9] << 8);
      expect(tagLen).toBe(0);
      expect(msg[10]).toBe(7);
      expect(msg[11]).toBe(0);
      expect(msg.length).toBe(12);
    });

    it("with tag, srcPtyId, and command", () => {
      const msg = buildCreate2Message(0, 24, 80, {
        tag: "x",
        srcPtyId: 0x0102,
        command: "ls",
      });
      expect(msg[7]).toBe(CREATE2_HAS_SRC_PTY | CREATE2_HAS_COMMAND);
      const tagLen = msg[8] | (msg[9] << 8);
      expect(tagLen).toBe(1);
      expect(textDecoder.decode(msg.subarray(10, 11))).toBe("x");
      // srcPtyId after tag
      expect(msg[11]).toBe(0x02);
      expect(msg[12]).toBe(0x01);
      // command after srcPtyId
      expect(textDecoder.decode(msg.subarray(13))).toBe("ls");
    });

    it("trims whitespace-only command", () => {
      const msg = buildCreate2Message(0, 24, 80, { command: "  " });
      expect(msg[7]).toBe(0); // no command feature
      expect(msg.length).toBe(10);
    });
  });
});
