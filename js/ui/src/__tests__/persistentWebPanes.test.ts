import { describe, expect, it } from "vitest";
import { clipInsetsFor } from "../webPaneClipping";
import { selectWebPaneHost } from "../webPaneHostSelection";

describe("clipInsetsFor", () => {
  const host = { top: 100, right: 300, bottom: 300, left: 100 };

  it("does not clip a host inside its ancestors", () => {
    expect(
      clipInsetsFor(host, [{ top: 0, right: 400, bottom: 400, left: 0 }]),
    ).toEqual({ top: 0, right: 0, bottom: 0, left: 0 });
  });

  it("clips portions outside multiple ancestor viewports", () => {
    expect(
      clipInsetsFor(host, [
        { top: 150, right: 400, bottom: 400, left: 0 },
        { top: 0, right: 250, bottom: 260, left: 120 },
      ]),
    ).toEqual({ top: 50, right: 50, bottom: 40, left: 20 });
  });

  it("fully clips a host outside an ancestor viewport", () => {
    expect(
      clipInsetsFor(host, [{ top: 400, right: 500, bottom: 500, left: 400 }]),
    ).toEqual({ top: 200, right: 0, bottom: 0, left: 200 });
  });
});

describe("selectWebPaneHost", () => {
  it("prefers a focused foreground host", () => {
    const dock = { id: "dock", interactive: false, focused: false };
    const pane = { id: "pane", interactive: true, focused: true };
    expect(selectWebPaneHost([dock, pane])).toBe(pane);
  });

  it("prefers an interactive host over a dock host", () => {
    const dock = { id: "dock", interactive: false, focused: false };
    const pane = { id: "pane", interactive: true, focused: false };
    expect(selectWebPaneHost([dock, pane])).toBe(pane);
  });

  it("falls back to the dock and handles no hosts", () => {
    const dock = { id: "dock", interactive: false, focused: false };
    expect(selectWebPaneHost([dock])).toBe(dock);
    expect(selectWebPaneHost([])).toBeNull();
  });
});
