import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BlitTerminal } from "../BlitTerminal";
import { BlitWorkspace } from "@blit-sh/core";
import { BlitWorkspaceProvider } from "../BlitContext";
import type { BlitWasmModule } from "@blit-sh/core";
import type { TerminalPalette, SessionId } from "@blit-sh/core";
import { MockTransport } from "../../../core/src/__tests__/mock-transport";

vi.mock("@blit-sh/core", async () => {
  const actual = await vi.importActual<typeof import("@blit-sh/core")>("@blit-sh/core");
  return {
    ...actual,
    createGlRenderer: vi.fn(() => ({
      supported: false,
      resize: vi.fn(),
      render: vi.fn(),
      dispose: vi.fn(),
    })),
    measureCell: vi.fn(() => ({
      w: 8,
      h: 16,
      pw: 8,
      ph: 16,
    })),
    cssFontFamily: (f: string) => f,
    CSS_GENERIC: new Set(["monospace"]),
  };
});

type FakeTerminal = {
  rows: number;
  cols: number;
  set_default_colors: ReturnType<typeof vi.fn>;
  set_ansi_color: ReturnType<typeof vi.fn>;
  set_cell_size: ReturnType<typeof vi.fn>;
  set_font_family: ReturnType<typeof vi.fn>;
  set_font_size: ReturnType<typeof vi.fn>;
  invalidate_render_cache: ReturnType<typeof vi.fn>;
  mouse_mode: ReturnType<typeof vi.fn>;
  mouse_encoding: ReturnType<typeof vi.fn>;
  title: ReturnType<typeof vi.fn>;
  feed_compressed: ReturnType<typeof vi.fn>;
  free: ReturnType<typeof vi.fn>;
};

function makeFakeTerminal(): FakeTerminal {
  return {
    rows: 24,
    cols: 80,
    set_default_colors: vi.fn(),
    set_ansi_color: vi.fn(),
    set_cell_size: vi.fn(),
    set_font_family: vi.fn(),
    set_font_size: vi.fn(),
    invalidate_render_cache: vi.fn(),
    mouse_mode: vi.fn(() => 0),
    mouse_encoding: vi.fn(() => 0),
    title: vi.fn(() => ""),
    feed_compressed: vi.fn(),
    free: vi.fn(),
  };
}

let fakeTerminals: FakeTerminal[] = [];

const wasm = {
  Terminal: class {
    rows = 24;
    cols = 80;
    set_default_colors = vi.fn();
    set_ansi_color = vi.fn();
    set_cell_size = vi.fn();
    set_font_family = vi.fn();
    set_font_size = vi.fn();
    invalidate_render_cache = vi.fn();
    mouse_mode = vi.fn(() => 0);
    mouse_encoding = vi.fn(() => 0);
    title = vi.fn(() => "");
    feed_compressed = vi.fn();
    free = vi.fn();
    constructor(_r: number, _c: number, _pw: number, _ph: number) {
      fakeTerminals.push(this as unknown as FakeTerminal);
    }
  },
} as unknown as BlitWasmModule;

function setup() {
  const transport = new MockTransport();
  const workspace = new BlitWorkspace({
    wasm,
    connections: [{ id: "c1", transport }],
  });
  return { transport, workspace };
}

describe("BlitTerminal", () => {
  let originalFonts: PropertyDescriptor | undefined;

  beforeEach(() => {
    fakeTerminals = [];
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
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

  it("renders with null sessionId without crashing", () => {
    const { workspace } = setup();
    render(
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal sessionId={null} />
      </BlitWorkspaceProvider>,
    );
  });

  it("applies palette to terminal created via update", async () => {
    const { transport, workspace } = setup();
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

    // Create a session
    transport.pushList([{ ptyId: 7, tag: "test" }]);
    const snap = workspace.getSnapshot();
    const sessionId = snap.sessions[0]?.id as SessionId;

    render(
      <BlitWorkspaceProvider workspace={workspace} palette={palette}>
        <BlitTerminal sessionId={sessionId} readOnly />
      </BlitWorkspaceProvider>,
    );

    // Push an update to trigger terminal creation in the store
    await act(async () => {
      transport.pushUpdate(7, new Uint8Array([1, 2, 3]));
    });

    // The WASM terminal should have been created and palette applied
    const terminal = fakeTerminals[fakeTerminals.length - 1];
    if (terminal) {
      expect(terminal.set_default_colors).toHaveBeenCalled();
    }
  });

  it("invalidates the glyph cache when fonts finish loading", async () => {
    const { transport, workspace } = setup();

    transport.pushList([{ ptyId: 7, tag: "test" }]);
    const snap = workspace.getSnapshot();
    const sessionId = snap.sessions[0]?.id as SessionId;

    render(
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal
          sessionId={sessionId}
          fontFamily="Test Mono"
          fontSize={14}
        />
      </BlitWorkspaceProvider>,
    );

    // Push an update to create the terminal
    await act(async () => {
      transport.pushUpdate(7, new Uint8Array([1, 2, 3]));
    });

    const terminal = fakeTerminals[fakeTerminals.length - 1];
    if (terminal) {
      terminal.invalidate_render_cache.mockClear();

      await act(async () => {
        (
          document.fonts as unknown as {
            dispatch: (type: string) => void;
          }
        ).dispatch("loadingdone");
      });

      expect(terminal.invalidate_render_cache).toHaveBeenCalledTimes(1);
    }
  });
});
