import { cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { BlitWorkspaceProvider } from "../BlitContext";
import { BlitSurfaceView } from "../BlitSurfaceView";

type FakeConnection = {
  surfaceStore: {
    getSurface: ReturnType<typeof vi.fn>;
    onChange: ReturnType<typeof vi.fn>;
    onFrame: ReturnType<typeof vi.fn>;
  };
  sendSurfaceAxis: ReturnType<typeof vi.fn>;
  sendSurfacePointer: ReturnType<typeof vi.fn>;
  sendSurfaceInput: ReturnType<typeof vi.fn>;
  sendSurfaceFocus: ReturnType<typeof vi.fn>;
};

function createConnection(): FakeConnection {
  return {
    surfaceStore: {
      getSurface: vi.fn(() => ({ sessionId: 7, width: 320, height: 200 })),
      onChange: vi.fn(() => () => {}),
      onFrame: vi.fn(() => () => {}),
    },
    sendSurfaceAxis: vi.fn(),
    sendSurfacePointer: vi.fn(),
    sendSurfaceInput: vi.fn(),
    sendSurfaceFocus: vi.fn(),
  };
}

describe("BlitSurfaceView", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
      drawImage: vi.fn(),
    } as unknown as CanvasRenderingContext2D);
  });

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it("installs a non-passive wheel listener and forwards wheel events", async () => {
    const conn = createConnection();
    const workspace = {
      getConnection: vi.fn(() => conn),
    };
    const addEventListenerSpy = vi.spyOn(
      HTMLCanvasElement.prototype,
      "addEventListener",
    );
    const removeEventListenerSpy = vi.spyOn(
      HTMLCanvasElement.prototype,
      "removeEventListener",
    );

    const { unmount } = render(
      <BlitWorkspaceProvider workspace={workspace as never}>
        <BlitSurfaceView connectionId={"c1"} surfaceId={11} />
      </BlitWorkspaceProvider>,
    );

    await waitFor(() => {
      expect(
        addEventListenerSpy.mock.calls.filter(([type]) => type === "wheel").length,
      ).toBeGreaterThan(0);
    });

    const wheelCall = addEventListenerSpy.mock.calls
      .filter(([type]) => type === "wheel")
      .at(-1);
    expect(wheelCall).toBeDefined();
    const wheelListener = wheelCall?.[1] as EventListener;
    const event = {
      deltaX: 0,
      deltaY: 2.5,
      preventDefault: vi.fn(),
    } as unknown as WheelEvent;

    wheelListener(event);

    expect(event.preventDefault).toHaveBeenCalledTimes(1);
    expect(conn.sendSurfaceAxis).toHaveBeenCalledWith(7, 11, 0, 250);

    unmount();

    expect(removeEventListenerSpy).toHaveBeenCalledWith("wheel", wheelListener);
  });
});
