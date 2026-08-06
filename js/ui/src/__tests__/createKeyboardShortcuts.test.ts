import { describe, expect, it } from "vitest";
import type { BSPAssignments } from "@blit-sh/core";
import { surfaceAssignment } from "../bsp/layout";
import {
  hasFocusedWaylandSurface,
  nextCycleTarget,
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

describe("nextCycleTarget", () => {
  // A terminal, a surface, and two tabs: the chord has to reach all of them,
  // which is the whole point of the ring.
  const surface = surfaceAssignment("connection-1", 7);
  const ring = ["session-1", surface, "editor:connection-1:/a.ts", "web:c:1"];

  it("walks every kind, forwards and backwards", () => {
    expect(nextCycleTarget(ring, "session-1", 1)).toBe(surface);
    expect(nextCycleTarget(ring, surface, 1)).toBe("editor:connection-1:/a.ts");
    expect(nextCycleTarget(ring, "editor:connection-1:/a.ts", 1)).toBe(
      "web:c:1",
    );
    expect(nextCycleTarget(ring, "web:c:1", -1)).toBe(
      "editor:connection-1:/a.ts",
    );
    expect(nextCycleTarget(ring, surface, -1)).toBe("session-1");
  });

  it("wraps at both ends", () => {
    expect(nextCycleTarget(ring, "web:c:1", 1)).toBe("session-1");
    expect(nextCycleTarget(ring, "session-1", -1)).toBe("web:c:1");
  });

  it("enters at the near end when nothing is focused", () => {
    expect(nextCycleTarget(ring, null, 1)).toBe("session-1");
    expect(nextCycleTarget(ring, null, -1)).toBe("web:c:1");
    // A focused thing that is not in the ring (mid-teardown) is the same case.
    expect(nextCycleTarget(ring, "session-gone", 1)).toBe("session-1");
  });

  it("skips what another BSP pane is already showing", () => {
    const elsewhere = new Set([surface, "editor:connection-1:/a.ts"]);
    expect(nextCycleTarget(ring, "session-1", 1, elsewhere)).toBe("web:c:1");
    expect(nextCycleTarget(ring, "web:c:1", 1, elsewhere)).toBe("session-1");
  });

  it("stays put when the focused thing is the only candidate", () => {
    expect(nextCycleTarget(["session-1"], "session-1", 1)).toBeNull();
    const elsewhere = new Set([surface, "editor:connection-1:/a.ts"]);
    expect(
      nextCycleTarget(["session-1", ...elsewhere], "session-1", 1, elsewhere),
    ).toBeNull();
  });

  it("has nothing to move to when the ring is empty", () => {
    expect(nextCycleTarget([], null, 1)).toBeNull();
    expect(nextCycleTarget(ring, null, 1, new Set(ring))).toBeNull();
  });
});
