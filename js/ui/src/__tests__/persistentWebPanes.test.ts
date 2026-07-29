import { describe, expect, it } from "vitest";
import { selectWebPaneHost } from "../webPaneHostSelection";

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
