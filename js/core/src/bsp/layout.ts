import type { BSPNode, BSPSplit, BSPChild, BSPLeaf } from "./dsl";
import { parseDSL } from "./dsl";

export interface BSPLayout {
  name: string;
  dsl: string;
  root: BSPNode;
  weight: number;
}

export interface BSPPane {
  id: string;
  leaf: BSPLeaf;
}

export interface BSPAssignments {
  assignments: Record<string, string | null>;
}

export const PRESETS: BSPLayout[] = [
  preset("Side by side", "line(left, right)"),
  preset("Tabs", "tabs(a, b, c)"),
  preset("2-1 thirds", "line(main 2, side)"),
  preset("Grid", "col(line(a, b), line(c, d))"),
  preset("Dev", "line(editor 2, col(shell, logs))"),
  preset("Dev + tabs", "line(editor 2, tabs(shell, logs, build))"),
  preset("Split + tabs", "line(tabs(a, b) 2, tabs(c, d))"),
];

function preset(name: string, dsl: string): BSPLayout {
  return { name, dsl, ...parseDSL(dsl) };
}

export function enumeratePanes(
  node: BSPNode,
  path: readonly number[] = [],
): BSPPane[] {
  if (node.type === "leaf") {
    return [
      {
        id: path.length > 0 ? path.join(".") : "0",
        leaf: node,
      },
    ];
  }
  return node.children.flatMap((child, index) =>
    enumeratePanes(child.node, [...path, index]),
  );
}

export function assignSessionsToPanes(
  panes: readonly BSPPane[],
  orderedSessionIds: readonly string[],
): BSPAssignments {
  const assignments: Record<string, string | null> = {};
  let sessionIdx = 0;
  for (const pane of panes) {
    if (pane.leaf.command) {
      assignments[pane.id] = null;
    } else {
      assignments[pane.id] = orderedSessionIds[sessionIdx++] ?? null;
    }
  }
  return { assignments };
}

export function buildCandidateOrder({
  liveSessionIds,
  focusedSessionId,
  currentAssignedInPaneOrder = [],
  lruSessionIds = [],
}: {
  liveSessionIds: readonly string[];
  focusedSessionId: string | null;
  currentAssignedInPaneOrder?: readonly string[];
  lruSessionIds?: readonly string[];
}): string[] {
  const live = new Set(liveSessionIds);
  const seen = new Set<string>();
  const ordered: string[] = [];

  const push = (sessionId: string | null | undefined) => {
    if (!sessionId || !live.has(sessionId) || seen.has(sessionId)) return;
    seen.add(sessionId);
    ordered.push(sessionId);
  };

  push(focusedSessionId);
  currentAssignedInPaneOrder.forEach(push);
  lruSessionIds.forEach(push);
  liveSessionIds.forEach(push);

  return ordered;
}

export function reconcileAssignments({
  panes,
  previous,
  liveSessionIds,
  knownSessionIds,
}: {
  panes: readonly BSPPane[];
  previous: BSPAssignments;
  liveSessionIds: readonly string[];
  knownSessionIds: readonly string[];
}): BSPAssignments {
  const live = new Set(liveSessionIds);
  const known = new Set(knownSessionIds);
  const assignments: Record<string, string | null> = {};

  for (const pane of panes) {
    const sessionId = previous.assignments[pane.id];
    const keep =
      sessionId != null && (live.has(sessionId) || !known.has(sessionId));
    assignments[pane.id] = keep ? sessionId : null;
  }

  return { assignments };
}

export function adjustWeights(
  split: BSPSplit,
  indexA: number,
  indexB: number,
  fraction: number,
): BSPSplit {
  const totalWeight =
    split.children[indexA].weight + split.children[indexB].weight;
  const delta = fraction * totalWeight;
  const minWeight = 0.1;

  const newA = Math.max(minWeight, split.children[indexA].weight + delta);
  const newB = Math.max(minWeight, split.children[indexB].weight - delta);

  const children: BSPChild[] = split.children.map((c, i) => {
    if (i === indexA) return { ...c, weight: newA };
    if (i === indexB) return { ...c, weight: newB };
    return c;
  });

  return { ...split, children };
}

export function layoutFromDSL(dsl: string): BSPLayout {
  const { root, weight } = parseDSL(dsl);
  return { name: dsl, dsl, root, weight };
}
