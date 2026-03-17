import { describe, it, expect } from "vitest";
import { keyToBytes } from "../keyboard";

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

describe("keyToBytes", () => {
  describe("printable characters", () => {
    it("single ascii character", () => {
      const bytes = keyToBytes(makeEvent("a"), false);
      expect(bytes).toEqual(new TextEncoder().encode("a"));
    });

    it("uppercase character", () => {
      const bytes = keyToBytes(makeEvent("A", { shiftKey: true }), false);
      expect(bytes).toEqual(new TextEncoder().encode("A"));
    });

    it("space", () => {
      const bytes = keyToBytes(makeEvent(" "), false);
      expect(bytes).toEqual(new TextEncoder().encode(" "));
    });
  });

  describe("simple keys", () => {
    it("Enter sends CR", () => {
      expect(Array.from(keyToBytes(makeEvent("Enter"), false)!)).toEqual([
        0x0d,
      ]);
    });

    it("Backspace sends DEL", () => {
      expect(Array.from(keyToBytes(makeEvent("Backspace"), false)!)).toEqual([
        0x7f,
      ]);
    });

    it("Tab sends HT", () => {
      expect(Array.from(keyToBytes(makeEvent("Tab"), false)!)).toEqual([0x09]);
    });

    it("Escape sends ESC", () => {
      expect(Array.from(keyToBytes(makeEvent("Escape"), false)!)).toEqual([
        0x1b,
      ]);
    });
  });

  describe("ctrl sequences", () => {
    it("Ctrl+C sends 0x03", () => {
      const bytes = keyToBytes(
        makeEvent("c", { ctrlKey: true, code: "KeyC" }),
        false,
      );
      expect(bytes).toEqual(new Uint8Array([0x03]));
    });

    it("Ctrl+A sends 0x01", () => {
      const bytes = keyToBytes(
        makeEvent("a", { ctrlKey: true, code: "KeyA" }),
        false,
      );
      expect(bytes).toEqual(new Uint8Array([0x01]));
    });

    it("Ctrl+Z sends 0x1A", () => {
      const bytes = keyToBytes(
        makeEvent("z", { ctrlKey: true, code: "KeyZ" }),
        false,
      );
      expect(bytes).toEqual(new Uint8Array([0x1a]));
    });

    it("Ctrl+[ sends ESC", () => {
      const bytes = keyToBytes(
        makeEvent("[", { ctrlKey: true, code: "BracketLeft" }),
        false,
      );
      expect(bytes).toEqual(new Uint8Array([0x1b]));
    });
  });

  describe("arrow keys", () => {
    it("ArrowUp in normal mode", () => {
      const bytes = keyToBytes(makeEvent("ArrowUp"), false);
      expect(bytes).toEqual(new TextEncoder().encode("\x1b[A"));
    });

    it("ArrowUp in app cursor mode", () => {
      const bytes = keyToBytes(makeEvent("ArrowUp"), true);
      expect(bytes).toEqual(new TextEncoder().encode("\x1bOA"));
    });

    it("ArrowDown with shift modifier", () => {
      const bytes = keyToBytes(
        makeEvent("ArrowDown", { shiftKey: true }),
        false,
      );
      expect(bytes).toEqual(new TextEncoder().encode("\x1b[1;2B"));
    });

    it("ArrowRight with ctrl modifier", () => {
      const bytes = keyToBytes(
        makeEvent("ArrowRight", { ctrlKey: true }),
        false,
      );
      expect(bytes).toEqual(new TextEncoder().encode("\x1b[1;5C"));
    });
  });

  describe("function keys", () => {
    it("F1 sends ESC O P", () => {
      expect(keyToBytes(makeEvent("F1"), false)).toEqual(
        new TextEncoder().encode("\x1bOP"),
      );
    });

    it("F5 sends tilde sequence", () => {
      expect(keyToBytes(makeEvent("F5"), false)).toEqual(
        new TextEncoder().encode("\x1b[15~"),
      );
    });

    it("F12 sends tilde sequence", () => {
      expect(keyToBytes(makeEvent("F12"), false)).toEqual(
        new TextEncoder().encode("\x1b[24~"),
      );
    });
  });

  describe("navigation keys", () => {
    it("Home sends ESC [ H", () => {
      expect(keyToBytes(makeEvent("Home"), false)).toEqual(
        new TextEncoder().encode("\x1b[H"),
      );
    });

    it("End sends ESC [ F", () => {
      expect(keyToBytes(makeEvent("End"), false)).toEqual(
        new TextEncoder().encode("\x1b[F"),
      );
    });

    it("PageUp sends tilde sequence", () => {
      expect(keyToBytes(makeEvent("PageUp"), false)).toEqual(
        new TextEncoder().encode("\x1b[5~"),
      );
    });

    it("Delete sends tilde sequence", () => {
      expect(keyToBytes(makeEvent("Delete"), false)).toEqual(
        new TextEncoder().encode("\x1b[3~"),
      );
    });
  });

  describe("alt sequences", () => {
    it("Alt+a sends ESC a", () => {
      const bytes = keyToBytes(makeEvent("a", { altKey: true }), false);
      expect(bytes).toEqual(new TextEncoder().encode("\x1ba"));
    });
  });

  describe("ignored keys", () => {
    it("returns null for unhandled multi-char key", () => {
      expect(keyToBytes(makeEvent("Shift"), false)).toBeNull();
    });

    it("returns null for meta+key", () => {
      expect(keyToBytes(makeEvent("c", { metaKey: true }), false)).toBeNull();
    });
  });
});
