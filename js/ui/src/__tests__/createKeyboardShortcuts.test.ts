import { describe, expect, it } from "vitest";
import type { BSPAssignments } from "@blit-sh/core";
import { surfaceAssignment } from "../bsp/layout";
import {
  hasFocusedWaylandSurface,
  shouldHandleNewTerminalShortcut,
} from "../createKeyboardShortcuts";

function focusState(options: {
  surfaceId?: number | null;
  paneId?: string | null;
  assignment?: string | null;
}) {
  const paneId = options.paneId ?? null;
  const assignments = paneId
    ? ({
        assignments: { [paneId]: options.assignment ?? null },
      } as BSPAssignments)
    : null;
  return {
    focusedSurfaceId: () => options.surfaceId ?? null,
    bspFocusedPaneId: () => paneId,
    layoutAssignments: () => assignments,
  };
}

describe("hasFocusedWaylandSurface", () => {
  it("detects a standalone focused surface", () => {
    const state = focusState({ surfaceId: 7 });
    expect(hasFocusedWaylandSurface(state)).toBe(true);
    expect(shouldHandleNewTerminalShortcut(state)).toBe(false);
  });

  it("detects a surface assigned to the focused BSP pane", () => {
    const state = focusState({
      paneId: "pane-1",
      assignment: surfaceAssignment("connection-1", 7),
    });
    expect(hasFocusedWaylandSurface(state)).toBe(true);
    expect(shouldHandleNewTerminalShortcut(state)).toBe(false);
  });

  it("does not treat terminal assignments as Wayland surfaces", () => {
    const state = focusState({ paneId: "pane-1", assignment: "session-1" });
    expect(hasFocusedWaylandSurface(state)).toBe(false);
    expect(shouldHandleNewTerminalShortcut(state)).toBe(true);
  });
});
