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
  beforeEach(() => {
    vi.stubGlobal("requestAnimationFrame", vi.fn(() => 1));
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
  });

  afterEach(() => {
    cleanup();
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
});
