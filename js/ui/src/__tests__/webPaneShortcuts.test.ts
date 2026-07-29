import { describe, expect, it, vi } from "vitest";
import { forwardWebPaneCloseShortcut } from "../webPaneShortcuts";

describe("forwardWebPaneCloseShortcut", () => {
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

    expect(forwardWebPaneCloseShortcut(source, claimFocus, target)).toBe(true);
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
      forwardWebPaneCloseShortcut(event, () => {}, new EventTarget()),
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

    expect(forwardWebPaneCloseShortcut(source, claimFocus, target)).toBe(true);
    expect(claimFocus).toHaveBeenCalledOnce();
    expect(listener).toHaveBeenCalledOnce();
    expect(source.defaultPrevented).toBe(true);
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
    ]) {
      expect(
        forwardWebPaneCloseShortcut(
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
