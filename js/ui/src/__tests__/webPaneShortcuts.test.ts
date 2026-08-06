import { describe, expect, it, vi } from "vitest";
import { forwardWebPaneWorkspaceShortcut } from "../webPaneShortcuts";

describe("forwardWebPaneWorkspaceShortcut", () => {
  it("relays Ctrl+Alt+Shift+Q and claims the pane before dispatch", () => {
    const source = new KeyboardEvent("keydown", {
      key: "Q",
      code: "KeyQ",
      ctrlKey: true,
      altKey: true,
      shiftKey: true,
      cancelable: true,
    });
    const target = new EventTarget();
    const order: string[] = [];
    const claimFocus = vi.fn(() => order.push("claim"));
    target.addEventListener("keydown", (raw) => {
      const event = raw as KeyboardEvent;
      order.push("dispatch");
      expect(event.ctrlKey).toBe(true);
      expect(event.altKey).toBe(true);
      expect(event.shiftKey).toBe(true);
      expect(event.code).toBe("KeyQ");
      event.preventDefault();
    });

    expect(forwardWebPaneWorkspaceShortcut(source, claimFocus, target)).toBe(
      true,
    );
    expect(order).toEqual(["claim", "dispatch"]);
    expect(claimFocus).toHaveBeenCalledOnce();
    expect(source.defaultPrevented).toBe(true);
  });

  it("accepts KeyQ when Alt changes the key value", () => {
    const event = new KeyboardEvent("keydown", {
      key: "œ",
      code: "KeyQ",
      ctrlKey: true,
      altKey: true,
      shiftKey: true,
    });
    expect(
      forwardWebPaneWorkspaceShortcut(event, () => {}, new EventTarget()),
    ).toBe(true);
  });

  it("relays Ctrl+Shift+Q for recoverable pane removal", () => {
    const source = new KeyboardEvent("keydown", {
      key: "Q",
      code: "KeyQ",
      ctrlKey: true,
      shiftKey: true,
      cancelable: true,
    });
    const target = new EventTarget();
    const claimFocus = vi.fn();
    const listener = vi.fn((raw: Event) => {
      const event = raw as KeyboardEvent;
      expect(event.ctrlKey).toBe(true);
      expect(event.altKey).toBe(false);
      expect(event.shiftKey).toBe(true);
      event.preventDefault();
    });
    target.addEventListener("keydown", listener);

    expect(forwardWebPaneWorkspaceShortcut(source, claimFocus, target)).toBe(
      true,
    );
    expect(claimFocus).toHaveBeenCalledOnce();
    expect(listener).toHaveBeenCalledOnce();
    expect(source.defaultPrevented).toBe(true);
  });

  it("relays Alt+Shift+[ / ] so a page cannot trap focus", () => {
    for (const code of ["BracketLeft", "BracketRight"]) {
      const source = new KeyboardEvent("keydown", {
        // Alt rewrites [ and ] to " and ' on a Mac; only the code is reliable.
        key: code === "BracketLeft" ? "“" : "‘",
        code,
        altKey: true,
        shiftKey: true,
        cancelable: true,
      });
      const target = new EventTarget();
      const claimFocus = vi.fn();
      const listener = vi.fn((raw: Event) => {
        const event = raw as KeyboardEvent;
        expect(event.code).toBe(code);
        expect(event.altKey).toBe(true);
        expect(event.shiftKey).toBe(true);
        expect(event.ctrlKey).toBe(false);
        event.preventDefault();
      });
      target.addEventListener("keydown", listener);

      expect(forwardWebPaneWorkspaceShortcut(source, claimFocus, target)).toBe(
        true,
      );
      expect(claimFocus).toHaveBeenCalledOnce();
      expect(listener).toHaveBeenCalledOnce();
      expect(source.defaultPrevented).toBe(true);
    }
  });

  it("leaves all other iframe keyboard events alone", () => {
    const claimFocus = vi.fn();
    const target = new EventTarget();
    const listener = vi.fn();
    target.addEventListener("keydown", listener);

    for (const init of [
      { key: "Q", code: "KeyQ", ctrlKey: true, altKey: true },
      { key: "P", code: "KeyP", ctrlKey: true, altKey: true, shiftKey: true },
      {
        key: "Q",
        code: "KeyQ",
        ctrlKey: true,
        altKey: true,
        shiftKey: true,
        metaKey: true,
      },
      // Bracket chords the page keeps: Ctrl+[ / ] is BSP's own pane cycle,
      // handled in the workspace document, and a bare [ is just typing.
      { key: "[", code: "BracketLeft" },
      { key: "[", code: "BracketLeft", altKey: true },
      { key: "[", code: "BracketLeft", ctrlKey: true, shiftKey: true },
      {
        key: "[",
        code: "BracketLeft",
        ctrlKey: true,
        altKey: true,
        shiftKey: true,
      },
    ]) {
      expect(
        forwardWebPaneWorkspaceShortcut(
          new KeyboardEvent("keydown", init),
          claimFocus,
          target,
        ),
      ).toBe(false);
    }
    expect(claimFocus).not.toHaveBeenCalled();
    expect(listener).not.toHaveBeenCalled();
  });
});
