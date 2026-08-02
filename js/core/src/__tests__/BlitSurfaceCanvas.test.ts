import { describe, expect, it } from "vitest";

import { BlitSurfaceCanvas } from "../BlitSurfaceCanvas";
import type { BlitWorkspace } from "../BlitWorkspace";

/** Minimal workspace stub: no connection, so the canvas never subscribes
 *  and layout can be exercised in isolation. */
function makeWorkspace(): BlitWorkspace {
  return {
    getConnection: () => null,
    subscribe: () => () => {},
  } as unknown as BlitWorkspace;
}

function attachCanvas() {
  const surface = new BlitSurfaceCanvas({
    workspace: makeWorkspace(),
    connectionId: "conn-1" as never,
    surfaceId: 7,
  });
  const container = document.createElement("div");
  surface.attach(container);
  const canvas = surface.canvasElement;
  if (!canvas) throw new Error("Expected surface canvas");
  return { surface, canvas };
}

describe("BlitSurfaceCanvas layout", () => {
  it("fills the container until a display size is known", () => {
    const { surface, canvas } = attachCanvas();
    expect(canvas.style.width).toBe("100%");
    expect(canvas.style.height).toBe("100%");
    expect(canvas.style.objectFit).toBe("contain");
    surface.dispose();
  });

  it("shows a frame smaller than the view at 1:1 device pixels, centered", () => {
    const { surface, canvas } = attachCanvas();
    // Backing buffer is the attach() default 640×480 "frame"; the view is
    // 1280×960 device pixels at scale 2 (scale120 = 240).  The frame must
    // not be upscaled: 640 device px = 320 CSS px, centered.
    surface.setDisplaySize(1280, 960, 240);
    expect(canvas.style.position).toBe("absolute");
    expect(canvas.style.width).toBe("320px");
    expect(canvas.style.height).toBe("240px");
    expect(canvas.style.left).toBe("160px");
    expect(canvas.style.top).toBe("120px");
    surface.dispose();
  });

  it("fills the view exactly when the frame matches it", () => {
    const { surface, canvas } = attachCanvas();
    canvas.width = 1280;
    canvas.height = 960;
    surface.setDisplaySize(1280, 960, 240);
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe("480px");
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe("0px");
    surface.dispose();
  });

  it("scales down (never clips) a frame transiently larger than the view", () => {
    const { surface, canvas } = attachCanvas();
    canvas.width = 2000;
    canvas.height = 480;
    surface.setDisplaySize(1280, 960, 240);
    // fit = 1280/2000 = 0.64 → 1280×307 device px, vertically centered.
    expect(canvas.style.width).toBe("640px");
    expect(canvas.style.height).toBe(`${307 / 2}px`);
    expect(canvas.style.left).toBe("0px");
    expect(canvas.style.top).toBe(`${326 / 2}px`);
    surface.dispose();
  });

  it("reverts to fill-and-contain when the display size is cleared", () => {
    const { surface, canvas } = attachCanvas();
    surface.setDisplaySize(1280, 960, 240);
    surface.setDisplaySize(null);
    expect(canvas.style.position).toBe("");
    expect(canvas.style.width).toBe("100%");
    expect(canvas.style.height).toBe("100%");
    surface.dispose();
  });
});
