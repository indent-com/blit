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
  buildSurfaceAxis2Message,
  buildSurfaceSubscribeMessage,
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
  CREATE2_HAS_CWD,
  C2S_SURFACE_POINTER_AXIS2,
  C2S_SURFACE_SUBSCRIBE,
  AXIS_SOURCE_FINGER,
  AXIS_SOURCE_WHEEL,
  AXIS_FLAG_SOURCE_KNOWN,
  AXIS_FLAG_STOP,
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

    it("with cwd", () => {
      const msg = buildCreate2Message(0, 24, 80, { cwd: "/src/blit" });
      expect(msg[7]).toBe(CREATE2_HAS_CWD);
      const tagLen = msg[8] | (msg[9] << 8);
      expect(tagLen).toBe(0);
      const cwdLen = msg[10] | (msg[11] << 8);
      expect(cwdLen).toBe(9);
      expect(textDecoder.decode(msg.subarray(12, 12 + cwdLen))).toBe(
        "/src/blit",
      );
      expect(msg.length).toBe(21);
    });

    it("with srcPtyId, cwd, and command", () => {
      const msg = buildCreate2Message(0, 24, 80, {
        srcPtyId: 0x0102,
        cwd: "/tmp",
        command: "pwd",
      });
      expect(msg[7]).toBe(
        CREATE2_HAS_SRC_PTY | CREATE2_HAS_CWD | CREATE2_HAS_COMMAND,
      );
      expect(msg[10]).toBe(0x02);
      expect(msg[11]).toBe(0x01);
      const cwdLen = msg[12] | (msg[13] << 8);
      expect(cwdLen).toBe(4);
      expect(textDecoder.decode(msg.subarray(14, 18))).toBe("/tmp");
      expect(textDecoder.decode(msg.subarray(18))).toBe("pwd");
    });

    it("trims whitespace-only command", () => {
      const msg = buildCreate2Message(0, 24, 80, { command: "  " });
      expect(msg[7]).toBe(0); // no command feature
      expect(msg.length).toBe(10);
    });
  });
});

/** The Rust side parses these bytes by fixed offset, so the layout is the
 *  contract — see `parse_surface_pointer_axis2` in crates/remote. */
describe("buildSurfaceAxis2Message", () => {
  const read = (msg: Uint8Array) => {
    const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    return {
      opcode: msg[0],
      surfaceId: v.getUint16(1, true),
      flags: msg[3],
      dxX100: v.getInt32(4, true),
      dyX100: v.getInt32(8, true),
      v120x: v.getInt16(12, true),
      v120y: v.getInt16(14, true),
    };
  };

  it("lays the fields out where the server reads them", () => {
    const msg = buildSurfaceAxis2Message(0x1234, {
      dx: -1.5,
      dy: 2.25,
      v120x: 0,
      v120y: -240,
      source: AXIS_SOURCE_FINGER,
      stop: false,
    });
    expect(msg).toHaveLength(16);
    expect(read(msg)).toEqual({
      opcode: C2S_SURFACE_POINTER_AXIS2,
      surfaceId: 0x1234,
      flags: AXIS_SOURCE_FINGER | AXIS_FLAG_SOURCE_KNOWN,
      dxX100: -150,
      dyX100: 225,
      v120x: 0,
      v120y: -240,
    });
  });

  /** A wheel source is 0, so only the "known" bit separates it from an
   *  unclassified scroll — get this wrong and every trackpad gesture is
   *  labelled a wheel. */
  it("distinguishes a wheel from an unclassified source", () => {
    const wheel = buildSurfaceAxis2Message(1, {
      dx: 0,
      dy: 1,
      v120x: 0,
      v120y: 0,
      source: AXIS_SOURCE_WHEEL,
      stop: false,
    });
    const unknown = buildSurfaceAxis2Message(1, {
      dx: 0,
      dy: 1,
      v120x: 0,
      v120y: 0,
      source: null,
      stop: false,
    });
    expect(read(wheel).flags).toBe(AXIS_FLAG_SOURCE_KNOWN);
    expect(read(unknown).flags).toBe(0);
  });

  it("marks a stop", () => {
    const msg = buildSurfaceAxis2Message(1, {
      dx: 0,
      dy: 0,
      v120x: 0,
      v120y: 0,
      source: AXIS_SOURCE_FINGER,
      stop: true,
    });
    expect(read(msg).flags).toBe(
      AXIS_SOURCE_FINGER | AXIS_FLAG_SOURCE_KNOWN | AXIS_FLAG_STOP,
    );
  });

  /** A runaway delta must not wrap into a scroll the other direction. */
  it("clamps rather than wraps out-of-range values", () => {
    const msg = buildSurfaceAxis2Message(1, {
      dx: 0,
      dy: 1e12,
      v120x: 0,
      v120y: 1e6,
      source: AXIS_SOURCE_WHEEL,
      stop: false,
    });
    expect(read(msg).dyX100).toBe(2147483647);
    expect(read(msg).v120y).toBe(32767);
  });

  it("survives a non-finite delta", () => {
    const msg = buildSurfaceAxis2Message(1, {
      dx: NaN,
      dy: Infinity,
      v120x: 0,
      v120y: 0,
      source: null,
      stop: false,
    });
    expect(read(msg).dxX100).toBe(0);
    expect(read(msg).dyX100).toBe(0);
  });
});

/** The Rust side parses these bytes by fixed offset, so the layout is the
 *  contract — see the `C2S_SURFACE_SUBSCRIBE` arm in crates/server, which
 *  reads the size from bytes 6..10 and only when at least 10 arrived. */
describe("buildSurfaceSubscribeMessage", () => {
  const read = (msg: Uint8Array) => {
    const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    return {
      opcode: msg[0],
      surfaceId: v.getUint16(1, true),
      codec: msg[3],
      bandwidth: msg[4],
      speed: msg[5],
      width: v.getUint16(6, true),
      height: v.getUint16(8, true),
    };
  };

  it("stays at the 3-byte form when nothing is overridden", () => {
    const msg = buildSurfaceSubscribeMessage(7);
    expect(msg).toHaveLength(3);
    expect(msg[0]).toBe(C2S_SURFACE_SUBSCRIBE);
  });

  it("uses the 6-byte form for preferences alone", () => {
    expect(buildSurfaceSubscribeMessage(7, 0, 2, 3)).toHaveLength(6);
  });

  it("lays the scaled size out where the server reads it", () => {
    const msg = buildSurfaceSubscribeMessage(0x1234, 0x0f, 2, 3, 1472, 2092);
    expect(msg).toHaveLength(10);
    expect(read(msg)).toEqual({
      opcode: C2S_SURFACE_SUBSCRIBE,
      surfaceId: 0x1234,
      codec: 0x0f,
      bandwidth: 2,
      speed: 3,
      width: 1472,
      height: 2092,
    });
  });

  it("reaches the long form for a size even at default preferences", () => {
    // The size lives past the preference bytes, so shortening the message
    // would drop it — a thumbnail asking for a stream at server defaults
    // would silently get a full-size one.
    const msg = buildSurfaceSubscribeMessage(1, 0, 0, 0, 320, 180);
    expect(msg).toHaveLength(10);
    expect(read(msg)).toMatchObject({ width: 320, height: 180 });
  });

  it("treats a half-specified size as no size at all", () => {
    // The server requires both axes nonzero; emitting one would be read as
    // mediated anyway, so don't pay for the longer message.
    expect(buildSurfaceSubscribeMessage(1, 0, 0, 0, 320, 0)).toHaveLength(3);
    expect(buildSurfaceSubscribeMessage(1, 0, 0, 0, 0, 180)).toHaveLength(3);
  });
});
