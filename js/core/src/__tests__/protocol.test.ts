import { describe, it, expect } from "vitest";
import {
  buildAckMessage,
  buildClearResizeBatchMessage,
  buildClearResizeMessage,
  buildResizeBatchMessage,
  buildClientMetricsMessage,
  buildDisplayRateMessage,
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
  buildSurfaceAckMessage,
  buildSurfaceSubscribeMessage,
  buildClientFeaturesMessage,
  buildClipboardGetMessage,
  buildClipboardListMessage,
  buildSurfacePreeditMessage,
  buildSurfaceDragEnterMessage,
  buildSurfaceTouchMessage,
} from "../protocol";
import {
  C2S_ACK,
  C2S_CLIENT_METRICS,
  C2S_DISPLAY_RATE,
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
  C2S_SURFACE_ACK,
  C2S_SURFACE_SUBSCRIBE,
  C2S_CLIENT_FEATURES,
  C2S_CLIPBOARD_GET,
  C2S_CLIPBOARD_LIST,
  C2S_SURFACE_PREEDIT,
  AXIS_SOURCE_FINGER,
  AXIS_SOURCE_WHEEL,
  AXIS_FLAG_SOURCE_KNOWN,
  AXIS_FLAG_STOP,
  CREATE2_WANT_STATUS,
  C2S_SURFACE_TOUCH,
  SURFACE_TOUCH_MOTION,
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

  it("buildDisplayRateMessage preserves precise unsnapped refresh", () => {
    const msg = buildDisplayRateMessage(239.976);
    const view = new DataView(msg.buffer);
    expect(msg[0]).toBe(C2S_DISPLAY_RATE);
    expect(view.getUint16(1, true)).toBe(240);
    expect(view.getUint32(3, true)).toBe(239_976);
  });

  it("buildInputMessage", () => {
    const data = new Uint8Array([0x68, 0x69]); // "hi"
    const msg = buildInputMessage(5, data);
    expect(msg[0]).toBe(C2S_INPUT);
    expect(msg[1] | (msg[2] << 8)).toBe(5);
    expect(Array.from(msg.subarray(3))).toEqual([0x68, 0x69]);
  });

  it("buildClipboardListMessage", () => {
    expect(buildClipboardListMessage()).toEqual(
      new Uint8Array([C2S_CLIPBOARD_LIST]),
    );
  });

  it("buildClipboardGetMessage", () => {
    const msg = buildClipboardGetMessage("text/plain");
    const view = new DataView(msg.buffer);
    expect(msg[0]).toBe(C2S_CLIPBOARD_GET);
    expect(view.getUint16(1, true)).toBe(10);
    expect(textDecoder.decode(msg.subarray(3))).toBe("text/plain");
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

    it("wantStatus sets the flag without a trailing field", () => {
      const msg = buildCreate2Message(0, 24, 80, { wantStatus: true });
      expect(msg[7]).toBe(CREATE2_WANT_STATUS);
      expect(msg.length).toBe(10);
    });

    it("wantStatus combines with the other feature bits", () => {
      const msg = buildCreate2Message(0, 24, 80, {
        command: "vim",
        cwd: "/tmp",
        wantStatus: true,
      });
      expect(msg[7]).toBe(
        CREATE2_HAS_CWD | CREATE2_HAS_COMMAND | CREATE2_WANT_STATUS,
      );
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
      timeMs: v.getUint32(16, true),
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
      timeMs: 9_876.4,
    });
    expect(msg).toHaveLength(20);
    expect(read(msg)).toEqual({
      opcode: C2S_SURFACE_POINTER_AXIS2,
      surfaceId: 0x1234,
      flags: AXIS_SOURCE_FINGER | AXIS_FLAG_SOURCE_KNOWN,
      dxX100: -150,
      dyX100: 225,
      v120x: 0,
      v120y: -240,
      // Kinetic scrolling integrates deltas against these timestamps, so the
      // browser's own event time has to reach the compositor.
      timeMs: 9_876,
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

describe("buildSurfaceTouchMessage", () => {
  it("keeps contacts from one browser event in one wire frame", () => {
    const msg = buildSurfaceTouchMessage(
      0x1234,
      SURFACE_TOUCH_MOTION,
      [
        { identifier: -7, x: 12.25, y: -3.5 },
        { identifier: 9, x: 640, y: 480.75 },
      ],
      1234.6,
    );
    const view = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    expect(msg).toHaveLength(33);
    expect(msg[0]).toBe(C2S_SURFACE_TOUCH);
    expect(view.getUint16(1, true)).toBe(0x1234);
    expect(msg[3]).toBe(SURFACE_TOUCH_MOTION);
    expect(msg[4]).toBe(2);
    // The browser's own event time, to whole ms. Apps differentiate position
    // against it for a fling velocity, so it cannot be stamped on arrival: a
    // burst of coalesced moves would then share one instant.
    expect(view.getUint32(5, true)).toBe(1235);
    expect(view.getInt32(9, true)).toBe(-7);
    expect(view.getInt32(13, true)).toBe(1225);
    expect(view.getInt32(17, true)).toBe(-350);
    expect(view.getInt32(21, true)).toBe(9);
    expect(view.getInt32(25, true)).toBe(64000);
    expect(view.getInt32(29, true)).toBe(48075);
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
      maxFps: msg.length >= 12 ? v.getUint16(10, true) : 0,
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
      maxFps: 0,
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

  it("appends a per-surface frame-rate ceiling", () => {
    const msg = buildSurfaceSubscribeMessage(1, 0, 0, 0, 320, 180, 15);
    expect(msg).toHaveLength(12);
    expect(read(msg)).toMatchObject({ width: 320, height: 180, maxFps: 15 });
  });

  it("can request a cadence without a scaled target", () => {
    const msg = buildSurfaceSubscribeMessage(1, 0, 0, 0, 0, 0, 30);
    expect(msg).toHaveLength(12);
    expect(read(msg)).toMatchObject({ width: 0, height: 0, maxFps: 30 });
  });
});

describe("buildSurfaceAckMessage", () => {
  it("appends decoder depth without changing the legacy prefix", () => {
    expect(buildSurfaceAckMessage(0x1234, 7)).toEqual(
      new Uint8Array([C2S_SURFACE_ACK, 0x34, 0x12, 7]),
    );
  });

  it("clamps decoder queue depth to one wire byte", () => {
    expect(buildSurfaceAckMessage(1, 999)[3]).toBe(255);
    expect(buildSurfaceAckMessage(1, -1)[3]).toBe(0);
  });
});

/**
 * C2S_CLIENT_FEATURES carries the decoder's frame-size ceiling alongside
 * the codec bitmask.  The server holds an undeclared client to the H.264
 * ceiling, so getting these bytes wrong silently caps every surface at 4K
 * (or, worse, unlocks 5K for a decoder that can't take it).
 */
describe("buildClientFeaturesMessage", () => {
  it("packs the decode ceiling as little-endian u16s", () => {
    const msg = buildClientFeaturesMessage(0x03, 5120, 2880);
    expect(msg).toHaveLength(7);
    expect(msg[0]).toBe(C2S_CLIENT_FEATURES);
    expect(msg[1]).toBe(0x03);
    expect(msg[2] | (msg[3] << 8)).toBe(5120);
    expect(msg[4] | (msg[5] << 8)).toBe(2880);
    expect(msg[6]).toBe(1);
  });

  it("defaults the ceiling to zero, which the server reads as undeclared", () => {
    const msg = buildClientFeaturesMessage(0x02);
    expect(msg).toHaveLength(7);
    expect(msg[1]).toBe(0x02);
    expect(msg[2] | (msg[3] << 8)).toBe(0);
    expect(msg[4] | (msg[5] << 8)).toBe(0);
  });
});

/**
 * The cursor is a byte offset on the wire, because that is what
 * zwp_text_input_v3 counts in — but the DOM hands us a UTF-16 offset. The two
 * agree only for ASCII, and a composition is made of exactly the characters
 * where they don't: a caret sent as a UTF-16 offset lands mid-codepoint in
 * the app, which is where it draws the cursor.
 */
describe("buildSurfacePreeditMessage", () => {
  it("converts the caret from UTF-16 units to bytes", () => {
    // "にほ" is 2 UTF-16 units and 6 UTF-8 bytes.
    const msg = buildSurfacePreeditMessage(7, "にほn", 2);
    expect(msg[0]).toBe(C2S_SURFACE_PREEDIT);
    expect(msg[1] | (msg[2] << 8)).toBe(7);
    expect(msg[3] | (msg[4] << 8)).toBe(6);
    expect(new TextDecoder().decode(msg.slice(5))).toBe("にほn");
  });

  it("carries an empty composition, which withdraws it", () => {
    const msg = buildSurfacePreeditMessage(7, "", 0);
    expect(msg).toHaveLength(5);
    expect(msg[3] | (msg[4] << 8)).toBe(0);
  });
});

describe("buildSurfaceDragEnterMessage", () => {
  const hex = (msg: Uint8Array) =>
    Array.from(msg, (b) => b.toString(16).padStart(2, "0")).join("");

  // The wire layout, pinned: the Rust side pins this exact fixture.
  // [0x35][surface:2][x:2][y:2][mime_count:2][mime_len:2][mime]... then the
  // optional item trailer [item_count:2][mime_len:2][mime]...
  const ENTER_NO_ITEMS =
    "35" + // C2S_SURFACE_DRAG_ENTER
    "0700" + // surface 7
    "6400" + // x 100
    "c800" + // y 200
    "0200" + // two offered MIMEs
    "0d00" +
    "746578742f7572692d6c697374" + // "text/uri-list"
    "1800" +
    "6170706c69636174696f6e2f6f637465742d73747265616d"; // "application/octet-stream"

  it("appends the item MIMEs as a trailer", () => {
    expect(
      hex(
        buildSurfaceDragEnterMessage(
          7,
          100,
          200,
          ["text/uri-list", "application/octet-stream"],
          ["image/png", "image/jpeg"],
        ),
      ),
    ).toBe(
      ENTER_NO_ITEMS +
        "0200" + // two items
        "0900" +
        "696d6167652f706e67" + // "image/png"
        "0a00" +
        "696d6167652f6a706567", // "image/jpeg"
    );
  });

  it("stays byte-identical without items", () => {
    expect(
      hex(
        buildSurfaceDragEnterMessage(7, 100, 200, [
          "text/uri-list",
          "application/octet-stream",
        ]),
      ),
    ).toBe(ENTER_NO_ITEMS);
  });
});
