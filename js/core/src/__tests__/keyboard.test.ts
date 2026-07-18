import { describe, it, expect } from "vitest";
import { keyToBytes, macEditingKeybind } from "../keyboard";

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

    it("Shift+digit with correct e.key (normal browser) passes through", () => {
      // When the browser correctly reports e.key="@" for Shift+2, we send it.
      const bytes = keyToBytes(
        makeEvent("@", {
          shiftKey: true,
          code: "Digit2",
        } as Partial<KeyboardEvent>),
        false,
      );
      expect(bytes).toEqual(new TextEncoder().encode("@"));
    });

    it("Shift+digit with wrong e.key (Brave bug) returns null", () => {
      // Brave reports e.key="2" even with Shift held — bail out so the
      // textarea input event can produce the correct character.
      const bytes = keyToBytes(
        makeEvent("2", {
          shiftKey: true,
          code: "Digit2",
        } as Partial<KeyboardEvent>),
        false,
      );
      expect(bytes).toBeNull();
    });

    it("unshifted digit key still works", () => {
      const bytes = keyToBytes(
        makeEvent("2", { code: "Digit2" } as Partial<KeyboardEvent>),
        false,
      );
      expect(bytes).toEqual(new TextEncoder().encode("2"));
    });
  });

  describe("simple keys", () => {
    it("Enter sends CR", () => {
      expect(Array.from(keyToBytes(makeEvent("Enter"), false)!)).toEqual([
        0x0d,
      ]);
    });

    it("Alt+Enter sends ESC CR", () => {
      expect(
        Array.from(keyToBytes(makeEvent("Enter", { altKey: true }), false)!),
      ).toEqual([0x1b, 0x0d]);
    });

    it("Backspace sends DEL", () => {
      expect(Array.from(keyToBytes(makeEvent("Backspace"), false)!)).toEqual([
        0x7f,
      ]);
    });

    it("Tab sends HT", () => {
      expect(Array.from(keyToBytes(makeEvent("Tab"), false)!)).toEqual([0x09]);
    });

    it("Shift+Tab sends CBT (ESC [ Z)", () => {
      expect(
        Array.from(keyToBytes(makeEvent("Tab", { shiftKey: true }), false)!),
      ).toEqual([0x1b, 0x5b, 0x5a]);
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

  describe("kitty delegation", () => {
    const enc = new TextEncoder();

    it("flags 0 keeps the legacy body byte-identical", () => {
      // Enter is a CR in legacy mode regardless of the (empty) kitty state.
      expect(keyToBytes(makeEvent("Enter"), false, { flags: 0 })).toEqual(
        enc.encode("\r"),
      );
    });

    it("delegates Shift+Enter to CSI-u when flags are active", () => {
      expect(
        keyToBytes(makeEvent("Enter", { shiftKey: true }), false, { flags: 1 }),
      ).toEqual(enc.encode("\x1b[13;2u"));
    });

    it("Cmd+a: null in legacy, CSI-u once kitty is on", () => {
      expect(keyToBytes(makeEvent("a", { metaKey: true }), false)).toBeNull();
      expect(
        keyToBytes(makeEvent("a", { metaKey: true }), false, { flags: 1 }),
      ).toEqual(enc.encode("\x1b[97;9u"));
    });

    it("masks unsupported bits: flags 24 ≡ flags 0 (legacy)", () => {
      expect(keyToBytes(makeEvent("Enter"), false, { flags: 24 })).toEqual(
        enc.encode("\r"),
      );
    });

    it("masks unsupported bits: flags 9 ≡ flags 1", () => {
      expect(
        keyToBytes(makeEvent("Enter", { shiftKey: true }), false, { flags: 9 }),
      ).toEqual(enc.encode("\x1b[13;2u"));
    });

    it("passes the event type through to the encoder", () => {
      expect(
        keyToBytes(makeEvent("ArrowUp"), false, {
          flags: 3,
          eventType: "release",
        }),
      ).toEqual(enc.encode("\x1b[1;1:3A"));
    });
  });
});

describe("macEditingKeybind", () => {
  const enc = new TextEncoder();

  it("Cmd chords map to line-edge control bytes", () => {
    expect(
      macEditingKeybind(makeEvent("Backspace", { metaKey: true })),
    ).toEqual(
      new Uint8Array([0x15]), // Ctrl+U
    );
    expect(
      macEditingKeybind(makeEvent("ArrowLeft", { metaKey: true })),
    ).toEqual(
      new Uint8Array([0x01]), // Ctrl+A
    );
    expect(
      macEditingKeybind(makeEvent("ArrowRight", { metaKey: true })),
    ).toEqual(new Uint8Array([0x05])); // Ctrl+E
  });

  it("Option chords map to word-wise escape sequences", () => {
    expect(macEditingKeybind(makeEvent("Backspace", { altKey: true }))).toEqual(
      enc.encode("\x1b\x7f"),
    );
    expect(macEditingKeybind(makeEvent("ArrowLeft", { altKey: true }))).toEqual(
      enc.encode("\x1bb"),
    );
    expect(
      macEditingKeybind(makeEvent("ArrowRight", { altKey: true })),
    ).toEqual(enc.encode("\x1bf"));
  });

  it("requires a bare chord — Shift or the opposite modifier disqualifies", () => {
    expect(
      macEditingKeybind(
        makeEvent("ArrowLeft", { metaKey: true, shiftKey: true }),
      ),
    ).toBeNull();
    expect(
      macEditingKeybind(
        makeEvent("Backspace", { metaKey: true, ctrlKey: true }),
      ),
    ).toBeNull();
    expect(
      macEditingKeybind(
        makeEvent("ArrowLeft", { metaKey: true, altKey: true }),
      ),
    ).toBeNull();
  });

  it("returns null for unrelated keys and unmodified presses", () => {
    expect(macEditingKeybind(makeEvent("a", { metaKey: true }))).toBeNull();
    expect(macEditingKeybind(makeEvent("Backspace"))).toBeNull();
    expect(
      macEditingKeybind(makeEvent("ArrowUp", { metaKey: true })),
    ).toBeNull();
  });
});
