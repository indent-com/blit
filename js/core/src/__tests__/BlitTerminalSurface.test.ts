import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { BlitTerminalSurface } from "../BlitTerminalSurface";

function mockCanvasContext(): void {
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
}

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
    mockCanvasContext();
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

  it("copySelection() reads from the wasm terminal for in-viewport selections", async () => {
    const s = newSurface();
    // Stub the wasm terminal so copySelection's in-viewport branch runs
    // synchronously through to navigator.clipboard.writeText.
    const get_text = vi.fn().mockReturnValue("hello");
    // @ts-expect-error — install a fake wasm terminal stub.
    s["terminal"] = { get_text, bracketed_paste: () => false };
    // @ts-expect-error — force a non-empty selection that lands in the
    // viewport (tailOffset 0 maps to the bottom row regardless of _rows).
    s.selStart = { row: 0, col: 0, tailOffset: 0 };
    // @ts-expect-error — touching private state purely to drive the test.
    s.selEnd = { row: 0, col: 5, tailOffset: 0 };
    const result = await s.copySelection();
    expect(result).toBe("hello");
    expect(navigator.clipboard.writeText).toHaveBeenCalledWith("hello");
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

describe("BlitTerminalSurface native scroll surface", () => {
  beforeEach(() => {
    mockCanvasContext();
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn((cb: FrameRequestCallback) => {
        cb(0);
        return 1;
      }),
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function setClientHeight(el: HTMLElement, value: number): void {
    Object.defineProperty(el, "clientHeight", {
      configurable: true,
      value,
    });
  }

  function makeSurface(lines = 100, cellH = 10, clientHeight = 80) {
    const s = new BlitTerminalSurface({ sessionId: null });
    const el = document.createElement("div");
    const spacer = document.createElement("div");
    el.appendChild(spacer);
    setClientHeight(el, clientHeight);

    // @ts-expect-error — install DOM/terminal stubs for private scroll sync.
    s.scrollEl = el;
    // @ts-expect-error — install DOM/terminal stubs for private scroll sync.
    s.scrollSpacer = spacer;
    // @ts-expect-error — install DOM/terminal stubs for private scroll sync.
    s.terminal = { scrollback_lines: () => lines };
    // @ts-expect-error — only cell.h is read by the scroll surface methods.
    s.cell = { h: cellH };

    return { s, el, spacer };
  }

  it("sizes content so native bottom is reachable when offset is zero", () => {
    const { s, el, spacer } = makeSurface(100, 10, 80);
    // @ts-expect-error — touching private scrollOffset for direct sync test.
    s.scrollOffset = 0;

    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);

    expect(spacer.style.height).toBe("1080px");
    expect(el.scrollTop).toBe(1000);
  });

  it("maps native scroll to bottom back to zero offset", () => {
    const { s, el } = makeSurface(100, 10, 80);
    // @ts-expect-error — start scrolled back so the listener must update it.
    s.scrollOffset = 25;

    // @ts-expect-error — install and invoke the private scroll listener.
    s.setupScrollSurface();
    el.scrollTop = 1000;
    // @ts-expect-error — requestAnimationFrame stub already cleared this.
    s.boundScrollListener();

    // @ts-expect-error — assert private scrollback state after native scroll.
    expect(s.scrollOffset).toBe(0);
  });

  it("maps native scroll to top back to full scrollback offset", () => {
    const { s, el } = makeSurface(100, 10, 80);

    // @ts-expect-error — install and invoke the private scroll listener.
    s.setupScrollSurface();
    el.scrollTop = 0;
    // @ts-expect-error — requestAnimationFrame stub already cleared this.
    s.boundScrollListener();

    // @ts-expect-error — assert private scrollback state after native scroll.
    expect(s.scrollOffset).toBe(100);
  });

  it("keeps native scroll at bottom when the viewport height changes", () => {
    const { s, el, spacer } = makeSurface(100, 10, 80);
    // @ts-expect-error — touching private scrollOffset for direct sync test.
    s.scrollOffset = 0;

    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);
    expect(spacer.style.height).toBe("1080px");
    expect(el.scrollTop).toBe(1000);

    setClientHeight(el, 120);
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);

    expect(spacer.style.height).toBe("1120px");
    expect(el.scrollTop).toBe(1000);
  });
});
