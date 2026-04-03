import { describe, it, expect } from "vitest";
import {
  enumeratePanes,
  assignSessionsToPanes,
  buildCandidateOrder,
  reconcileAssignments,
  adjustWeights,
  PRESETS,
} from "../bsp/layout";
import { parseDSL } from "../bsp/dsl";
import type { BSPSplit, BSPLeaf } from "../bsp/dsl";

describe("PRESETS", () => {
  it("all presets parse without error", () => {
    for (const preset of PRESETS) {
      expect(preset.root).toBeDefined();
      expect(preset.name).toBeTruthy();
    }
  });
});

describe("enumeratePanes", () => {
  it("returns single pane for a leaf", () => {
    const { root } = parseDSL("shell");
    const panes = enumeratePanes(root);
    expect(panes).toHaveLength(1);
    expect(panes[0].id).toBe("0");
    expect(panes[0].leaf.tag).toBe("shell");
  });

  it("returns panes with dot-separated IDs for a split", () => {
    const { root } = parseDSL("line(a, b)");
    const panes = enumeratePanes(root);
    expect(panes).toHaveLength(2);
    expect(panes[0].id).toBe("0");
    expect(panes[1].id).toBe("1");
  });

  it("generates nested IDs for deep splits", () => {
    const { root } = parseDSL("line(a, col(b, c))");
    const panes = enumeratePanes(root);
    expect(panes).toHaveLength(3);
    expect(panes[0].id).toBe("0");
    expect(panes[1].id).toBe("1.0");
    expect(panes[2].id).toBe("1.1");
  });

  it("handles grid layout", () => {
    const { root } = parseDSL("col(line(a, b), line(c, d))");
    const panes = enumeratePanes(root);
    expect(panes).toHaveLength(4);
    expect(panes.map((p) => p.id)).toEqual(["0.0", "0.1", "1.0", "1.1"]);
  });
});

describe("assignSessionsToPanes", () => {
  it("assigns sessions in order", () => {
    const { root } = parseDSL("line(a, b)");
    const panes = enumeratePanes(root);
    const result = assignSessionsToPanes(panes, ["s1", "s2"]);
    expect(result.assignments["0"]).toBe("s1");
    expect(result.assignments["1"]).toBe("s2");
  });

  it("assigns null when sessions run out", () => {
    const { root } = parseDSL("line(a, b, c)");
    const panes = enumeratePanes(root);
    const result = assignSessionsToPanes(panes, ["s1"]);
    expect(result.assignments["0"]).toBe("s1");
    expect(result.assignments["1"]).toBeNull();
    expect(result.assignments["2"]).toBeNull();
  });

  it("skips panes with commands", () => {
    const { root } = parseDSL('line(shell="htop", editor)');
    const panes = enumeratePanes(root);
    const result = assignSessionsToPanes(panes, ["s1"]);
    expect(result.assignments["0"]).toBeNull();
    expect(result.assignments["1"]).toBe("s1");
  });

  it("handles zero sessions", () => {
    const { root } = parseDSL("line(a, b)");
    const panes = enumeratePanes(root);
    const result = assignSessionsToPanes(panes, []);
    expect(result.assignments["0"]).toBeNull();
    expect(result.assignments["1"]).toBeNull();
  });
});

describe("buildCandidateOrder", () => {
  it("puts focused session first", () => {
    const order = buildCandidateOrder({
      liveSessionIds: ["a", "b", "c"],
      focusedSessionId: "b",
    });
    expect(order[0]).toBe("b");
    expect(order).toContain("a");
    expect(order).toContain("c");
  });

  it("deduplicates across sources", () => {
    const order = buildCandidateOrder({
      liveSessionIds: ["a", "b"],
      focusedSessionId: "a",
      currentAssignedInPaneOrder: ["a", "b"],
      lruSessionIds: ["b", "a"],
    });
    expect(order).toEqual(["a", "b"]);
  });

  it("excludes focused session if not live", () => {
    const order = buildCandidateOrder({
      liveSessionIds: ["a"],
      focusedSessionId: "dead",
    });
    expect(order).toEqual(["a"]);
  });

  it("returns empty for empty inputs", () => {
    const order = buildCandidateOrder({
      liveSessionIds: [],
      focusedSessionId: null,
    });
    expect(order).toEqual([]);
  });

  it("preserves LRU order after focused and current", () => {
    const order = buildCandidateOrder({
      liveSessionIds: ["a", "b", "c", "d"],
      focusedSessionId: null,
      currentAssignedInPaneOrder: ["c"],
      lruSessionIds: ["d", "b"],
    });
    expect(order).toEqual(["c", "d", "b", "a"]);
  });
});

describe("reconcileAssignments", () => {
  it("keeps live sessions", () => {
    const { root } = parseDSL("line(a, b)");
    const panes = enumeratePanes(root);
    const result = reconcileAssignments({
      panes,
      previous: { assignments: { "0": "s1", "1": "s2" } },
      liveSessionIds: ["s1", "s2"],
      knownSessionIds: ["s1", "s2"],
    });
    expect(result.assignments["0"]).toBe("s1");
    expect(result.assignments["1"]).toBe("s2");
  });

  it("nulls out dead known sessions", () => {
    const { root } = parseDSL("line(a, b)");
    const panes = enumeratePanes(root);
    const result = reconcileAssignments({
      panes,
      previous: { assignments: { "0": "s1", "1": "s2" } },
      liveSessionIds: ["s1"],
      knownSessionIds: ["s1", "s2"],
    });
    expect(result.assignments["0"]).toBe("s1");
    expect(result.assignments["1"]).toBeNull();
  });

  it("retains unknown sessions", () => {
    const { root } = parseDSL("shell");
    const panes = enumeratePanes(root);
    const result = reconcileAssignments({
      panes,
      previous: { assignments: { "0": "unknown-id" } },
      liveSessionIds: [],
      knownSessionIds: ["other"],
    });
    expect(result.assignments["0"]).toBe("unknown-id");
  });

  it("handles pane not in previous assignments", () => {
    const { root } = parseDSL("line(a, b)");
    const panes = enumeratePanes(root);
    const result = reconcileAssignments({
      panes,
      previous: { assignments: {} },
      liveSessionIds: ["s1"],
      knownSessionIds: ["s1"],
    });
    expect(result.assignments["0"]).toBeNull();
    expect(result.assignments["1"]).toBeNull();
  });
});

describe("adjustWeights", () => {
  function makeSplit(): BSPSplit {
    return {
      type: "split",
      direction: "horizontal",
      children: [
        { node: { type: "leaf", tag: "a" }, weight: 1 },
        { node: { type: "leaf", tag: "b" }, weight: 1 },
      ],
    };
  }

  it("adjusts weights by fraction", () => {
    const split = makeSplit();
    const result = adjustWeights(split, 0, 1, 0.25);
    expect(result.children[0].weight).toBe(1.5);
    expect(result.children[1].weight).toBe(0.5);
  });

  it("does not mutate the original", () => {
    const split = makeSplit();
    adjustWeights(split, 0, 1, 0.25);
    expect(split.children[0].weight).toBe(1);
    expect(split.children[1].weight).toBe(1);
  });

  it("clamps to minimum weight", () => {
    const split = makeSplit();
    const result = adjustWeights(split, 0, 1, 0.99);
    expect(result.children[1].weight).toBe(0.1);
  });

  it("zero fraction produces no change", () => {
    const split = makeSplit();
    const result = adjustWeights(split, 0, 1, 0);
    expect(result.children[0].weight).toBe(1);
    expect(result.children[1].weight).toBe(1);
  });

  it("negative fraction grows B and shrinks A", () => {
    const split = makeSplit();
    const result = adjustWeights(split, 0, 1, -0.25);
    expect(result.children[0].weight).toBe(0.5);
    expect(result.children[1].weight).toBe(1.5);
  });

  it("preserves other children unchanged", () => {
    const split: BSPSplit = {
      type: "split",
      direction: "horizontal",
      children: [
        { node: { type: "leaf", tag: "a" }, weight: 1 },
        { node: { type: "leaf", tag: "b" }, weight: 1 },
        { node: { type: "leaf", tag: "c" }, weight: 2 },
      ],
    };
    const result = adjustWeights(split, 0, 1, 0.1);
    expect(result.children[2].weight).toBe(2);
  });
});
