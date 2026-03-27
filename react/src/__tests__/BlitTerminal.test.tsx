import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BlitTerminal } from "../BlitTerminal";
import type { TerminalStore } from "../TerminalStore";
import type { TerminalPalette } from "../types";
import { MockTransport } from "./mock-transport";

vi.mock("../gl-renderer", () => ({
  createGlRenderer: vi.fn(() => ({
    supported: false,
    resize: vi.fn(),
    render: vi.fn(),
    dispose: vi.fn(),
  })),
}));

vi.mock("../hooks/useBlitTerminal", () => ({
  measureCell: vi.fn(() => ({
    w: 8,
    h: 16,
    pw: 8,
    ph: 16,
  })),
}));

type FakeTerminal = {
  rows: number;
  cols: number;
  set_default_colors: ReturnType<typeof vi.fn>;
  set_ansi_color: ReturnType<typeof vi.fn>;
  set_cell_size: ReturnType<typeof vi.fn>;
  set_font_family: ReturnType<typeof vi.fn>;
  invalidate_render_cache: ReturnType<typeof vi.fn>;
};

class MockStore {
  terminal: FakeTerminal | null = null;
  private dirtyListener: ((ptyId: number) => void) | null = null;

  addDirtyListener(listener: (ptyId: number) => void): () => void {
    this.dirtyListener = listener;
    return () => {
      if (this.dirtyListener === listener) this.dirtyListener = null;
    };
  }

  emitDirty(ptyId: number): void {
    this.dirtyListener?.(ptyId);
  }

  getTerminal(): FakeTerminal | null {
    return this.terminal;
  }

  onReady(): () => void {
    return () => {};
  }

  isReady(): boolean {
    return true;
  }

  retain(): void {}

  release(): void {}

  getSharedRenderer() {
    return null;
  }

  setCellSize(): void {}
}

describe("BlitTerminal", () => {
  let originalFonts: PropertyDescriptor | undefined;

  beforeEach(() => {
    vi.stubGlobal("requestAnimationFrame", vi.fn(() => 1));
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.stubGlobal(
      "ResizeObserver",
      class {
        observe(): void {}
        disconnect(): void {}
      },
    );
    const listeners = new Map<string, Set<EventListener>>();
    originalFonts = Object.getOwnPropertyDescriptor(document, "fonts");
    Object.defineProperty(document, "fonts", {
      configurable: true,
      value: {
        addEventListener: vi.fn((type: string, listener: EventListener) => {
          let set = listeners.get(type);
          if (!set) {
            set = new Set();
            listeners.set(type, set);
          }
          set.add(listener);
        }),
        removeEventListener: vi.fn((type: string, listener: EventListener) => {
          listeners.get(type)?.delete(listener);
        }),
        dispatch(type: string) {
          for (const listener of listeners.get(type) ?? []) {
            listener(new Event(type));
          }
        },
      },
    });
  });

  afterEach(() => {
    cleanup();
    if (originalFonts) {
      Object.defineProperty(document, "fonts", originalFonts);
    } else {
      delete (document as Document & { fonts?: unknown }).fonts;
    }
    vi.unstubAllGlobals();
    vi.clearAllMocks();
  });

  it("applies the configured palette when a terminal arrives after mount", () => {
    const transport = new MockTransport();
    const store = new MockStore();
    const palette: TerminalPalette = {
      id: "tomorrow",
      name: "Tomorrow",
      dark: false,
      fg: [34, 34, 34],
      bg: [255, 255, 255],
      ansi: Array.from(
        { length: 16 },
        (_, i) => [i, i + 1, i + 2] as [number, number, number],
      ),
    };

    render(
      <BlitTerminal
        ptyId={7}
        palette={palette}
        readOnly
        store={store as unknown as TerminalStore}
        transport={transport}
      />,
    );

    const terminal: FakeTerminal = {
      rows: 24,
      cols: 80,
      set_default_colors: vi.fn(),
      set_ansi_color: vi.fn(),
      set_cell_size: vi.fn(),
      set_font_family: vi.fn(),
      invalidate_render_cache: vi.fn(),
    };

    expect(terminal.set_default_colors).not.toHaveBeenCalled();
    expect(terminal.set_ansi_color).not.toHaveBeenCalled();

    act(() => {
      store.terminal = terminal;
      store.emitDirty(7);
    });

    expect(terminal.set_default_colors).toHaveBeenCalledTimes(1);
    expect(terminal.set_default_colors).toHaveBeenCalledWith(
      ...palette.fg,
      ...palette.bg,
    );
    expect(terminal.set_ansi_color).toHaveBeenCalledTimes(16);
    for (let i = 0; i < 16; i++) {
      expect(terminal.set_ansi_color).toHaveBeenNthCalledWith(
        i + 1,
        i,
        ...palette.ansi[i],
      );
    }
  });

  it("invalidates the glyph cache when fonts finish loading even if metrics stay the same", () => {
    const transport = new MockTransport();
    const store = new MockStore();
    const terminal: FakeTerminal = {
      rows: 24,
      cols: 80,
      set_default_colors: vi.fn(),
      set_ansi_color: vi.fn(),
      set_cell_size: vi.fn(),
      set_font_family: vi.fn(),
      invalidate_render_cache: vi.fn(),
    };
    store.terminal = terminal;

    render(
      <BlitTerminal
        ptyId={7}
        fontFamily="Test Mono"
        fontSize={14}
        store={store as unknown as TerminalStore}
        transport={transport}
      />,
    );

    terminal.invalidate_render_cache.mockClear();

    act(() => {
      (
        document.fonts as unknown as {
          dispatch: (type: string) => void;
        }
      ).dispatch("loadingdone");
    });

    expect(terminal.invalidate_render_cache).toHaveBeenCalledTimes(1);
    expect(terminal.set_font_family).toHaveBeenCalledWith("Test Mono");
  });
});
