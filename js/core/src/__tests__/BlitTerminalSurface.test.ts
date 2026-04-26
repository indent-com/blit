import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { BlitTerminalSurface } from "../BlitTerminalSurface";

describe("BlitTerminalSurface mobile copy/paste API", () => {
  beforeEach(() => {
    // jsdom doesn't ship a clipboard mock; install one we can spy on.
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      writable: true,
      value: {
        writeText: vi.fn().mockResolvedValue(undefined),
        readText: vi.fn().mockResolvedValue(""),
      },
    });
    // jsdom returns null for getContext("2d") on detached canvases.
    // Stub it with a minimal mock that satisfies measureCell().
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(
      () => {
        return {
          font: "",
          textBaseline: "",
          measureText: () => ({ width: 8 }) as TextMetrics,
          getImageData: () =>
            ({ data: new Uint8ClampedArray(40000) }) as unknown as ImageData,
          fillRect: () => {},
          fillText: () => {},
          clearRect: () => {},
          save: () => {},
          restore: () => {},
          beginPath: () => {},
          rect: () => {},
          clip: () => {},
          fill: () => {},
        } as unknown as CanvasRenderingContext2D;
      },
    );
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function newSurface(): BlitTerminalSurface {
    return new BlitTerminalSurface({ sessionId: null });
  }

  it("starts with no selection", () => {
    const s = newSurface();
    expect(s.hasSelection()).toBe(false);
  });

  it("notifies subscribers when selection is cleared from empty state", () => {
    const s = newSurface();
    const listener = vi.fn();
    s.onSelectionChange(listener);
    s.clearSelection();
    // No mutation occurred — listener should not fire.
    expect(listener).not.toHaveBeenCalled();
  });

  it("supports unsubscribing selection listeners", () => {
    const s = newSurface();
    const listener = vi.fn();
    const unsub = s.onSelectionChange(listener);
    unsub();
    // Force a notification by directly mutating internal state, then
    // clearing — the unsubscribed listener must not fire.
    // @ts-expect-error — touching private state purely to drive the test.
    s.selStart = { row: 0, col: 0, tailOffset: 0 };
    // @ts-expect-error — touching private state purely to drive the test.
    s.selEnd = { row: 0, col: 5, tailOffset: 0 };
    s.clearSelection();
    expect(listener).not.toHaveBeenCalled();
  });

  it("hasSelection() ignores zero-length selections", () => {
    const s = newSurface();
    // @ts-expect-error — touching private state purely to drive the test.
    s.selStart = { row: 0, col: 3, tailOffset: 2 };
    // @ts-expect-error — touching private state purely to drive the test.
    s.selEnd = { row: 0, col: 3, tailOffset: 2 };
    expect(s.hasSelection()).toBe(false);
  });

  it("hasSelection() reports true once start and end differ", () => {
    const s = newSurface();
    // @ts-expect-error — touching private state purely to drive the test.
    s.selStart = { row: 0, col: 0, tailOffset: 0 };
    // @ts-expect-error — touching private state purely to drive the test.
    s.selEnd = { row: 0, col: 4, tailOffset: 0 };
    expect(s.hasSelection()).toBe(true);
  });

  it("clearSelection() resets state and notifies listeners", () => {
    const s = newSurface();
    // @ts-expect-error — touching private state purely to drive the test.
    s.selStart = { row: 0, col: 0, tailOffset: 0 };
    // @ts-expect-error — touching private state purely to drive the test.
    s.selEnd = { row: 0, col: 4, tailOffset: 0 };
    const listener = vi.fn();
    s.onSelectionChange(listener);
    s.clearSelection();
    expect(s.hasSelection()).toBe(false);
    expect(listener).toHaveBeenCalledWith(false);
  });

  it("copySelection() returns null when nothing is selected", async () => {
    const s = newSurface();
    const result = await s.copySelection();
    expect(result).toBeNull();
    expect(navigator.clipboard.writeText).not.toHaveBeenCalled();
  });

  it("pasteFromClipboard() returns null when read-only", async () => {
    const s = new BlitTerminalSurface({ sessionId: "s1", readOnly: true });
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
    expect(navigator.clipboard.readText).not.toHaveBeenCalled();
  });

  it("pasteFromClipboard() returns null when not connected", async () => {
    const s = newSurface();
    // sessionId is null; even if connected, it would short-circuit.
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
  });

  it("pasteText() is a no-op when read-only", () => {
    const s = new BlitTerminalSurface({ sessionId: "s1", readOnly: true });
    const sendInput = vi.fn();
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    s.pasteText("hello");
    expect(sendInput).not.toHaveBeenCalled();
  });
});
