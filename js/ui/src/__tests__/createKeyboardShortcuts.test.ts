import { describe, expect, it } from "vitest";
import type { BSPAssignments } from "@blit-sh/core";
import { surfaceAssignment } from "../bsp/layout";
import {
  createMacDeadKeyHandler,
  hasFocusedWaylandSurface,
  isSwitcherShortcut,
  nextCycleTarget,
  shouldHandleNewTerminalShortcut,
  shouldForwardClosingSwitcherCtrlK,
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

describe("Ctrl-K switcher toggle", () => {
  it("opens for Ctrl-K as well as Cmd-K on every platform", () => {
    expect(
      isSwitcherShortcut({
        ctrlKey: true,
        metaKey: false,
        shiftKey: false,
        key: "k",
      }),
    ).toBe(true);
    expect(
      isSwitcherShortcut({
        ctrlKey: false,
        metaKey: true,
        shiftKey: false,
        key: "k",
      }),
    ).toBe(true);
  });

  it("forwards the second Ctrl-K to the focused pane", () => {
    expect(
      shouldForwardClosingSwitcherCtrlK("expose", {
        ctrlKey: true,
        metaKey: false,
      }),
    ).toBe(true);
  });

  it("does not forward the opening chord or Cmd-K", () => {
    expect(
      shouldForwardClosingSwitcherCtrlK(null, {
        ctrlKey: true,
        metaKey: false,
      }),
    ).toBe(false);
    expect(
      shouldForwardClosingSwitcherCtrlK("expose", {
        ctrlKey: false,
        metaKey: true,
      }),
    ).toBe(false);
  });
});

describe("macOS dead-key fallback", () => {
  const key = (init: KeyboardEventInit) =>
    new KeyboardEvent("keydown", {
      bubbles: true,
      cancelable: true,
      ...init,
    });

  it.each([
    ["KeyE", "e", "é", "´"],
    ["KeyE", "E", "É", "´"],
    ["Backquote", "a", "à", "`"],
    ["KeyI", "o", "ô", "ˆ"],
    ["KeyN", "n", "ñ", "˜"],
    ["KeyU", "u", "ü", "¨"],
    ["KeyE", " ", "´", "´"],
  ])("composes %s then %s as %s", (code, next, expected, preedit) => {
    const input = document.createElement("input");
    document.body.append(input);
    input.focus();
    const events: string[] = [];
    input.addEventListener("compositionstart", () => events.push("start"));
    input.addEventListener("compositionupdate", () => events.push("update"));
    input.addEventListener("input", () => events.push("input"));
    input.addEventListener("compositionend", () => events.push("end"));
    const handle = createMacDeadKeyHandler(true);
    window.addEventListener("keydown", handle, true);

    try {
      const dead = key({ key: "Dead", code, altKey: true });
      input.dispatchEvent(dead);
      expect(input.value).toBe(preedit);
      const completing = key({
        key: next,
        code: next === " " ? "Space" : "KeyE",
      });
      input.dispatchEvent(completing);

      expect(dead.defaultPrevented).toBe(true);
      expect(completing.defaultPrevented).toBe(true);
      expect(input.value).toBe(expected);
      expect(events).toEqual([
        "start",
        "update",
        "input",
        "update",
        "input",
        "end",
      ]);
    } finally {
      window.removeEventListener("keydown", handle, true);
      input.remove();
    }
  });

  // A key that has no precomposed form with the accent must never leave the
  // combining mark in the field: NFC not composing means the pair is not a
  // character.  macOS commits the spacing accent and then the key instead.
  it.each([
    ["q", "´q"],
    ["h", "´h"],
    ["´", "´"],
  ])("does not compose %s into a bare combining mark", (next, expected) => {
    const input = document.createElement("input");
    document.body.append(input);
    input.focus();
    const handle = createMacDeadKeyHandler(true);
    window.addEventListener("keydown", handle, true);

    try {
      input.dispatchEvent(key({ key: "Dead", code: "KeyE", altKey: true }));
      input.dispatchEvent(key({ key: next, code: "KeyQ" }));
      expect(input.value).toBe(expected);
      expect(input.value).not.toContain("́");
    } finally {
      window.removeEventListener("keydown", handle, true);
      input.remove();
    }
  });

  // The range is recorded on the dead key and replayed on the next one, so
  // anything that writes to the field in between (a native composition the
  // page does not own) makes it point at text the handler never inserted.
  it("leaves the field alone when the recorded range moved", () => {
    const input = document.createElement("input");
    document.body.append(input);
    input.focus();
    const handle = createMacDeadKeyHandler(true);
    window.addEventListener("keydown", handle, true);

    try {
      input.dispatchEvent(key({ key: "Dead", code: "KeyE", altKey: true }));
      expect(input.value).toBe("´");
      input.value = "x´";
      const completing = key({ key: "e", code: "KeyE" });
      input.dispatchEvent(completing);
      expect(input.value).toBe("x´");
      expect(completing.defaultPrevented).toBe(false);
    } finally {
      window.removeEventListener("keydown", handle, true);
      input.remove();
    }
  });

  // Chromium's own IME is not cancelled by preventDefault(), so where the
  // native dead key works the accent would otherwise be inserted twice.
  it("retracts the synthesized preedit when a native composition starts", () => {
    const input = document.createElement("textarea");
    document.body.append(input);
    input.focus();
    const handle = createMacDeadKeyHandler(true);
    window.addEventListener("keydown", handle, true);

    try {
      input.dispatchEvent(key({ key: "Dead", code: "KeyE", altKey: true }));
      expect(input.value).toBe("´");

      input.dispatchEvent(
        new CompositionEvent("compositionstart", { bubbles: true, data: "" }),
      );
      expect(input.value).toBe("");

      // The native composition owns the field now: the completing key must
      // reach the browser instead of being composed a second time.
      const completing = key({ key: "e", code: "KeyE" });
      input.dispatchEvent(completing);
      expect(completing.defaultPrevented).toBe(false);
      expect(input.value).toBe("");
    } finally {
      window.removeEventListener("keydown", handle, true);
      input.remove();
    }
  });

  it("cancels a pending accent with Backspace", () => {
    const input = document.createElement("textarea");
    document.body.append(input);
    input.focus();
    const commits: string[] = [];
    input.addEventListener("compositionend", (event) =>
      commits.push(event.data),
    );
    const handle = createMacDeadKeyHandler(true);
    window.addEventListener("keydown", handle, true);

    try {
      input.dispatchEvent(key({ key: "Dead", code: "KeyE", altKey: true }));
      input.dispatchEvent(key({ key: "Backspace", code: "Backspace" }));
      expect(input.value).toBe("");
      expect(commits).toEqual([""]);
    } finally {
      window.removeEventListener("keydown", handle, true);
      input.remove();
    }
  });

  it("streams a marked preedit before committing to a surface-style target", () => {
    const input = document.createElement("textarea");
    document.body.append(input);
    input.focus();
    const preedits: string[] = [];
    const commits: string[] = [];
    input.addEventListener("input", (event) => {
      if (event.isComposing) preedits.push(input.value);
    });
    input.addEventListener("compositionend", (event) =>
      commits.push(event.data),
    );
    const handle = createMacDeadKeyHandler(true);
    window.addEventListener("keydown", handle, true);

    try {
      input.dispatchEvent(key({ key: "Dead", code: "KeyE", altKey: true }));
      expect(preedits).toEqual(["´"]);
      expect(commits).toEqual([]);

      input.dispatchEvent(key({ key: "e", code: "KeyE" }));
      expect(preedits).toEqual(["´", "é"]);
      expect(commits).toEqual(["é"]);
    } finally {
      window.removeEventListener("keydown", handle, true);
      input.remove();
    }
  });

  it("does nothing off macOS or for ordinary Option characters", () => {
    const disabled = createMacDeadKeyHandler(false);
    const dead = key({ key: "Dead", code: "KeyE", altKey: true });
    expect(disabled(dead)).toBe(false);
    expect(dead.defaultPrevented).toBe(false);

    const enabled = createMacDeadKeyHandler(true);
    const ellipsis = key({ key: "…", code: "Semicolon", altKey: true });
    expect(enabled(ellipsis)).toBe(false);
    expect(ellipsis.defaultPrevented).toBe(false);
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
