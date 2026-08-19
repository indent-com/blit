import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  BlitTerminalSurface,
  terminalSurfaceForInput,
} from "../BlitTerminalSurface";

function mockCanvasContext(): void {
  // jsdom returns null for getContext("2d") on detached canvases.
  // Stub it with a minimal mock that satisfies measureCell().
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(() => {
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
  });
}

describe("BlitTerminalSurface sizing", () => {
  const observe = vi.fn();
  const disconnect = vi.fn();
  let callbacks: ResizeObserverCallback[] = [];

  /** Deliver a container size to every live observer. */
  function resizeTo(width: number, height: number): void {
    const entry = { contentRect: { width, height } } as ResizeObserverEntry;
    for (const cb of callbacks) cb([entry], null as never);
  }

  beforeEach(() => {
    observe.mockClear();
    disconnect.mockClear();
    callbacks = [];
    mockCanvasContext();
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        constructor(cb: ResizeObserverCallback) {
          callbacks.push(cb);
        }
        observe = observe;
        disconnect = disconnect;
      },
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function attachSurface(
    options: { readOnly?: boolean; resizable?: boolean } = {},
  ) {
    const surface = new BlitTerminalSurface({
      sessionId: null,
      ...options,
    });
    const container = document.createElement("div");
    surface.attach(container);
    const canvas = container.querySelector("canvas");
    if (!(canvas instanceof HTMLCanvasElement)) {
      throw new Error("Expected Blit terminal canvas");
    }
    return { surface, canvas };
  }

  it("resolves the mounted surface from its keyboard textarea", () => {
    const { surface, canvas } = attachSurface();
    const input = canvas.parentElement?.querySelector(
      'textarea[aria-label="Terminal input"]',
    );
    expect(terminalSurfaceForInput(input ?? null)).toBe(surface);
    surface.dispose();
    expect(terminalSurfaceForInput(input ?? null)).toBeNull();
  });

  it("uses the same resizable layout in read-only and writable modes", () => {
    const writable = attachSurface();
    const readOnly = attachSurface({ readOnly: true });

    expect({
      writable: {
        objectFit: writable.canvas.style.objectFit,
        position: writable.canvas.style.position,
        top: writable.canvas.style.top,
        left: writable.canvas.style.left,
      },
      readOnly: {
        objectFit: readOnly.canvas.style.objectFit,
        position: readOnly.canvas.style.position,
        top: readOnly.canvas.style.top,
        left: readOnly.canvas.style.left,
      },
    }).toEqual({
      writable: {
        objectFit: "",
        position: "absolute",
        top: "0px",
        left: "0px",
      },
      readOnly: {
        objectFit: "",
        position: "absolute",
        top: "0px",
        left: "0px",
      },
    });
    expect(observe).toHaveBeenCalledTimes(2);

    writable.surface.dispose();
    readOnly.surface.dispose();
  });

  it("contains passive surfaces without registering their container size", () => {
    const writable = attachSurface({ resizable: false });
    const readOnly = attachSurface({ readOnly: true, resizable: false });

    // A passive surface is *clamped*, not stretched: `width: 100%` scaled the
    // grid to the container instead of letting it keep its own cell size, so
    // the bound is now a maximum.
    expect({
      writable: {
        maxWidth: writable.canvas.style.maxWidth,
        maxHeight: writable.canvas.style.maxHeight,
        objectFit: writable.canvas.style.objectFit,
        objectPosition: writable.canvas.style.objectPosition,
      },
      readOnly: {
        maxWidth: readOnly.canvas.style.maxWidth,
        maxHeight: readOnly.canvas.style.maxHeight,
        objectFit: readOnly.canvas.style.objectFit,
        objectPosition: readOnly.canvas.style.objectPosition,
      },
    }).toEqual({
      writable: {
        maxWidth: "100%",
        maxHeight: "100%",
        objectFit: "contain",
        objectPosition: "center",
      },
      readOnly: {
        maxWidth: "100%",
        maxHeight: "100%",
        objectFit: "contain",
        objectPosition: "center",
      },
    });
    // A passive surface does observe its container — it needs the box to pick
    // how far to box-filter the canvas down before the browser minifies it —
    // but the measurement stays local: no view id is allocated, so nothing it
    // sees can reach the server and drag the session's grid down to a card.
    expect(observe).toHaveBeenCalledTimes(2);
    resizeTo(200, 100);
    expect(writable.surface["_presentBox"]).toEqual({
      width: 200,
      height: 100,
    });
    expect(writable.surface["viewId"]).toBeFalsy();
    expect(readOnly.surface["viewId"]).toBeFalsy();

    writable.surface.dispose();
    readOnly.surface.dispose();
  });

  it("applies local glyph metrics to passive terminals", () => {
    const { surface } = attachSurface({ readOnly: true, resizable: false });
    const terminal = {
      set_cell_size: vi.fn(),
      set_font_family: vi.fn(),
      set_font_size: vi.fn(),
      invalidate_render_cache: vi.fn(),
    };
    surface.terminal = terminal as never;

    // A restored off-screen session can be created before any resizable pane.
    // Its TerminalStore defaults to 1x1 cells, so the passive preview must
    // install its own render metrics even though it never resizes the PTY.
    surface["remeasureCells"](true);

    expect(terminal.set_cell_size).toHaveBeenCalledWith(
      surface["cell"].pw,
      surface["cell"].ph,
    );
    expect(terminal.set_font_family).toHaveBeenCalled();
    expect(terminal.set_font_size).toHaveBeenCalled();
    expect(terminal.invalidate_render_cache).toHaveBeenCalled();
    expect(surface["viewId"]).toBeFalsy();

    surface.dispose();
  });

  it("reconciles canvas layout and terminal dimensions when resizable changes", () => {
    const { surface, canvas } = attachSurface({
      readOnly: true,
      resizable: false,
    });
    // @ts-expect-error — install the terminal dimensions a passive surface follows.
    surface.terminal = {
      rows: 40,
      cols: 120,
      set_cell_size: vi.fn(),
      set_font_family: vi.fn(),
      set_font_size: vi.fn(),
      invalidate_render_cache: vi.fn(),
    };

    // Each mode swaps the observer rather than dropping it: the passive one
    // from attach() is torn down and a sizing one takes its place.
    surface.setResizable(true);
    expect(canvas.style.position).toBe("absolute");
    expect(canvas.style.objectFit).toBe("");
    expect(observe).toHaveBeenCalledTimes(2);
    expect(disconnect).toHaveBeenCalledOnce();

    // And back the other way — a pane demoted to a thumbnail still needs a box
    // to downscale against, so it must not be left unobserved.
    surface.setResizable(false);
    expect(canvas.style.position).toBe("");
    expect(canvas.style.objectFit).toBe("contain");
    expect(surface.rows).toBe(40);
    expect(surface.cols).toBe(120);
    expect(disconnect).toHaveBeenCalledTimes(2);
    expect(observe).toHaveBeenCalledTimes(3);
    surface.dispose();
  });

  it("lets core suppress reconnect resizes for passive surfaces", () => {
    const setViewSize = vi.fn();
    const surface = new BlitTerminalSurface({
      sessionId: "s1",
      readOnly: true,
    });
    // @ts-expect-error — install the minimal connection state used by resendSize.
    surface._blitConn = { setViewSize };
    // @ts-expect-error — install an allocated sizing view.
    surface.viewId = "v1";

    surface.resendSize();
    expect(setViewSize).toHaveBeenCalledOnce();

    surface.setResizable(false);
    surface.resendSize();
    expect(setViewSize).toHaveBeenCalledOnce();
  });

  it("registers an exact 80x24 view during initial sizing", () => {
    const setViewSize = vi.fn();
    const removeView = vi.fn();
    const surface = new BlitTerminalSurface({ sessionId: "s1" });
    const container = document.createElement("div");
    const cell = surface["cell"];
    Object.defineProperties(container, {
      clientWidth: { configurable: true, value: cell.w * 80 + cell.w / 2 },
      clientHeight: { configurable: true, value: cell.h * 24 + cell.h / 2 },
    });
    surface["container"] = container;
    surface["viewId"] = "v1";
    surface["_blitConn"] = {
      setViewSize,
      removeView,
      release: vi.fn(),
    } as never;

    // rows/cols already hold their constructor defaults. Initial setup must
    // still publish the view instead of treating equality as no work.
    surface["handleResize"](true);
    expect(setViewSize).toHaveBeenCalledOnce();
    expect(setViewSize.mock.calls[0]!.slice(0, 4)).toEqual([
      "s1",
      "v1",
      24,
      80,
    ]);
    expect(setViewSize.mock.calls[0]![4]).toBeTypeOf("function");

    surface.dispose();
  });
});

describe("BlitTerminalSurface mobile copy/paste API", () => {
  beforeEach(() => {
    // jsdom doesn't ship a clipboard mock; install one we can spy on.
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      writable: true,
      value: {
        writeText: vi.fn().mockResolvedValue(undefined),
        readText: vi.fn().mockResolvedValue(""),
        read: vi.fn().mockResolvedValue([]),
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

  function newConnectedSurface(): {
    s: BlitTerminalSurface;
    sendInput: ReturnType<typeof vi.fn>;
    sendClipboard: ReturnType<typeof vi.fn>;
  } {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    const sendInput = vi.fn();
    const sendClipboard = vi.fn();
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — connection exposing a connected transport + clipboard.
    s["_blitConn"] = { transport: { status: "connected" }, sendClipboard };
    return { s, sendInput, sendClipboard };
  }

  function imageClipboardItem(bytes: Uint8Array): ClipboardItem {
    return {
      types: ["image/png"],
      getType: () => Promise.resolve(new Blob([bytes], { type: "image/png" })),
    } as unknown as ClipboardItem;
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

  it("invalidates stale Wayland clipboard ownership after copying a drag selection", async () => {
    const s = newSurface();
    const noteBrowserClipboardMayHaveChanged = vi.fn();
    // @ts-expect-error — only clipboard authority is relevant to this test.
    s["_blitConn"] = { noteBrowserClipboardMayHaveChanged };
    // @ts-expect-error — install a fake wasm terminal stub.
    s["terminal"] = { get_text: () => "fresh", bracketed_paste: () => false };
    // @ts-expect-error — force a non-empty in-viewport drag selection.
    s["selStart"] = { row: 0, col: 0, tailOffset: 0 };
    // @ts-expect-error — force a non-empty in-viewport drag selection.
    s["selEnd"] = { row: 0, col: 5, tailOffset: 0 };

    expect(await s.copySelection()).toBe("fresh");
    expect(noteBrowserClipboardMayHaveChanged).toHaveBeenCalledOnce();
  });

  it("keeps Wayland clipboard ownership when a drag-selection copy is rejected", async () => {
    vi.mocked(navigator.clipboard.writeText).mockRejectedValue(
      new Error("NotAllowedError"),
    );
    const s = newSurface();
    const noteBrowserClipboardMayHaveChanged = vi.fn();
    // @ts-expect-error — only clipboard authority is relevant to this test.
    s["_blitConn"] = { noteBrowserClipboardMayHaveChanged };
    // @ts-expect-error — install a fake wasm terminal stub.
    s["terminal"] = { get_text: () => "fresh", bracketed_paste: () => false };
    // @ts-expect-error — force a non-empty in-viewport drag selection.
    s["selStart"] = { row: 0, col: 0, tailOffset: 0 };
    // @ts-expect-error — force a non-empty in-viewport drag selection.
    s["selEnd"] = { row: 0, col: 5, tailOffset: 0 };

    expect(await s.copySelection()).toBe("fresh");
    expect(noteBrowserClipboardMayHaveChanged).not.toHaveBeenCalled();
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

  it("pasteFromClipboard() reads a Wayland-owned selection directly", async () => {
    const { s, sendInput } = newConnectedSurface();
    const readWaylandClipboardText = vi.fn().mockResolvedValue("from surface");
    // @ts-expect-error — add the clipboard authority methods to the fake.
    Object.assign(s["_blitConn"], {
      usesWaylandClipboard: () => true,
      readWaylandClipboardText,
    });

    const result = await s.pasteFromClipboard();

    expect(result).toBe("from surface");
    expect(readWaylandClipboardText).toHaveBeenCalledOnce();
    expect(navigator.clipboard.readText).not.toHaveBeenCalled();
    expect(new TextDecoder().decode(sendInput.mock.calls[0][1])).toBe(
      "from surface",
    );
  });

  it("does not paste stale host text for a non-text Wayland selection", async () => {
    const { s, sendInput } = newConnectedSurface();
    // @ts-expect-error — add the clipboard authority methods to the fake.
    Object.assign(s["_blitConn"], {
      usesWaylandClipboard: () => true,
      readWaylandClipboardText: () => Promise.resolve(null),
    });

    await expect(s.pasteFromClipboard()).resolves.toBeNull();
    expect(navigator.clipboard.readText).not.toHaveBeenCalled();
    expect(sendInput).not.toHaveBeenCalled();
  });

  it("pasteFromClipboard() forwards an image-only clipboard then sends ^V", async () => {
    const { s, sendInput, sendClipboard } = newConnectedSurface();
    const bytes = new Uint8Array([137, 80, 78, 71]); // "\x89PNG"
    vi.mocked(navigator.clipboard.read).mockResolvedValue([
      imageClipboardItem(bytes),
    ]);
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
    expect(sendClipboard).toHaveBeenCalledTimes(1);
    expect(sendClipboard).toHaveBeenCalledWith("image/png", bytes);
    expect(sendInput).toHaveBeenCalledTimes(1);
    expect(sendInput).toHaveBeenCalledWith("s1", new Uint8Array([0x16]));
    // The clipboard must be populated server-side before ^V reaches the app.
    expect(sendClipboard.mock.invocationCallOrder[0]).toBeLessThan(
      sendInput.mock.invocationCallOrder[0],
    );
  });

  it("pasteFromClipboard() tries the image read when readText rejects", async () => {
    const { s, sendClipboard } = newConnectedSurface();
    const bytes = new Uint8Array([1, 2, 3]);
    vi.mocked(navigator.clipboard.readText).mockRejectedValue(
      new Error("No valid data on clipboard."),
    );
    vi.mocked(navigator.clipboard.read).mockResolvedValue([
      imageClipboardItem(bytes),
    ]);
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
    expect(sendClipboard).toHaveBeenCalledWith("image/png", bytes);
  });

  it("pasteFromClipboard() drops a clipboard image over the size cap", async () => {
    const { s, sendInput, sendClipboard } = newConnectedSurface();
    const bytes = new Uint8Array(8 * 1024 * 1024 + 1);
    vi.mocked(navigator.clipboard.read).mockResolvedValue([
      imageClipboardItem(bytes),
    ]);
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
    expect(sendClipboard).not.toHaveBeenCalled();
    expect(sendInput).not.toHaveBeenCalled();
  });

  it("pasteFromClipboard() returns null when clipboard.read is unavailable", async () => {
    const { s, sendInput, sendClipboard } = newConnectedSurface();
    // Browsers without the structured read API (jsdom's default) leave the
    // image path a silent no-op rather than an error.
    Object.defineProperty(navigator.clipboard, "read", {
      configurable: true,
      writable: true,
      value: undefined,
    });
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
    expect(sendClipboard).not.toHaveBeenCalled();
    expect(sendInput).not.toHaveBeenCalled();
  });

  it("pasteFromClipboard() returns null when clipboard.read rejects", async () => {
    const { s, sendInput, sendClipboard } = newConnectedSurface();
    vi.mocked(navigator.clipboard.read).mockRejectedValue(
      new Error("NotAllowedError"),
    );
    const result = await s.pasteFromClipboard();
    expect(result).toBeNull();
    expect(sendClipboard).not.toHaveBeenCalled();
    expect(sendInput).not.toHaveBeenCalled();
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

describe("BlitTerminalSurface Ctrl+Shift+V paste shortcut", () => {
  beforeEach(() => {
    mockCanvasContext();
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      writable: true,
      value: {
        writeText: vi.fn().mockResolvedValue(undefined),
        readText: vi.fn().mockResolvedValue("pasted-text"),
      },
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function attachKeyboard(sendInput: (data: Uint8Array) => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keydown/compositionend/input listeners.
    s["setupKeyboard"]();
    return { s, input };
  }

  function fireKeyDown(input: HTMLTextAreaElement, init: KeyboardEventInit) {
    input.dispatchEvent(new KeyboardEvent("keydown", init));
  }

  it("Ctrl+Shift+V triggers pasteFromClipboard", async () => {
    const sendInput = vi.fn();
    const { input } = attachKeyboard(sendInput);

    fireKeyDown(input, {
      key: "v",
      code: "KeyV",
      ctrlKey: true,
      shiftKey: true,
      altKey: false,
      metaKey: false,
      bubbles: true,
    });

    expect(navigator.clipboard.readText).toHaveBeenCalled();
    // pasteFromClipboard is async; wait for it.
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(sendInput).toHaveBeenCalledTimes(1);
    const payload = sendInput.mock.calls[0][1] as Uint8Array;
    expect(new TextDecoder().decode(payload)).toBe("pasted-text");
  });

  it("Ctrl+V sends the ^V control character (0x16) when no paste follows", async () => {
    const sendInput = vi.fn();
    const { input } = attachKeyboard(sendInput);

    // Ctrl+V now defers ^V so a `paste` event can forward a clipboard image
    // first.  When no paste event materialises (jsdom dispatches none), the
    // fallback timer sends the raw ^V so quoted-insert still works.
    fireKeyDown(input, {
      key: "v",
      code: "KeyV",
      ctrlKey: true,
      shiftKey: false,
      altKey: false,
      metaKey: false,
      bubbles: true,
    });

    expect(navigator.clipboard.readText).not.toHaveBeenCalled();
    expect(sendInput).not.toHaveBeenCalled();

    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(sendInput).toHaveBeenCalledTimes(1);
    const payload = sendInput.mock.calls[0][1] as Uint8Array;
    expect(Array.from(payload)).toEqual([0x16]);
  });
});

describe("BlitTerminalSurface mobile toolbar modifiers", () => {
  beforeEach(() => {
    mockCanvasContext();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function attachKeyboard(sendInput: (data: Uint8Array) => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keydown/compositionend/input listeners.
    s["setupKeyboard"]();
    return { s, input };
  }

  it("applies one-shot Ctrl to an arrow key", () => {
    const sendInput = vi.fn();
    const { s, input } = attachKeyboard(sendInput);
    s.setCtrlModifier(true);

    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "ArrowRight",
        code: "ArrowRight",
        bubbles: true,
        cancelable: true,
      }),
    );

    expect(new TextDecoder().decode(sendInput.mock.calls[0][1])).toBe(
      "\x1b[1;5C",
    );
    expect(s.ctrlModifier).toBe(false);
  });

  it("applies one-shot Alt to an arrow key", () => {
    const sendInput = vi.fn();
    const { s, input } = attachKeyboard(sendInput);
    s.setAltModifier(true);

    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "ArrowLeft",
        code: "ArrowLeft",
        bubbles: true,
        cancelable: true,
      }),
    );

    expect(new TextDecoder().decode(sendInput.mock.calls[0][1])).toBe(
      "\x1b[1;3D",
    );
    expect(s.altModifier).toBe(false);
  });
});

describe("BlitTerminalSurface Ctrl+V image paste", () => {
  beforeEach(() => {
    mockCanvasContext();
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      writable: true,
      value: {
        writeText: vi.fn().mockResolvedValue(undefined),
        readText: vi.fn().mockResolvedValue(""),
      },
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function attach(sendInput: (data: Uint8Array) => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    const sendClipboard = vi.fn();
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — connection exposing a connected transport + clipboard.
    s["_blitConn"] = { transport: { status: "connected" }, sendClipboard };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keydown/input/paste listeners.
    s["setupKeyboard"]();
    return { s, input, sendClipboard };
  }

  function firePaste(input: HTMLTextAreaElement, file: File | null) {
    const item: DataTransferItem = {
      kind: file ? "file" : "string",
      type: file ? file.type : "text/plain",
      getAsFile: () => file,
      getAsString: () => {},
      webkitGetAsEntry: () => null,
    } as unknown as DataTransferItem;
    const clipboardData = {
      items: file ? ([item] as unknown as DataTransferItemList) : null,
      getData: () => "",
    } as unknown as DataTransfer;
    const ev = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(ev, "clipboardData", { value: clipboardData });
    input.dispatchEvent(ev);
    return ev;
  }

  function fireTextPaste(input: HTMLTextAreaElement, text: string) {
    const clipboardData = {
      items: null,
      getData: (type: string) => (type === "text/plain" ? text : ""),
    } as unknown as DataTransfer;
    const ev = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(ev, "clipboardData", { value: clipboardData });
    input.dispatchEvent(ev);
    return ev;
  }

  it("sends bracketed Cmd+V text without waiting for another input", () => {
    const sendInput = vi.fn();
    const { s, input } = attach(sendInput);
    // Fish enables bracketed paste while editing the command line.
    // @ts-expect-error — only the mode queried by pasteText is needed here.
    s["terminal"] = {
      app_cursor: () => false,
      bracketed_paste: () => true,
    };

    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "v",
        code: "KeyV",
        metaKey: true,
        bubbles: true,
      }),
    );
    const ev = fireTextPaste(input, "pasted-text");

    expect(ev.defaultPrevented).toBe(true);
    expect(sendInput).toHaveBeenCalledTimes(1);
    expect(new TextDecoder().decode(sendInput.mock.calls[0][1])).toBe(
      "\x1b[200~pasted-text\x1b[201~",
    );
  });

  it("uses Wayland text instead of stale Cmd+V clipboardData", async () => {
    const sendInput = vi.fn();
    const { s, input } = attach(sendInput);
    const readWaylandClipboardText = vi.fn().mockResolvedValue("from surface");
    // @ts-expect-error — add clipboard authority to the focused connection.
    Object.assign(s["_blitConn"], {
      usesWaylandClipboard: () => true,
      readWaylandClipboardText,
    });

    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "v",
        code: "KeyV",
        metaKey: true,
        bubbles: true,
      }),
    );
    const ev = fireTextPaste(input, "stale host text");
    await Promise.resolve();
    await Promise.resolve();

    expect(ev.defaultPrevented).toBe(true);
    expect(readWaylandClipboardText).toHaveBeenCalledOnce();
    expect(sendInput).toHaveBeenCalledTimes(1);
    expect(new TextDecoder().decode(sendInput.mock.calls[0][1])).toBe(
      "from surface",
    );
  });

  it("forwards a pasted image to the server clipboard then sends ^V", async () => {
    const sendInput = vi.fn();
    const { input, sendClipboard } = attach(sendInput);

    // Arm the Ctrl+V deferral, as a real keydown would.
    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "v",
        code: "KeyV",
        ctrlKey: true,
        bubbles: true,
      }),
    );

    const bytes = new Uint8Array([0x89, 0x50, 0x4e, 0x47]); // PNG magic
    const file = new File([bytes], "clip.png", { type: "image/png" });
    const ev = firePaste(input, file);

    // The textarea paste is consumed so it doesn't also emit an input event.
    expect(ev.defaultPrevented).toBe(true);
    // arrayBuffer() resolves on a microtask; let it settle.
    await Promise.resolve();
    await Promise.resolve();

    expect(sendClipboard).toHaveBeenCalledTimes(1);
    expect(sendClipboard.mock.calls[0][0]).toBe("image/png");
    expect(Array.from(sendClipboard.mock.calls[0][1] as Uint8Array)).toEqual(
      Array.from(bytes),
    );
    // ^V is sent after the image so the app reads a populated clipboard.
    expect(sendInput).toHaveBeenCalledTimes(1);
    expect(Array.from(sendInput.mock.calls[0][1] as Uint8Array)).toEqual([
      0x16,
    ]);
  });

  it("cancels the fallback ^V once the image paste is handled", async () => {
    const sendInput = vi.fn();
    const { input, sendClipboard } = attach(sendInput);

    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "v",
        code: "KeyV",
        ctrlKey: true,
        bubbles: true,
      }),
    );
    const file = new File([new Uint8Array([1, 2, 3])], "clip.png", {
      type: "image/png",
    });
    firePaste(input, file);

    await Promise.resolve();
    await Promise.resolve();
    // Let the (now-cancelled) fallback timer window elapse.
    await new Promise((resolve) => setTimeout(resolve, 0));

    // Exactly one ^V — the fallback timer must not double-send.
    expect(sendClipboard).toHaveBeenCalledTimes(1);
    expect(sendInput).toHaveBeenCalledTimes(1);
  });
});

describe("BlitTerminalSurface desktop composition", () => {
  beforeEach(() => {
    mockCanvasContext();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  function attach(sendInput: (data: Uint8Array) => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keyboard and composition listeners.
    s["setupKeyboard"]();
    return input;
  }

  it("commits a dead key when key and input events misreport isComposing", () => {
    // Safari/WebKit can report false on the completing keydown and input.
    // The composition lifecycle must keep those events away from the normal
    // key/input path or preventDefault cancels the Option+E, E commit.
    const sendInput = vi.fn();
    const input = attach(sendInput);

    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Alt",
        code: "AltLeft",
        altKey: true,
        cancelable: true,
      }),
    );
    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Dead",
        code: "KeyE",
        altKey: true,
        cancelable: true,
      }),
    );
    input.dispatchEvent(new CompositionEvent("compositionstart"));
    input.value = "´";
    input.dispatchEvent(
      new InputEvent("input", {
        data: "´",
        inputType: "insertCompositionText",
        isComposing: false,
      }),
    );
    input.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "e",
        code: "KeyE",
        cancelable: true,
        isComposing: false,
      }),
    );
    input.dispatchEvent(new CompositionEvent("compositionend", { data: "é" }));
    input.value = "é";
    input.dispatchEvent(
      new InputEvent("input", {
        data: "é",
        inputType: "insertCompositionText",
        isComposing: false,
      }),
    );

    expect(
      sendInput.mock.calls.map((call) =>
        new TextDecoder().decode(call[1] as Uint8Array),
      ),
    ).toEqual(["é"]);
  });
});

describe("BlitTerminalSurface Android composition", () => {
  beforeEach(() => {
    mockCanvasContext();
    vi.stubGlobal("navigator", {
      userAgent:
        "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36",
      platform: "Linux armv8l",
      maxTouchPoints: 1,
      clipboard: navigator.clipboard,
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function attachAndroid(sendInput: (data: Uint8Array) => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keydown/compositionend/input listeners.
    s["setupKeyboard"]();
    return { s, input };
  }

  function fireCompositionInput(
    input: HTMLTextAreaElement,
    value: string,
    inputType: string,
  ) {
    input.value = value;
    const ev = new Event("input") as InputEvent;
    Object.defineProperty(ev, "inputType", { value: inputType });
    Object.defineProperty(ev, "isComposing", { value: true });
    input.dispatchEvent(ev);
  }

  it("streams insertCompositionText updates letter-by-letter", () => {
    const sendInput = vi.fn();
    const { input } = attachAndroid(sendInput);

    input.dispatchEvent(new Event("compositionstart"));
    fireCompositionInput(input, "h", "insertCompositionText");
    fireCompositionInput(input, "he", "insertCompositionText");
    fireCompositionInput(input, "hel", "insertCompositionText");
    fireCompositionInput(input, "hell", "insertCompositionText");
    fireCompositionInput(input, "hello", "insertCompositionText");
    input.dispatchEvent(
      new CompositionEvent("compositionend", { data: "hello" }),
    );

    const calls = sendInput.mock.calls.map((c) =>
      new TextDecoder().decode(c[1] as Uint8Array),
    );
    expect(calls).toEqual(["h", "e", "l", "l", "o"]);
  });

  it("sends backspaces when the composition shrinks", () => {
    const sendInput = vi.fn();
    const { input } = attachAndroid(sendInput);

    input.dispatchEvent(new Event("compositionstart"));
    fireCompositionInput(input, "h", "insertCompositionText");
    fireCompositionInput(input, "he", "insertCompositionText");
    fireCompositionInput(input, "hel", "insertCompositionText");
    fireCompositionInput(input, "helo", "insertCompositionText");
    fireCompositionInput(input, "hel", "insertCompositionText");
    input.dispatchEvent(
      new CompositionEvent("compositionend", { data: "hel" }),
    );

    const calls = sendInput.mock.calls.map((c) =>
      Array.from(c[1] as Uint8Array),
    );
    expect(calls).toEqual([[0x68], [0x65], [0x6c], [0x6f], [0x7f]]);
  });

  it("replaces the composition on autocorrect", () => {
    const sendInput = vi.fn();
    const { input } = attachAndroid(sendInput);

    input.dispatchEvent(new Event("compositionstart"));
    fireCompositionInput(input, "t", "insertCompositionText");
    fireCompositionInput(input, "te", "insertCompositionText");
    fireCompositionInput(input, "teh", "insertCompositionText");
    fireCompositionInput(input, "the", "insertCompositionText");
    input.dispatchEvent(
      new CompositionEvent("compositionend", { data: "the" }),
    );

    const calls = sendInput.mock.calls.map((c) =>
      Array.from(c[1] as Uint8Array),
    );
    // "teh" typed letter-by-letter, then replaced by "the" in one shot.
    expect(calls).toEqual([
      [0x74],
      [0x65],
      [0x68],
      [0x7f],
      [0x7f],
      [0x7f],
      [0x74, 0x68, 0x65],
    ]);
  });

  it("applies the toolbar's one-shot Ctrl to a composed letter", () => {
    // Android keyboards keep the word in an active composition, so a letter
    // typed after tapping Ctrl arrives as composition input and reaches
    // neither the keydown nor the plain `input` branch that applies the
    // modifier: Ctrl+C used to come out as a literal "c".
    const sendInput = vi.fn();
    const { s, input } = attachAndroid(sendInput);

    input.dispatchEvent(new Event("compositionstart"));
    s.setCtrlModifier(true);
    fireCompositionInput(input, "c", "insertCompositionText");

    expect(
      sendInput.mock.calls.map((c) => Array.from(c[1] as Uint8Array)),
    ).toEqual([[0x03]]);
    // One-shot: it does not stick to the next letter.
    expect(s.ctrlModifier).toBe(false);
    // And the composition is abandoned, so the next letter is not read as a
    // delta against a buffer the shell never received.
    expect(input.value).toBe("");
    fireCompositionInput(input, "d", "insertCompositionText");
    expect(
      sendInput.mock.calls.map((c) => Array.from(c[1] as Uint8Array)),
    ).toEqual([[0x03], [0x64]]);
  });

  it("applies the toolbar's one-shot Alt to a composed letter", () => {
    const sendInput = vi.fn();
    const { s, input } = attachAndroid(sendInput);

    input.dispatchEvent(new Event("compositionstart"));
    s.setAltModifier(true);
    fireCompositionInput(input, "b", "insertCompositionText");

    expect(
      sendInput.mock.calls.map((c) => Array.from(c[1] as Uint8Array)),
    ).toEqual([[0x1b, 0x62]]);
    expect(s.altModifier).toBe(false);
  });

  it("applies Ctrl mid-word, to the letter that follows it", () => {
    // The realistic sequence: type part of a command, then Ctrl+C to kill it.
    const sendInput = vi.fn();
    const { s, input } = attachAndroid(sendInput);

    input.dispatchEvent(new Event("compositionstart"));
    fireCompositionInput(input, "s", "insertCompositionText");
    fireCompositionInput(input, "sl", "insertCompositionText");
    s.setCtrlModifier(true);
    fireCompositionInput(input, "slc", "insertCompositionText");

    expect(
      sendInput.mock.calls.map((c) => Array.from(c[1] as Uint8Array)),
    ).toEqual([[0x73], [0x6c], [0x03]]);
  });
});

describe("BlitTerminalSurface iPad autocorrect", () => {
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

  function attachConnected(sendInput: () => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    // Wire just the input path — bypass attach() so we don't have to stub the
    // full renderer/dirty-listener connection surface.  The input handler only
    // needs sendInput (via _workspace) and a connected transport status.
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keydown/compositionend/input listeners.
    s["setupKeyboard"]();
    return { s, input };
  }

  function fireInput(
    input: HTMLTextAreaElement,
    value: string,
    inputType: string,
  ) {
    input.value = value;
    // jsdom's InputEvent doesn't surface inputType from the init dict, so set
    // it explicitly to mirror what Safari/iPadOS deliver.
    const ev = new Event("input") as InputEvent;
    Object.defineProperty(ev, "inputType", { value: inputType });
    Object.defineProperty(ev, "isComposing", { value: false });
    input.dispatchEvent(ev);
  }

  it("forwards normally typed characters to the session", () => {
    const sendInput = vi.fn();
    const { input } = attachConnected(sendInput);
    fireInput(input, "a", "insertText");
    expect(sendInput).toHaveBeenCalledTimes(1);
    expect(input.value).toBe("");
  });

  it("drops iPad autocorrect (insertReplacementText) substitutions", () => {
    const sendInput = vi.fn();
    const { input } = attachConnected(sendInput);
    // iPadOS ignores autocorrect="off" and delivers the correction as an
    // insertReplacementText input event; it must never reach the shell.
    fireInput(input, "corrected", "insertReplacementText");
    expect(sendInput).not.toHaveBeenCalled();
    expect(input.value).toBe("");
  });
});

describe("BlitTerminalSurface iOS backspace repeat", () => {
  const NBSP = String.fromCharCode(0xa0);

  beforeEach(() => {
    mockCanvasContext();
    vi.stubGlobal("navigator", {
      userAgent:
        "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1",
      platform: "iPhone",
      maxTouchPoints: 5,
      clipboard: navigator.clipboard,
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function attachIOS(sendInput: (data: Uint8Array) => void) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    // @ts-expect-error — wire the keydown/compositionend/input listeners.
    s["setupKeyboard"]();
    return { s, input };
  }

  function fireInput(
    input: HTMLTextAreaElement,
    value: string,
    inputType: string,
  ) {
    input.value = value;
    const ev = new Event("input") as InputEvent;
    Object.defineProperty(ev, "inputType", { value: inputType });
    Object.defineProperty(ev, "isComposing", { value: false });
    input.dispatchEvent(ev);
  }

  it("seeds the capture textarea with non-empty filler", () => {
    const { input } = attachIOS(vi.fn());
    expect(input.value.length).toBeGreaterThan(0);
    expect(input.value).toBe(NBSP.repeat(input.value.length));
  });

  it("forwards a DEL for each deleteContentBackward while the buffer holds", () => {
    const sendInput = vi.fn();
    const { input } = attachIOS(sendInput);
    const seeded = input.value.length;

    // iOS deletes one filler char per key-repeat; each fires its own event.
    for (let i = 1; i <= 3; i++) {
      fireInput(input, NBSP.repeat(seeded - i), "deleteContentBackward");
    }

    const calls = sendInput.mock.calls.map((c) =>
      Array.from(c[1] as Uint8Array),
    );
    expect(calls).toEqual([[0x7f], [0x7f], [0x7f]]);
    // Buffer is left in place (not emptied) so iOS keeps auto-repeating.
    expect(input.value.length).toBeGreaterThan(0);
  });

  it("re-seeds the buffer before it runs dry mid-hold", () => {
    const sendInput = vi.fn();
    const { input } = attachIOS(sendInput);

    // Simulate the buffer nearly exhausted; the handler tops it back up.
    fireInput(input, NBSP.repeat(2), "deleteContentBackward");
    expect(Array.from(sendInput.mock.calls.at(-1)![1] as Uint8Array)).toEqual([
      0x7f,
    ]);
    expect(input.value.length).toBeGreaterThan(4);
  });

  it("forwards only the typed character, not the filler", () => {
    const sendInput = vi.fn();
    const { input } = attachIOS(sendInput);
    const seeded = input.value;

    fireInput(input, seeded + "a", "insertText");

    expect(sendInput).toHaveBeenCalledTimes(1);
    expect(
      new TextDecoder().decode(sendInput.mock.calls[0][1] as Uint8Array),
    ).toBe("a");
    // Field is re-seeded, not emptied.
    expect(input.value.length).toBeGreaterThan(0);
    expect(input.value).toBe(NBSP.repeat(input.value.length));
  });
});

describe("BlitTerminalSurface DPR detection", () => {
  beforeEach(() => {
    mockCanvasContext();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function stubNavigator(platform: string, maxTouchPoints: number): void {
    vi.stubGlobal("navigator", {
      userAgent:
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15",
      platform,
      maxTouchPoints,
      clipboard: navigator.clipboard,
    });
  }

  function stubWindowDpr(devicePixelRatio: number): void {
    Object.defineProperty(window, "devicePixelRatio", {
      configurable: true,
      value: devicePixelRatio,
    });
    Object.defineProperty(window, "outerWidth", {
      configurable: true,
      value: 2048,
    });
    Object.defineProperty(window, "innerWidth", {
      configurable: true,
      value: 1024,
    });
  }

  it("keeps desktop Safari zoom compensation", () => {
    stubNavigator("MacIntel", 0);
    stubWindowDpr(2);

    const s = new BlitTerminalSurface({ sessionId: null, fontSize: 10 });

    // @ts-expect-error — assert private raster metrics produced by DPR helper.
    expect(s.cell.ph).toBe(48);
  });

  it("does not double-count iPadOS Safari viewport scaling", () => {
    stubNavigator("MacIntel", 5);
    stubWindowDpr(2);

    const s = new BlitTerminalSurface({ sessionId: null, fontSize: 10 });

    // iPadOS reports a desktop-like Safari UA, but outerWidth / innerWidth is
    // not desktop page zoom.  Use raw devicePixelRatio so text rasters are not
    // inflated from 2x to 4x.
    // @ts-expect-error — assert private raster metrics produced by DPR helper.
    expect(s.cell.ph).toBe(24);
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

  it("leaves a gesture's scroll position where the user put it", () => {
    // Writing scrollTop back mid-flick cancels the browser's momentum
    // animation, and the only disagreement is where inside a row the
    // gesture currently is — the offset derived from it is already right.
    const { s, el } = makeSurface(100, 10, 80);
    // @ts-expect-error — install and invoke the private scroll listener.
    s.setupScrollSurface();
    el.scrollTop = 746; // mid-row, as a pixel-precise device leaves it
    // @ts-expect-error — the listener is the "user scrolled" signal.
    s.boundScrollListener();

    // @ts-expect-error — assert the offset the listener derived.
    expect(s.scrollOffset).toBe(25);
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);
    expect(el.scrollTop).toBe(746);
  });

  it("still re-aligns a jump that came from somewhere else", () => {
    const { s, el } = makeSurface(100, 10, 80);
    // @ts-expect-error — install and invoke the private scroll listener.
    s.setupScrollSurface();
    el.scrollTop = 746;
    // @ts-expect-error — the listener is the "user scrolled" signal.
    s.boundScrollListener();

    // Shift+Home while the flick is still warm: rows, not a fraction of one.
    // @ts-expect-error — what the scrollback-navigation keys do.
    s.scrollOffset = 100;
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);
    expect(el.scrollTop).toBe(0);
  });

  it("adopts the offset the server re-anchored a scrolled view to", () => {
    const { s, el } = makeSurface(100, 10, 80);
    let anchor: ((offset: number) => void) | null = null;
    // @ts-expect-error — minimal connection exposing only the anchor hook.
    s["_blitConn"] = {
      addScrollAnchorListener: (_id: string, cb: (offset: number) => void) => {
        anchor = cb;
        return () => {};
      },
    };
    // @ts-expect-error — the listener is per-session.
    s["_sessionId"] = "s1";
    // @ts-expect-error — wire the private listener.
    s["setupScrollAnchorListener"]();
    // @ts-expect-error — parked 25 rows above the live bottom.
    s.scrollOffset = 25;
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);
    expect(el.scrollTop).toBe(750);

    // Three lines printed: the server moves us three rows deeper so the
    // same text stays on screen, and the scrollback grows to match.
    anchor!(28);
    // @ts-expect-error — swap in the deepened scrollback the frame carries.
    s.terminal = { scrollback_lines: () => 103 };
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);

    // @ts-expect-error — assert private scrollback state.
    expect(s.scrollOffset).toBe(28);
    expect(el.scrollTop).toBe(750);
  });

  it("defers re-anchor compensation while a gesture is in flight", () => {
    // Scrollback capped: the app prints three lines but the depth no longer
    // grows, so the anchor moves targetTop by whole rows. Writing that back
    // mid-flick cancels the browser's momentum animation — the jumps.
    const { s, el } = makeSurface(100, 10, 80);
    // @ts-expect-error — install and invoke the private scroll listener.
    s.setupScrollSurface();
    el.scrollTop = 750;
    // @ts-expect-error — the listener is the "user scrolled" signal.
    s.boundScrollListener();
    // @ts-expect-error — assert the offset the listener derived.
    expect(s.scrollOffset).toBe(25);

    // @ts-expect-error — what the anchor listener does with a capped depth.
    s.scrollOffset = 28;
    // @ts-expect-error — three rows of re-anchor pending at the next sync.
    s.anchorRowsSinceSync = 3;
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);

    expect(el.scrollTop).toBe(750);
    // @ts-expect-error — the compensation was folded into the bookkeeping.
    expect(s.lastScrollTop).toBe(720);
  });

  it("writes re-anchor compensation once the gesture has settled", () => {
    const { s, el } = makeSurface(100, 10, 80);
    // @ts-expect-error — install and invoke the private scroll listener.
    s.setupScrollSurface();
    el.scrollTop = 750;
    // @ts-expect-error — the listener is the "user scrolled" signal.
    s.boundScrollListener();
    // @ts-expect-error — the flick ended long ago; the view is just parked.
    s.lastUserScrollAt = 0;

    // @ts-expect-error — what the anchor listener does with a capped depth.
    s.scrollOffset = 28;
    // @ts-expect-error — three rows of re-anchor pending at the next sync.
    s.anchorRowsSinceSync = 3;
    // @ts-expect-error — exercising private DOM sync directly.
    s.syncScrollSurface(true);

    // No gesture to protect: the parked view tracks its content exactly.
    expect(el.scrollTop).toBe(720);
  });

  it("carries a selection along when the view is re-anchored", () => {
    // Copying out of the scrollback while the app is still printing: the
    // highlight has to stay on its words, not crawl off them.
    const { s } = makeSurface(100, 10, 80);
    let anchor: ((offset: number) => void) | null = null;
    // @ts-expect-error — minimal connection exposing only the anchor hook.
    s["_blitConn"] = {
      addScrollAnchorListener: (_id: string, cb: (offset: number) => void) => {
        anchor = cb;
        return () => {};
      },
    };
    // @ts-expect-error — the listener is per-session.
    s["_sessionId"] = "s1";
    // @ts-expect-error — wire the private listener.
    s["setupScrollAnchorListener"]();
    // @ts-expect-error — parked, with two rows selected.
    s.scrollOffset = 25;
    // @ts-expect-error — a selection anchored to the live bottom.
    s.selStart = { row: 2, col: 0, tailOffset: 30 };
    // @ts-expect-error — a selection anchored to the live bottom.
    s.selEnd = { row: 3, col: 9, tailOffset: 29 };

    anchor!(28);

    // @ts-expect-error — assert private selection state.
    expect([s.selStart.tailOffset, s.selEnd.tailOffset]).toEqual([33, 32]);
  });
});

describe("BlitTerminalSurface scrollback against a server that answers", () => {
  // One gesture is many scroll events — a wheel notch Chromium animates over
  // several frames, a momentum flick on an iPad, dozens.  Each is reported as
  // a relative move, and each report the server answers comes back absolute
  // and a round trip late.  Adopting a late answer drags the view back to
  // where the gesture used to be, and the next delta — measured from there —
  // comes out too big, so the view lurches past where the finger asked.
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

  const LINES = 1000;
  const CELL_H = 10;
  /** jsdom reports no scrollHeight, so the code falls back to the model. */
  const MAX_TOP = LINES * CELL_H;

  /**
   * A surface wired to a server that holds its own offset, applies each
   * relative move to it, and answers `answerEverything` moves later.
   */
  function rig(lagFrames: number, answerEverything: boolean) {
    const s = new BlitTerminalSurface({ sessionId: null });
    const el = document.createElement("div");
    const spacer = document.createElement("div");
    el.appendChild(spacer);
    Object.defineProperty(el, "clientHeight", {
      configurable: true,
      value: 80,
    });

    // @ts-expect-error — install DOM/terminal stubs for the private sync.
    s.scrollEl = el;
    // @ts-expect-error — install DOM/terminal stubs for the private sync.
    s.scrollSpacer = spacer;
    // @ts-expect-error — only scrollback_lines is read here.
    s.terminal = { scrollback_lines: () => LINES };
    // @ts-expect-error — only cell.h is read by the scroll surface methods.
    s.cell = { h: CELL_H };

    let anchor: ((offset: number) => void) | null = null;
    // @ts-expect-error — minimal connection: status gate plus the anchor hook.
    s["_blitConn"] = {
      transport: { status: "connected" },
      addScrollAnchorListener: (_id: string, cb: (o: number) => void) => {
        anchor = cb;
        return () => {};
      },
    };
    // @ts-expect-error — the listener is per-session.
    s["_sessionId"] = "s1";

    let serverOffset = 0;
    let frame = 0;
    const inFlight: { at: number; offset: number }[] = [];
    const sent: number[] = [];

    // @ts-expect-error — minimal workspace: only the scroll verbs are used.
    s["_workspace"] = {
      scrollSessionBy: (_id: string, _abs: number, lines: number) => {
        sent.push(lines);
        const requested = serverOffset + lines;
        serverOffset = Math.max(0, Math.min(LINES, requested));
        if (answerEverything || requested !== serverOffset) {
          inFlight.push({ at: frame + lagFrames, offset: serverOffset });
        }
      },
      scrollSession: () => {},
    };

    // @ts-expect-error — wire the private listeners.
    s["setupScrollAnchorListener"]();
    // @ts-expect-error — wire the private listeners.
    s["setupScrollSurface"]();

    /** One frame: the browser moves scrollTop, then any answer that has
     *  finished its round trip lands. */
    const step = (scrollTop: number) => {
      el.scrollTop = scrollTop;
      // @ts-expect-error — the rAF stub already cleared the listener handle.
      s.boundScrollListener();
      frame++;
      while (inFlight.length && inFlight[0].at <= frame) {
        anchor!(inFlight.shift()!.offset);
      }
    };

    /** Drain the wire once the gesture has stopped. */
    const settle = () => {
      while (inFlight.length) {
        frame++;
        anchor!(inFlight.shift()!.offset);
      }
    };

    return { step, settle, sent, server: () => serverOffset };
  }

  /** Twelve rows of travel, two rows a frame, the way one notch arrives. */
  const oneNotch = (step: (top: number) => void) => {
    for (let i = 1; i <= 6; i++) step(MAX_TOP - i * 20);
  };

  it("lands a notch where it pointed when nothing answers back", () => {
    const { step, settle, sent, server } = rig(2, false);
    oneNotch(step);
    settle();
    expect(sent).toEqual([2, 2, 2, 2, 2, 2]);
    expect(server()).toBe(12);
  });

  it("would overshoot a notch if every move were answered", () => {
    // The behaviour this exists to prevent, kept as the thing being ruled
    // out: the doubled deltas are the answers landing mid-gesture.
    const { step, settle, sent, server } = rig(2, true);
    oneNotch(step);
    settle();
    expect(sent).toEqual([2, 2, 4, 4, 2]);
    expect(server()).toBe(14);
  });

  it("lands a flick where it pointed, however long the wire is", () => {
    for (const lag of [1, 2, 5]) {
      const { step, settle, server } = rig(lag, false);
      for (let i = 1; i <= 18; i++) step(MAX_TOP - i * 20);
      settle();
      expect({ lag, offset: server() }).toEqual({ lag, offset: 36 });
    }
  });
});

describe("BlitTerminalSurface wheel over the scrollback", () => {
  let now = 0;
  beforeEach(() => {
    now = 1000;
    vi.spyOn(performance, "now").mockImplementation(() => now);
    mockCanvasContext();
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn((cb: FrameRequestCallback) => {
        cb(0);
        return 1;
      }),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        disconnect() {}
      },
    );
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  const LINES = 1000;
  const CELL_H = 19; // 120px is 6.3 of these — the awkward case

  /** A surface at a plain prompt, where the wheel navigates scrollback. */
  function attachScrollback() {
    const surface = new BlitTerminalSurface({ sessionId: "s1" });
    surface.attach(document.createElement("div"));
    // @ts-expect-error — minimal connection exposing a connected transport.
    surface["_blitConn"] = { transport: { status: "connected" } };
    // @ts-expect-error — no app is reading the mouse.
    surface["terminal"] = {
      mouse_mode: () => 0,
      scrollback_lines: () => LINES,
    };
    // @ts-expect-error — a known cell for the row maths.
    surface["cell"] = { h: CELL_H, w: 8, pw: 8, ph: CELL_H };
    const el = surface["scrollEl"];
    if (!el) throw new Error("expected a scroll surface");
    el.scrollTop = LINES * CELL_H; // parked at the live bottom

    const notch = (deltaY: number, deltaMode = 0) => {
      const e = new WheelEvent("wheel", { cancelable: true });
      Object.defineProperties(e, {
        deltaY: { value: deltaY },
        deltaX: { value: 0 },
        deltaMode: { value: deltaMode },
      });
      el.dispatchEvent(e);
      // jsdom does not fire `scroll` for a programmatic scrollTop.
      // @ts-expect-error — the listener the real event would have run.
      surface["boundScrollListener"]?.();
      return e;
    };
    /** The wheel rests, and the next render syncs — a cursor blink will do. */
    const settle = () => {
      now += 200;
      const before = el.scrollTop;
      // @ts-expect-error — the render loop's idempotent sync.
      surface["syncScrollSurface"](true);
      return el.scrollTop - before;
    };
    // @ts-expect-error — read the offset the listener derived.
    const offset = () => surface["scrollOffset"] as number;
    return { surface, el, notch, settle, offset };
  }

  it("moves every notch the same whole number of rows", () => {
    const { notch, settle, offset } = attachScrollback();
    const steps: number[] = [];
    let prev = 0;
    for (let n = 0; n < 12; n++) {
      notch(-120);
      steps.push(offset() - prev);
      prev = offset();
      settle();
    }
    // 120px over a 19px cell: six rows, twelve times, not 6/7 alternating.
    expect(steps).toEqual(Array(12).fill(6));
  });

  it("leaves the surface nothing to snap back once the notch settles", () => {
    // The jank: the sync used to write the rounding back up to half a row
    // later, in whichever direction the remainder fell, as late as the next
    // cursor blink — +6, -7, -1, +5, -8 … px at this cell height.
    const { notch, settle, el } = attachScrollback();
    const start = el.scrollTop;
    const snaps: number[] = [];
    for (let n = 0; n < 12; n++) {
      notch(-120);
      snaps.push(settle());
    }
    expect(snaps).toEqual(Array(12).fill(0));
    // jsdom does no scrolling of its own, so assert the notches actually
    // moved the surface — otherwise "nothing snapped back" is vacuous.
    expect(el.scrollTop).toBe(start - 12 * 6 * CELL_H);
  });

  it("still squares up a surface a pixel-precise device left mid-row", () => {
    // The backstop the trackpad still depends on: an offset is whole rows,
    // so a position between them has to be written back eventually — just
    // not once per notch.
    const { el, settle, surface } = attachScrollback();
    el.scrollTop = LINES * CELL_H - 100; // 5.26 rows: not on the grid
    // @ts-expect-error — the listener the real scroll event would have run.
    surface["boundScrollListener"]();
    expect(settle()).toBe(5); // 5.26 rows rounds to 5, i.e. back up to 95px
  });

  it("claims the notch so the browser does not scroll it as well", () => {
    const { notch } = attachScrollback();
    expect(notch(-120).defaultPrevented).toBe(true);
  });

  it("leaves a trackpad to the browser's own scrolling", () => {
    const { notch, el } = attachScrollback();
    const before = el.scrollTop;
    const e = notch(-53.5);
    expect(e.defaultPrevented).toBe(false);
    expect(el.scrollTop).toBe(before);
  });

  it("still lets ctrl+wheel through as a zoom", () => {
    const { surface, el } = attachScrollback();
    const before = el.scrollTop;
    const e = new WheelEvent("wheel", { cancelable: true, ctrlKey: true });
    Object.defineProperties(e, {
      deltaY: { value: -120 },
      deltaMode: { value: 0 },
    });
    el.dispatchEvent(e);
    expect(e.defaultPrevented).toBe(false);
    expect(el.scrollTop).toBe(before);
    expect(surface).toBeTruthy();
  });
});

describe("BlitTerminalSurface wheel in mouse-reporting apps", () => {
  beforeEach(() => {
    mockCanvasContext();
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        disconnect() {}
      },
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  /** A surface whose app has asked for mouse reports (vim, htop, …). */
  function attachMouseMode() {
    const sendMouse = vi.fn();
    const surface = new BlitTerminalSurface({ sessionId: "s1" });
    const container = document.createElement("div");
    surface.attach(container);
    // @ts-expect-error — install a fake workspace stub.
    surface["_workspace"] = { sendMouse };
    // @ts-expect-error — minimal connection exposing a connected transport.
    surface["_blitConn"] = { transport: { status: "connected" } };
    // @ts-expect-error — the app reads the mouse (mode 3 = button tracking).
    surface["terminal"] = { mouse_mode: () => 3 };
    // @ts-expect-error — a known line height for the detent maths.
    surface["cell"] = { h: 18, w: 8, pw: 8, ph: 18 };
    const scrollEl = surface["scrollEl"];
    if (!scrollEl) throw new Error("expected a scroll surface");
    const wheel = (init: Partial<WheelEvent>) => {
      const e = new WheelEvent("wheel", { cancelable: true });
      Object.defineProperties(e, {
        deltaY: { value: init.deltaY ?? 0 },
        deltaX: { value: init.deltaX ?? 0 },
        deltaMode: { value: init.deltaMode ?? 0 },
      });
      scrollEl.dispatchEvent(e);
      return e;
    };
    return { surface, sendMouse, wheel };
  }

  it("reports one wheel button per notch, not per event", () => {
    const { sendMouse, wheel } = attachMouseMode();
    wheel({ deltaY: 120 });
    expect(sendMouse).toHaveBeenCalledTimes(1);
    expect(sendMouse.mock.calls[0][2]).toBe(65); // wheel down
  });

  it("does not report a step for every trackpad sliver", () => {
    // One 6px sliver is a third of a row; twelve of them are four rows,
    // which is one conventional three-line step and change.
    const { sendMouse, wheel } = attachMouseMode();
    for (let i = 0; i < 12; i++) wheel({ deltaY: 6 });
    expect(sendMouse).toHaveBeenCalledTimes(1);
  });

  it("does not scroll the app on a sideways swipe", () => {
    // deltaY of 0 used to fall through to "wheel down" and scroll the app.
    const { sendMouse, wheel } = attachMouseMode();
    wheel({ deltaX: 240, deltaY: 0 });
    expect(sendMouse).not.toHaveBeenCalled();
  });

  it("leaves ctrl+wheel to the browser as a zoom", () => {
    const { surface, sendMouse } = attachMouseMode();
    const scrollEl = surface["scrollEl"]!;
    const e = new WheelEvent("wheel", { cancelable: true, ctrlKey: true });
    Object.defineProperty(e, "deltaY", { value: 120 });
    scrollEl.dispatchEvent(e);
    expect(sendMouse).not.toHaveBeenCalled();
    expect(e.defaultPrevented).toBe(false);
  });
});

describe("BlitTerminalSurface cursor", () => {
  beforeEach(() => {
    mockCanvasContext();
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        disconnect() {}
      },
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  /** The writable surface's scroll overlay — the element that owns the
   *  cursor. Read-only surfaces have none. */
  function attachWritable() {
    const surface = new BlitTerminalSurface({ sessionId: null });
    const container = document.createElement("div");
    surface.attach(container);
    const scrollEl = surface["scrollEl"];
    if (!scrollEl) throw new Error("expected a scroll surface");
    return { surface, container, scrollEl };
  }

  it("writes the cursor through to the element", () => {
    // Regression: this used to call itself instead of assigning the style,
    // so it terminated on the dedup guard and silently wrote nothing — the
    // terminal never showed a pointer over a link or grabbing on the
    // scrollbar, it just kept the inline I-beam forever.
    const { surface, scrollEl } = attachWritable();
    expect(scrollEl.style.cursor).toBe("text");

    surface["setCursor"](scrollEl, "pointer");
    expect(scrollEl.style.cursor).toBe("pointer");

    surface["setCursor"](scrollEl, "grabbing");
    expect(scrollEl.style.cursor).toBe("grabbing");

    surface.dispose();
  });

  it("still skips a redundant write", () => {
    // The dedup is why this helper exists: mousemove fires many times per
    // frame and rewriting the same value dirties style every time.
    const { surface, scrollEl } = attachWritable();
    surface["setCursor"](scrollEl, "pointer");
    let writes = 0;
    Object.defineProperty(scrollEl.style, "cursor", {
      configurable: true,
      get: () => "pointer",
      set: () => {
        writes += 1;
      },
    });
    surface["setCursor"](scrollEl, "pointer");
    expect(writes).toBe(0);

    surface.dispose();
  });

  it("does not carry a stale cursor across a re-attach", () => {
    // detach() drops the element; attach() builds a fresh one whose inline
    // style is the I-beam again. A cache left pointing at the old element's
    // value would suppress the next real change against the new one.
    const first = attachWritable();
    first.surface["setCursor"](first.scrollEl, "pointer");

    first.surface.detach();
    first.surface.attach(document.createElement("div"));
    const scrollEl = first.surface["scrollEl"];
    if (!scrollEl) throw new Error("expected a scroll surface");
    expect(scrollEl.style.cursor).toBe("text");

    first.surface["setCursor"](scrollEl, "pointer");
    expect(scrollEl.style.cursor).toBe("pointer");

    first.surface.dispose();
  });
});

describe("BlitTerminalSurface mouse-mode touch scrolling", () => {
  beforeEach(() => {
    mockCanvasContext();
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        disconnect() {}
      },
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  /** jsdom implements neither Touch nor TouchEvent, and the handlers only
   *  ever reach for the identifier, the client point and the touch lists. */
  function touchEvent(
    type: string,
    points: { identifier: number; clientX: number; clientY: number }[],
    opts: { ongoing?: boolean } = {},
  ): Event {
    const list = {
      length: points.length,
      item: (i: number) => points[i] ?? null,
      [0]: points[0],
    } as unknown as TouchList;
    const empty = { length: 0, item: () => null } as unknown as TouchList;
    const ev = new Event(type, { bubbles: true, cancelable: true });
    Object.defineProperty(ev, "touches", {
      value: opts.ongoing === false ? empty : list,
    });
    Object.defineProperty(ev, "changedTouches", { value: list });
    return ev;
  }

  function attachMouseMode() {
    const sendMouse = vi.fn();
    const surface = new BlitTerminalSurface({ sessionId: "s1" });
    const container = document.createElement("div");
    surface.attach(container);
    // @ts-expect-error — fake workspace capturing mouse-mode wheel reports.
    surface["_workspace"] = { sendMouse };
    // @ts-expect-error — fake wasm terminal in mouse-reporting mode.
    surface["terminal"] = { mouse_mode: () => 1 };
    const scrollEl = surface["scrollEl"];
    if (!scrollEl) throw new Error("expected a scroll surface");
    const lineH = surface["cell"].h || 20;
    return { surface, scrollEl, sendMouse, lineH };
  }

  it("swiping up reports wheel-down, matching natural touch scrolling", () => {
    const { surface, scrollEl, sendMouse, lineH } = attachMouseMode();
    const finger = { identifier: 1, clientX: 40, clientY: 300 };

    scrollEl.dispatchEvent(touchEvent("touchstart", [finger]));
    scrollEl.dispatchEvent(
      touchEvent("touchmove", [{ ...finger, clientY: 300 - 2 * lineH }]),
    );
    scrollEl.dispatchEvent(
      touchEvent("touchend", [{ ...finger, clientY: 300 - 2 * lineH }], {
        ongoing: false,
      }),
    );

    expect(sendMouse.mock.calls.map((c) => c[2])).toEqual([65, 65]);
    surface.dispose();
  });

  it("swiping down reports wheel-up, matching natural touch scrolling", () => {
    const { surface, scrollEl, sendMouse, lineH } = attachMouseMode();
    const finger = { identifier: 1, clientX: 40, clientY: 100 };

    scrollEl.dispatchEvent(touchEvent("touchstart", [finger]));
    scrollEl.dispatchEvent(
      touchEvent("touchmove", [{ ...finger, clientY: 100 + 2 * lineH }]),
    );
    scrollEl.dispatchEvent(
      touchEvent("touchend", [{ ...finger, clientY: 100 + 2 * lineH }], {
        ongoing: false,
      }),
    );

    expect(sendMouse.mock.calls.map((c) => c[2])).toEqual([64, 64]);
    surface.dispose();
  });

  it("reports every wheel step at the cell where the drag began", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();
    // Pin the cell metrics and grid size so client pixels map to cells
    // deterministically (jsdom layout would otherwise collapse the grid).
    // @ts-expect-error — touching private state purely to drive the test.
    surface["cell"] = { w: 10, h: 20, pw: 20, ph: 40 };
    // @ts-expect-error — touching private state purely to drive the test.
    surface["_rows"] = 24;
    // @ts-expect-error — touching private state purely to drive the test.
    surface["_cols"] = 80;
    const lineH = 20;
    const finger = { identifier: 1, clientX: 45, clientY: 300 };

    scrollEl.dispatchEvent(touchEvent("touchstart", [finger]));
    // Drag up two lines while wandering sideways; the reported position
    // must stay where the drag began, not follow the finger.
    scrollEl.dispatchEvent(
      touchEvent("touchmove", [
        { ...finger, clientX: 120, clientY: 300 - 2 * lineH },
      ]),
    );
    scrollEl.dispatchEvent(
      touchEvent(
        "touchend",
        [{ ...finger, clientX: 120, clientY: 300 - 2 * lineH }],
        { ongoing: false },
      ),
    );

    // Drag-begin cell: col floor(45/10)=4, row floor(300/20)=15.
    expect(sendMouse.mock.calls.map((c) => [c[2], c[3], c[4]])).toEqual([
      [65, 4, 15],
      [65, 4, 15],
    ]);
    surface.dispose();
  });
});

describe("BlitTerminalSurface mouse-mode touch momentum", () => {
  /** Fake clock the surface reads through `performance.now`, so a flick can
   *  be dealt out at a chosen frame rate instead of in real time. */
  let clock = 0;
  /** Frame callbacks, in the order they were requested. Nothing runs them
   *  by itself — `coast` picks out the ones the fling queued. */
  let frames: FrameRequestCallback[] = [];

  beforeEach(() => {
    mockCanvasContext();
    clock = 1000;
    frames = [];
    vi.spyOn(performance, "now").mockImplementation(() => clock);
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn((cb: FrameRequestCallback) => frames.push(cb)),
    );
    // A cancelled callback stays in the queue; running it is harmless
    // because the fling drops its state before it cancels.
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe() {}
        disconnect() {}
      },
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function touchEvent(
    type: string,
    points: { identifier: number; clientX: number; clientY: number }[],
    opts: { ongoing?: boolean } = {},
  ): Event {
    const list = {
      length: points.length,
      item: (i: number) => points[i] ?? null,
      [0]: points[0],
    } as unknown as TouchList;
    const empty = { length: 0, item: () => null } as unknown as TouchList;
    const ev = new Event(type, { bubbles: true, cancelable: true });
    Object.defineProperty(ev, "touches", {
      value: opts.ongoing === false ? empty : list,
    });
    Object.defineProperty(ev, "changedTouches", { value: list });
    return ev;
  }

  function attachMouseMode() {
    const sendMouse = vi.fn();
    const surface = new BlitTerminalSurface({ sessionId: "s1" });
    const container = document.createElement("div");
    surface.attach(container);
    // @ts-expect-error — fake workspace capturing mouse-mode wheel reports.
    surface["_workspace"] = { sendMouse };
    let mode = 1;
    // @ts-expect-error — fake wasm terminal in mouse-reporting mode.
    surface["terminal"] = { mouse_mode: () => mode };
    // Pin the metrics: jsdom would collapse the grid and take cell.h with it.
    // @ts-expect-error — touching private state purely to drive the test.
    surface["cell"] = { w: 10, h: 20, pw: 20, ph: 40 };
    // @ts-expect-error — touching private state purely to drive the test.
    surface["_rows"] = 24;
    // @ts-expect-error — touching private state purely to drive the test.
    surface["_cols"] = 80;
    const scrollEl = surface["scrollEl"];
    if (!scrollEl) throw new Error("expected a scroll surface");
    return {
      surface,
      scrollEl,
      sendMouse,
      lineH: 20,
      leaveMouseMode: () => {
        mode = 0;
      },
    };
  }

  /**
   * Throw the content: a drag at `pxPerFrame` for `moves` frames, then a
   * lift with no pause before it. Returns the index the fling's own frame
   * callbacks start at, so the coast can be run without also driving the
   * render loop.
   */
  function flick(
    scrollEl: HTMLElement,
    { from = 300, pxPerFrame = 32, moves = 4, frameMs = 16, dir = -1 } = {},
  ): number {
    const finger = { identifier: 1, clientX: 45, clientY: from };
    scrollEl.dispatchEvent(touchEvent("touchstart", [finger]));
    let y = from;
    for (let i = 0; i < moves; i++) {
      clock += frameMs;
      y += dir * pxPerFrame;
      scrollEl.dispatchEvent(
        touchEvent("touchmove", [{ ...finger, clientY: y }]),
      );
    }
    const base = frames.length;
    scrollEl.dispatchEvent(
      touchEvent("touchend", [{ ...finger, clientY: y }], { ongoing: false }),
    );
    return base;
  }

  /** Run the coast from `base` until it stops asking for frames, or until
   *  `maxFrames` — a fling that never converges must fail, not hang. */
  function coast(base: number, { frameMs = 16, maxFrames = 400 } = {}): number {
    let i = base;
    let ran = 0;
    while (i < frames.length && ran < maxFrames) {
      const cb = frames[i]!;
      i++;
      ran++;
      clock += frameMs;
      cb(clock);
    }
    return ran;
  }

  it("keeps scrolling after the finger lifts, then coasts to a stop", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();

    const base = flick(scrollEl);
    const duringDrag = sendMouse.mock.calls.length;
    // 4 frames x 32px over a 20px line.
    expect(duringDrag).toBe(6);

    const framesRun = coast(base);
    const total = sendMouse.mock.calls.length;

    // It kept going on its own...
    expect(total).toBeGreaterThan(duringDrag);
    // ...in the direction of the throw...
    expect(sendMouse.mock.calls.every((c) => c[2] === 65)).toBe(true);
    // ...at the cell the drag began (col 45/10, row 300/20)...
    expect(sendMouse.mock.calls.every((c) => c[3] === 4 && c[4] === 15)).toBe(
      true,
    );
    // ...and stopped by itself rather than running into the frame cap.
    expect(framesRun).toBeLessThan(400);
    // A 2 px/ms throw decaying at 0.998/ms travels ~1000px ≈ 50 lines.
    expect(total - duringDrag).toBeGreaterThan(30);
    expect(total - duringDrag).toBeLessThan(70);

    surface.dispose();
  });

  it("coasts downward for a downward throw", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();

    const base = flick(scrollEl, { from: 100, dir: 1 });
    const duringDrag = sendMouse.mock.calls.length;
    coast(base);

    expect(sendMouse.mock.calls.length).toBeGreaterThan(duringDrag);
    expect(sendMouse.mock.calls.every((c) => c[2] === 64)).toBe(true);

    surface.dispose();
  });

  it("doesn't coast when the finger was placing the content, not throwing", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();

    // Same distance, spread over frames slow enough to be a drag.
    const base = flick(scrollEl, { pxPerFrame: 8, moves: 16, frameMs: 80 });
    const duringDrag = sendMouse.mock.calls.length;
    expect(duringDrag).toBeGreaterThan(0);

    expect(coast(base)).toBe(0);
    expect(sendMouse.mock.calls.length).toBe(duringDrag);

    surface.dispose();
  });

  it("doesn't coast when the finger came to rest before lifting", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();
    const finger = { identifier: 1, clientX: 45, clientY: 300 };

    scrollEl.dispatchEvent(touchEvent("touchstart", [finger]));
    clock += 16;
    scrollEl.dispatchEvent(
      touchEvent("touchmove", [{ ...finger, clientY: 300 - 64 }]),
    );
    const duringDrag = sendMouse.mock.calls.length;
    // The finger stayed put for a moment before leaving the glass.
    clock += 400;
    const base = frames.length;
    scrollEl.dispatchEvent(
      touchEvent("touchend", [{ ...finger, clientY: 300 - 64 }], {
        ongoing: false,
      }),
    );

    expect(coast(base)).toBe(0);
    expect(sendMouse.mock.calls.length).toBe(duringDrag);

    surface.dispose();
  });

  it("a finger back on the glass catches the coast", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();

    const base = flick(scrollEl);
    // Let it run a few frames, then land a finger.
    let i = base;
    for (; i < base + 3 && i < frames.length; i++) {
      clock += 16;
      frames[i]!(clock);
    }
    const caught = sendMouse.mock.calls.length;
    expect(caught).toBeGreaterThan(0);

    scrollEl.dispatchEvent(
      touchEvent("touchstart", [{ identifier: 2, clientX: 45, clientY: 300 }]),
    );
    coast(i);

    expect(sendMouse.mock.calls.length).toBe(caught);
    surface.dispose();
  });

  it("stops coasting when the app leaves mouse mode", () => {
    const { surface, scrollEl, sendMouse, leaveMouseMode } = attachMouseMode();

    const base = flick(scrollEl);
    let i = base;
    for (; i < base + 3 && i < frames.length; i++) {
      clock += 16;
      frames[i]!(clock);
    }
    const before = sendMouse.mock.calls.length;
    // The TUI exited; scrollback (with its own native momentum) owns the
    // gesture again, so nothing more should be reported to the app.
    leaveMouseMode();
    coast(i);

    expect(sendMouse.mock.calls.length).toBe(before);
    surface.dispose();
  });

  it("drops the coast when the surface goes away mid-flight", () => {
    const { surface, scrollEl, sendMouse } = attachMouseMode();

    const base = flick(scrollEl);
    const duringDrag = sendMouse.mock.calls.length;
    surface.dispose();
    coast(base);

    expect(sendMouse.mock.calls.length).toBe(duringDrag);
  });
});

describe("BlitTerminalSurface host text prediction", () => {
  beforeEach(() => {
    mockCanvasContext();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  /** A surface with the capture field accumulating the line, as it is on
   *  macOS at a shell prompt.  A real prompt is `-icanon -echo` — the shell
   *  does its own line editing — so those flags say nothing here; the
   *  alternate screen is the gate. */
  function attachPrediction(
    mode: { echo?: boolean; altScreen?: boolean; capture?: boolean } = {},
  ) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    const sendInput = vi.fn();
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    document.body.appendChild(input);
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    const chip = document.createElement("div");
    // @ts-expect-error — and the chip the mount path would have made.
    s["chipEl"] = chip;
    // @ts-expect-error — stand in for the platform gate (macOS desktop).
    s["_predictionCapture"] = mode.capture ?? true;
    // @ts-expect-error — a terminal in line-editing mode.
    s["terminal"] = {
      echo: () => mode.echo ?? false,
      alt_screen: () => mode.altScreen ?? false,
      app_cursor: () => false,
      bracketed_paste: () => false,
      cursor_row: 0,
      cursor_col: 0,
    };
    // @ts-expect-error — wire the keydown/composition/input listeners.
    s["setupKeyboard"]();
    return { s, input, chip, sendInput };
  }

  /** What the browser does when the field is left to take a key: put the text
   *  in, then fire `input`. */
  function fieldBecomes(
    input: HTMLTextAreaElement,
    value: string,
    opts: { selStart?: number; selEnd?: number; inputType?: string } = {},
  ) {
    input.value = value;
    input.setSelectionRange(
      opts.selStart ?? value.length,
      opts.selEnd ?? opts.selStart ?? value.length,
    );
    input.dispatchEvent(
      new InputEvent("input", {
        inputType: opts.inputType ?? "insertText",
        bubbles: true,
      }),
    );
  }

  function text(sendInput: ReturnType<typeof vi.fn>): string {
    return sendInput.mock.calls
      .map((c) => new TextDecoder().decode(c[1] as Uint8Array))
      .join("");
  }

  it("lets a printable key reach the field instead of encoding it", () => {
    const { input, sendInput } = attachPrediction();
    const e = new KeyboardEvent("keydown", { key: "g", cancelable: true });
    input.dispatchEvent(e);

    // Nothing forwarded yet, and the browser is free to insert the character:
    // without that the host has no text to predict against.
    expect(e.defaultPrevented).toBe(false);
    expect(sendInput).not.toHaveBeenCalled();

    fieldBecomes(input, "g");
    expect(text(sendInput)).toBe("g");
  });

  it("keeps the old encode-at-keydown path in a full-screen TUI", () => {
    const { input, sendInput } = attachPrediction({ altScreen: true });
    const e = new KeyboardEvent("keydown", { key: "g", cancelable: true });
    input.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(true);
    expect(text(sendInput)).toBe("g");
  });

  it("shows a proposal in the chip without forwarding it", () => {
    const { input, chip, sendInput } = attachPrediction();
    fieldBecomes(input, "git st");
    sendInput.mockClear();

    // The host proposes "atus" as a selected tail.
    fieldBecomes(input, "git status", { selStart: 6, selEnd: 10 });

    expect(sendInput).not.toHaveBeenCalled();
    expect(chip.textContent).toBe("atus");
    expect(chip.style.display).toBe("block");
  });

  it("forwards only the tail when the proposal is accepted", () => {
    const { input, chip, sendInput } = attachPrediction();
    fieldBecomes(input, "git st");
    fieldBecomes(input, "git status", { selStart: 6, selEnd: 10 });
    sendInput.mockClear();

    // Accepting collapses the selection to the end of the line.
    fieldBecomes(input, "git status");

    expect(text(sendInput)).toBe("atus");
    expect(chip.style.display).toBe("none");
  });

  it("refuses a substitution over text the pty already has", () => {
    const { input, sendInput } = attachPrediction();
    fieldBecomes(input, "teh");
    sendInput.mockClear();

    fieldBecomes(input, "the", { inputType: "insertReplacementText" });

    expect(sendInput).not.toHaveBeenCalled();
    expect(input.value).toBe("teh");
  });

  it("forwards a Backspace through the field as DEL", () => {
    const { input, sendInput } = attachPrediction();
    fieldBecomes(input, "ab");
    sendInput.mockClear();

    const e = new KeyboardEvent("keydown", {
      key: "Backspace",
      cancelable: true,
    });
    input.dispatchEvent(e);
    expect(e.defaultPrevented).toBe(false);
    expect(sendInput).not.toHaveBeenCalled();

    fieldBecomes(input, "a", { inputType: "deleteContentBackward" });
    expect(Array.from(sendInput.mock.calls[0]![1] as Uint8Array)).toEqual([
      0x7f,
    ]);
  });

  it("still encodes Backspace itself once the field is empty", () => {
    const { input, sendInput } = attachPrediction();
    const e = new KeyboardEvent("keydown", {
      key: "Backspace",
      cancelable: true,
    });
    input.dispatchEvent(e);

    expect(e.defaultPrevented).toBe(true);
    expect(Array.from(sendInput.mock.calls[0]![1] as Uint8Array)).toEqual([
      0x7f,
    ]);
  });

  it("empties the field on Enter so the next line starts clean", () => {
    const { input, chip, sendInput } = attachPrediction();
    fieldBecomes(input, "git status", { selStart: 6, selEnd: 10 });
    sendInput.mockClear();

    input.dispatchEvent(
      new KeyboardEvent("keydown", { key: "Enter", cancelable: true }),
    );

    expect(text(sendInput)).toBe("\r");
    expect(input.value).toBe("");
    expect(chip.style.display).toBe("none");

    // And the line that follows is forwarded whole, not as a delta against
    // the line that was submitted.
    sendInput.mockClear();
    fieldBecomes(input, "ls");
    expect(text(sendInput)).toBe("ls");
  });

  it("holds a real composition instead of streaming its intermediate states", () => {
    const { input, sendInput } = attachPrediction();
    input.dispatchEvent(new CompositionEvent("compositionstart"));
    fieldBecomes(input, "にほn", { inputType: "insertCompositionText" });
    expect(sendInput).not.toHaveBeenCalled();

    input.value = "日本";
    input.setSelectionRange(2, 2);
    input.dispatchEvent(
      new CompositionEvent("compositionend", { data: "日本" }),
    );

    expect(text(sendInput)).toBe("日本");
  });

  it("does not type a composition twice when input follows compositionend", () => {
    const { input, sendInput } = attachPrediction();
    input.dispatchEvent(new CompositionEvent("compositionstart"));
    input.value = "日本";
    input.setSelectionRange(2, 2);
    input.dispatchEvent(
      new CompositionEvent("compositionend", { data: "日本" }),
    );
    // Chromium emits one more input event after compositionend.
    input.dispatchEvent(
      new InputEvent("input", {
        inputType: "insertCompositionText",
        bubbles: true,
      }),
    );

    expect(text(sendInput)).toBe("日本");
  });

  it("sends a paste once, stripped of the line the field was holding", () => {
    const { input, sendInput } = attachPrediction();
    fieldBecomes(input, "echo ");
    sendInput.mockClear();

    fieldBecomes(input, "echo hello", { inputType: "insertFromPaste" });

    expect(text(sendInput)).toBe("hello");
    expect(input.value).toBe("");
  });
});

describe("BlitTerminalSurface composition chip", () => {
  beforeEach(() => {
    mockCanvasContext();
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  /** A writable terminal with the chip the mount path makes for every one of
   *  them.  `capture` is the macOS-only prediction gate; a composition has to
   *  be drawn with it off, which is every other platform. */
  function attach(capture: boolean) {
    const s = new BlitTerminalSurface({ sessionId: "s1" });
    const sendInput = vi.fn();
    // @ts-expect-error — install a fake workspace stub.
    s["_workspace"] = { sendInput };
    // @ts-expect-error — minimal connection exposing only a connected transport.
    s["_blitConn"] = { transport: { status: "connected" } };
    const input = document.createElement("textarea");
    document.body.appendChild(input);
    // @ts-expect-error — install the hidden capture textarea directly.
    s["inputEl"] = input;
    const chip = document.createElement("div");
    // @ts-expect-error — and the chip beside the cursor.
    s["chipEl"] = chip;
    // @ts-expect-error — the platform gate for prediction, not for the chip.
    s["_predictionCapture"] = capture;
    // @ts-expect-error — a terminal on the main screen.
    s["terminal"] = {
      echo: () => false,
      alt_screen: () => false,
      app_cursor: () => false,
      bracketed_paste: () => false,
      cursor_row: 0,
      cursor_col: 0,
    };
    // @ts-expect-error — wire the keydown/composition/input listeners.
    s["setupKeyboard"]();
    return { s, input, chip, sendInput };
  }

  /** What the browser does while an IME builds a composition: the buffer goes
   *  into the field, then `input` fires. */
  function composing(input: HTMLTextAreaElement, value: string) {
    input.value = value;
    input.setSelectionRange(value.length, value.length);
    input.dispatchEvent(
      new InputEvent("input", {
        inputType: "insertCompositionText",
        bubbles: true,
      }),
    );
  }

  function sent(sendInput: ReturnType<typeof vi.fn>): string {
    return sendInput.mock.calls
      .map((c) => new TextDecoder().decode(c[1] as Uint8Array))
      .join("");
  }

  for (const capture of [false, true]) {
    const where = capture ? "with prediction on" : "with prediction off";

    it(`draws the composition being built, ${where}`, () => {
      const { input, chip, sendInput } = attach(capture);
      input.dispatchEvent(new CompositionEvent("compositionstart"));
      composing(input, "に");
      expect(chip.textContent).toBe("に");
      expect(chip.style.display).toBe("block");

      composing(input, "にほn");
      expect(chip.textContent).toBe("にほn");

      // Nothing reaches the shell until the composition is committed: a
      // terminal cannot take back a romaji it has already been given.
      expect(sendInput).not.toHaveBeenCalled();
    });

    it(`underlines the composition, as an IME draws its own, ${where}`, () => {
      const { input, chip } = attach(capture);
      input.dispatchEvent(new CompositionEvent("compositionstart"));
      composing(input, "にほn");
      expect(chip.style.textDecoration).toBe("underline");
    });

    it(`clears the chip and commits on compositionend, ${where}`, () => {
      const { input, chip, sendInput } = attach(capture);
      input.dispatchEvent(new CompositionEvent("compositionstart"));
      composing(input, "にほn");

      input.value = "日本";
      input.setSelectionRange(2, 2);
      input.dispatchEvent(
        new CompositionEvent("compositionend", { data: "日本" }),
      );

      expect(chip.style.display).toBe("none");
      expect(sent(sendInput)).toBe("日本");
    });

    it(`clears the chip when the composition is abandoned, ${where}`, () => {
      const { input, chip, sendInput } = attach(capture);
      input.dispatchEvent(new CompositionEvent("compositionstart"));
      composing(input, "にほn");

      // Escape: the IME withdraws the buffer and commits nothing.
      input.value = "";
      input.setSelectionRange(0, 0);
      input.dispatchEvent(new CompositionEvent("compositionend", { data: "" }));

      expect(chip.style.display).toBe("none");
      expect(sent(sendInput)).toBe("");
    });
  }
});
