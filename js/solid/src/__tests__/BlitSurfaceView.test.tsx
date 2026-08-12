import { cleanup, render } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { BlitWorkspace, ConnectionId } from "@blit-sh/core";
import { BlitWorkspaceProvider } from "../BlitContext";
import { BlitSurfaceView } from "../BlitSurfaceView";

const mockAttach = vi.fn();
const mockDispose = vi.fn();
const mockSetDisplaySize = vi.fn();
const mockRequestResize = vi.fn();
const mockSetTouchMode = vi.fn();

// The real canvas decodes video and probes WebCodecs; the view's contract
// with it is just these calls.
vi.mock("@blit-sh/core", async () => {
  const actual =
    await vi.importActual<typeof import("@blit-sh/core")>("@blit-sh/core");
  return {
    ...actual,
    detectCodecSupport: () => {},
    BlitSurfaceCanvas: class {
      canvasElement = null;
      attach = mockAttach;
      dispose = mockDispose;
      setDisplaySize = mockSetDisplaySize;
      requestResize = mockRequestResize;
      setConnectionId = vi.fn();
      setSurfaceId = vi.fn();
      setLive = vi.fn();
      setTouchMode = mockSetTouchMode;
    },
  };
});

/** No connection, so the view never reads a surface store. */
const workspace = {
  getConnection: () => null,
  subscribe: () => () => {},
} as unknown as BlitWorkspace;

/** jsdom has no ResizeObserver, and the view falls back to the container's
 *  bounding rect when none of its callbacks ever fire — which is the path
 *  under test. */
class NoopResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

const PANE_CSS_W = 800;
const PANE_CSS_H = 600;

function renderView(
  zoom: () => number | undefined,
  zoomMode: () => "relative" | "exact" | undefined = () => undefined,
  touchMode: () => "pointer" | "direct" | undefined = () => undefined,
) {
  return render(() => (
    <BlitWorkspaceProvider workspace={workspace}>
      <BlitSurfaceView
        connectionId={"conn-1" as ConnectionId}
        surfaceId={7}
        resizable
        zoom={zoom()}
        zoomMode={zoomMode()}
        touchMode={touchMode()}
      />
    </BlitWorkspaceProvider>
  ));
}

describe("BlitSurfaceView zoom", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("ResizeObserver", NoopResizeObserver);
    vi.stubGlobal("devicePixelRatio", 1);
    vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(
      () =>
        ({
          width: PANE_CSS_W,
          height: PANE_CSS_H,
          left: 0,
          top: 0,
        }) as DOMRect,
    );
  });

  afterEach(() => {
    cleanup();
    vi.useRealTimers();
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
    mockSetDisplaySize.mockClear();
    mockRequestResize.mockClear();
    mockSetTouchMode.mockClear();
  });

  it("asks the compositor for DPI × zoom while keeping the pane's pixels", () => {
    renderView(() => 1.5);
    // 800×600 device pixels either way — zoom does not change how many
    // pixels the pane has, only how many logical pixels the app fits in
    // them (800 × 120/180 = 533 wide).
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(800, 600, 180, 120);
    vi.advanceTimersByTime(50);
    expect(mockRequestResize).toHaveBeenLastCalledWith(800, 600, 180);
  });

  it("sends the display's DPI alone at 100%", () => {
    renderView(() => 1);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(800, 600, 120, 120);
    vi.advanceTimersByTime(50);
    expect(mockRequestResize).toHaveBeenLastCalledWith(800, 600, 120);
  });

  it("updates direct touch mode without remounting the canvas", () => {
    const [touchMode, setTouchMode] = createSignal<"pointer" | "direct">(
      "direct",
    );
    renderView(
      () => 1,
      () => undefined,
      touchMode,
    );
    expect(mockSetTouchMode).toHaveBeenLastCalledWith("direct");

    setTouchMode("pointer");
    expect(mockSetTouchMode).toHaveBeenLastCalledWith("pointer");
  });

  it("uses direct touch when touchMode is absent", () => {
    renderView(() => 1);
    expect(mockSetTouchMode).toHaveBeenLastCalledWith("direct");
  });

  it("treats an absent zoom as 100%", () => {
    renderView(() => undefined);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(800, 600, 120, 120);
  });

  it("re-sends the box when the zoom changes without a resize", () => {
    const [zoom, setZoom] = createSignal(1);
    renderView(zoom);
    vi.advanceTimersByTime(50);
    expect(mockRequestResize).toHaveBeenLastCalledWith(800, 600, 120);

    // Nothing about the container changed, so the ResizeObserver would
    // never fire — the zoom effect has to re-derive the scale itself.
    setZoom(2);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(800, 600, 240, 120);
    vi.advanceTimersByTime(50);
    expect(mockRequestResize).toHaveBeenLastCalledWith(800, 600, 240);
  });

  it("clamps a nonsensical zoom instead of asking for a zero scale", () => {
    renderView(() => 0);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(800, 600, 120, 120);
  });

  it("sends a sub-1x relative scale so a 1x pane can zoom out", () => {
    renderView(() => 0.5);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(800, 600, 60, 120);
    vi.advanceTimersByTime(50);
    expect(mockRequestResize).toHaveBeenLastCalledWith(800, 600, 60);
  });

  it("zooms out into the display's own DPI where there is some", () => {
    // 2x pane: 1600×1200 device pixels for the same 800×600 CSS box.  75%
    // lands on 1.5x, above the floor, so the app really does get more
    // logical pixels (1600 × 120/180 = 1066) than at 100%.
    vi.stubGlobal("devicePixelRatio", 2);
    renderView(() => 0.75);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(1600, 1200, 180, 240);
  });

  it("uses an exact scale independently of display DPI", () => {
    vi.stubGlobal("devicePixelRatio", 2);
    renderView(
      () => 1,
      () => "exact",
    );
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(1600, 1200, 120, 240);
    vi.advanceTimersByTime(50);
    expect(mockRequestResize).toHaveBeenLastCalledWith(1600, 1200, 120);
  });

  it("re-sends the box when switching between relative and exact scale", () => {
    vi.stubGlobal("devicePixelRatio", 2);
    const [mode, setMode] = createSignal<"relative" | "exact">("relative");
    renderView(() => 1, mode);
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(1600, 1200, 240, 240);

    setMode("exact");
    expect(mockSetDisplaySize).toHaveBeenLastCalledWith(1600, 1200, 120, 240);
  });
});
