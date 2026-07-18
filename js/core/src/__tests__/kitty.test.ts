import { describe, it, expect } from "vitest";
import {
  encodeKittyKey,
  KITTY_DISAMBIGUATE,
  KITTY_EVENT_TYPES,
  KITTY_ALTERNATE,
  type KittyEventType,
} from "../kitty";

const enc = new TextEncoder();

function makeEvent(
  key: string,
  opts: Partial<KeyboardEvent> = {},
): KeyboardEvent {
  return {
    key,
    code: opts.code ?? "",
    ctrlKey: opts.ctrlKey ?? false,
    shiftKey: opts.shiftKey ?? false,
    altKey: opts.altKey ?? false,
    metaKey: opts.metaKey ?? false,
    isComposing: false,
  } as KeyboardEvent;
}

/** Convenience wrapper defaulting event type / appCursor. */
function encode(
  key: string,
  flags: number,
  opts: Partial<KeyboardEvent> = {},
  eventType: KittyEventType = "press",
  appCursor = false,
): Uint8Array | null {
  return encodeKittyKey(makeEvent(key, opts), flags, eventType, appCursor);
}

const DISAMBIG = KITTY_DISAMBIGUATE; // 1
const EVENTS = KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES; // 3
const ALTERNATE = KITTY_DISAMBIGUATE | KITTY_ALTERNATE; // 5

describe("encodeKittyKey", () => {
  describe("editing keys", () => {
    it("Shift+Enter → CSI 13;2 u", () => {
      expect(encode("Enter", DISAMBIG, { shiftKey: true })).toEqual(
        enc.encode("\x1b[13;2u"),
      );
    });

    it("Ctrl+Enter → CSI 13;5 u", () => {
      expect(encode("Enter", DISAMBIG, { ctrlKey: true })).toEqual(
        enc.encode("\x1b[13;5u"),
      );
    });

    it("Cmd+Backspace → CSI 127;9 u", () => {
      expect(encode("Backspace", DISAMBIG, { metaKey: true })).toEqual(
        enc.encode("\x1b[127;9u"),
      );
    });

    it("plain Enter / Tab / Backspace stay legacy", () => {
      expect(encode("Enter", DISAMBIG)).toEqual(enc.encode("\r"));
      expect(encode("Tab", DISAMBIG)).toEqual(enc.encode("\t"));
      expect(encode("Backspace", DISAMBIG)).toEqual(enc.encode("\x7f"));
    });

    it("editing-key releases are never forwarded", () => {
      expect(encode("Enter", EVENTS, { shiftKey: true }, "release")).toBeNull();
      expect(encode("Backspace", EVENTS, {}, "release")).toBeNull();
    });
  });

  describe("escape", () => {
    it("unmodified Escape → CSI 27 u", () => {
      expect(encode("Escape", DISAMBIG)).toEqual(enc.encode("\x1b[27u"));
    });

    it("Escape release (event types) → CSI 27;1:3 u", () => {
      expect(encode("Escape", EVENTS, {}, "release")).toEqual(
        enc.encode("\x1b[27;1:3u"),
      );
    });
  });

  describe("text keys", () => {
    it("plain 'a' press → text, release → null", () => {
      expect(
        encode("a", KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES | KITTY_ALTERNATE),
      ).toEqual(enc.encode("a"));
      expect(
        encode(
          "a",
          KITTY_DISAMBIGUATE | KITTY_EVENT_TYPES | KITTY_ALTERNATE,
          {},
          "release",
        ),
      ).toBeNull();
    });

    it("Ctrl+C → CSI 99;5 u", () => {
      expect(encode("c", DISAMBIG, { ctrlKey: true })).toEqual(
        enc.encode("\x1b[99;5u"),
      );
    });

    it("Cmd+a → CSI 97;9 u when kitty active (was null before)", () => {
      expect(encode("a", DISAMBIG, { metaKey: true })).toEqual(
        enc.encode("\x1b[97;9u"),
      );
    });

    it("Ctrl+Shift+A with alternate flag → CSI 97:65;6 u", () => {
      expect(
        encode("A", ALTERNATE, {
          ctrlKey: true,
          shiftKey: true,
          code: "KeyA",
        }),
      ).toEqual(enc.encode("\x1b[97:65;6u"));
    });
  });

  describe("functional keys", () => {
    it("ArrowUp appCursor unmodified press → legacy SS3", () => {
      expect(encode("ArrowUp", DISAMBIG, {}, "press", true)).toEqual(
        enc.encode("\x1bOA"),
      );
    });

    it("ArrowUp repeat (event types) → CSI 1;1:2 A", () => {
      expect(encode("ArrowUp", EVENTS, {}, "repeat")).toEqual(
        enc.encode("\x1b[1;1:2A"),
      );
    });

    it("ArrowUp release (event types) → CSI 1;1:3 A", () => {
      expect(encode("ArrowUp", EVENTS, {}, "release")).toEqual(
        enc.encode("\x1b[1;1:3A"),
      );
    });

    it("Shift+ArrowLeft → CSI 1;2 D", () => {
      expect(encode("ArrowLeft", DISAMBIG, { shiftKey: true })).toEqual(
        enc.encode("\x1b[1;2D"),
      );
    });

    it("Ctrl+PageUp → CSI 5;5 ~", () => {
      expect(encode("PageUp", DISAMBIG, { ctrlKey: true })).toEqual(
        enc.encode("\x1b[5;5~"),
      );
    });
  });

  describe("non-forwarded events", () => {
    it("modifier-only keys → null", () => {
      for (const k of ["Shift", "Control", "Alt", "Meta"]) {
        expect(encode(k, EVENTS)).toBeNull();
      }
    });

    it("release without event reporting → null", () => {
      expect(encode("ArrowUp", DISAMBIG, {}, "release")).toBeNull();
    });
  });
});
