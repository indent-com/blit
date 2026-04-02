import { cleanup, render } from "@solidjs/testing-library";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BlitTerminal } from "../BlitTerminal";
import { BlitWorkspace } from "@blit-sh/core";
import { BlitWorkspaceProvider } from "../BlitContext";
import type { BlitWasmModule } from "@blit-sh/core";
import type { TerminalPalette, SessionId } from "@blit-sh/core";
import { MockTransport } from "../../../core/src/__tests__/mock-transport";

// Mock BlitTerminalSurface so it never touches real DOM/Canvas APIs.
const mockAttach = vi.fn();
const mockDetach = vi.fn();
const mockDispose = vi.fn();
const mockSetWorkspace = vi.fn();
const mockSetConnection = vi.fn();
const mockSetSessionId = vi.fn();
const mockSetPalette = vi.fn();
const mockSetFontFamily = vi.fn();
const mockSetFontSize = vi.fn();
const mockSetShowCursor = vi.fn();
const mockSetOnRender = vi.fn();
const mockSetAdvanceRatio = vi.fn();
const mockSetReadOnly = vi.fn();
const mockResendSize = vi.fn();
const mockFocus = vi.fn();

vi.mock("@blit-sh/core", async () => {
  const actual = await vi.importActual<typeof import("@blit-sh/core")>(
    "@blit-sh/core",
  );
  return {
    ...actual,
    BlitTerminalSurface: class {
      currentTerminal = null;
      rows = 24;
      cols = 80;
      status = "disconnected";
      attach = mockAttach;
      detach = mockDetach;
      dispose = mockDispose;
      setWorkspace = mockSetWorkspace;
      setConnection = mockSetConnection;
      setSessionId = mockSetSessionId;
      setPalette = mockSetPalette;
      setFontFamily = mockSetFontFamily;
      setFontSize = mockSetFontSize;
      setShowCursor = mockSetShowCursor;
      setOnRender = mockSetOnRender;
      setAdvanceRatio = mockSetAdvanceRatio;
      setReadOnly = mockSetReadOnly;
      resendSize = mockResendSize;
      focus = mockFocus;
    },
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
  beforeEach(() => {
    fakeTerminals = [];
    vi.stubGlobal(
      "requestAnimationFrame",
      vi.fn(() => 1),
    );
    vi.stubGlobal("cancelAnimationFrame", vi.fn());
    vi.clearAllMocks();
  });

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("renders with null sessionId without crashing", () => {
    const { workspace } = setup();
    render(() => (
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal sessionId={null} />
      </BlitWorkspaceProvider>
    ));
  });

  it("creates surface and attaches to container", () => {
    const { workspace } = setup();
    render(() => (
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal sessionId={null} />
      </BlitWorkspaceProvider>
    ));
    expect(mockAttach).toHaveBeenCalledTimes(1);
    expect(mockSetWorkspace).toHaveBeenCalledWith(workspace);
  });

  it("disposes surface on unmount", () => {
    const { workspace } = setup();
    const { unmount } = render(() => (
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal sessionId={null} />
      </BlitWorkspaceProvider>
    ));
    unmount();
    expect(mockDispose).toHaveBeenCalledTimes(1);
  });

  it("forwards sessionId changes to surface", () => {
    const { transport, workspace } = setup();

    transport.pushList([{ ptyId: 7, tag: "test" }]);
    const snap = workspace.getSnapshot();
    const sessionId = snap.sessions[0]?.id as SessionId;

    render(() => (
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal sessionId={sessionId} readOnly />
      </BlitWorkspaceProvider>
    ));

    expect(mockSetSessionId).toHaveBeenCalledWith(sessionId);
  });

  it("forwards palette to surface", () => {
    const { workspace } = setup();
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

    render(() => (
      <BlitWorkspaceProvider workspace={workspace} palette={palette}>
        <BlitTerminal sessionId={null} />
      </BlitWorkspaceProvider>
    ));

    expect(mockSetPalette).toHaveBeenCalledWith(palette);
  });

  it("forwards font settings to surface", () => {
    const { workspace } = setup();

    render(() => (
      <BlitWorkspaceProvider workspace={workspace}>
        <BlitTerminal
          sessionId={null}
          fontFamily="Test Mono"
          fontSize={14}
        />
      </BlitWorkspaceProvider>
    ));

    expect(mockSetFontFamily).toHaveBeenCalledWith("Test Mono");
    expect(mockSetFontSize).toHaveBeenCalledWith(14);
  });
});
