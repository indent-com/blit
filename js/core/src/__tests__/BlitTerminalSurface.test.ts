import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { BlitTerminalSurface } from "../BlitTerminalSurface";

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

  it("Ctrl+V sends the ^V control character (0x16)", () => {
    const sendInput = vi.fn();
    const { input } = attachKeyboard(sendInput);

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
    expect(sendInput).toHaveBeenCalledTimes(1);
    const payload = sendInput.mock.calls[0][1] as Uint8Array;
    expect(Array.from(payload)).toEqual([0x16]);
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
});
