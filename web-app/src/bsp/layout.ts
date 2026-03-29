import type { SessionId } from "blit-react";
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
  assignments: Record<string, SessionId | null>;
  overflowSessionIds: SessionId[];
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

export const PRESETS: BSPLayout[] = [
  { name: "Side by side", dsl: "line(left, right)", ...parseDSL("line(left, right)") },
  { name: "2-1 thirds", dsl: "line(main 2, side)", ...parseDSL("line(main 2, side)") },
  { name: "Grid", dsl: "col(line(a, b), line(c, d))", ...parseDSL("col(line(a, b), line(c, d))") },
  { name: "Dev", dsl: "line(editor 2, col(shell, live))", ...parseDSL("line(editor 2, col(shell, live))") },
];

// ---------------------------------------------------------------------------
// Pane enumeration and assignment
// ---------------------------------------------------------------------------

export function enumeratePanes(
  node: BSPNode,
  path: readonly number[] = [],
): BSPPane[] {
  if (node.type === "leaf") {
    return [{
      id: path.length > 0 ? path.join(".") : "0",
      leaf: node,
    }];
  }
  return node.children.flatMap((child, index) => enumeratePanes(child.node, [...path, index]));
}

export function assignSessionsToPanes(
  paneIds: readonly string[],
  orderedSessionIds: readonly SessionId[],
): BSPAssignments {
  const assignments: Record<string, SessionId | null> = {};
  for (let index = 0; index < paneIds.length; index += 1) {
    assignments[paneIds[index]] = orderedSessionIds[index] ?? null;
  }
  return {
    assignments,
    overflowSessionIds: orderedSessionIds.slice(paneIds.length),
  };
}

export function buildCandidateOrder({
  liveSessionIds,
  focusedSessionId,
  currentAssignedInPaneOrder = [],
  overflowSessionIds = [],
  lruSessionIds = [],
}: {
  liveSessionIds: readonly SessionId[];
  focusedSessionId: SessionId | null;
  currentAssignedInPaneOrder?: readonly SessionId[];
  overflowSessionIds?: readonly SessionId[];
  lruSessionIds?: readonly SessionId[];
}): SessionId[] {
  const live = new Set(liveSessionIds);
  const seen = new Set<SessionId>();
  const ordered: SessionId[] = [];

  const push = (sessionId: SessionId | null | undefined) => {
    if (!sessionId || !live.has(sessionId) || seen.has(sessionId)) return;
    seen.add(sessionId);
    ordered.push(sessionId);
  };

  push(focusedSessionId);
  currentAssignedInPaneOrder.forEach(push);
  overflowSessionIds.forEach(push);
  lruSessionIds.forEach(push);
  liveSessionIds.forEach(push);

  return ordered;
}

export function reconcileAssignments({
  paneIds,
  previous,
  liveSessionIds,
  preferredPaneId,
}: {
  paneIds: readonly string[];
  previous: BSPAssignments;
  liveSessionIds: readonly SessionId[];
  preferredPaneId?: string | null;
}): BSPAssignments {
  const live = new Set(liveSessionIds);
  const assignments: Record<string, SessionId | null> = {};
  const assigned = new Set<SessionId>();
  const overflowSessionIds = previous.overflowSessionIds.filter((sessionId) => live.has(sessionId));

  for (const paneId of paneIds) {
    const sessionId = previous.assignments[paneId];
    const next = sessionId && live.has(sessionId) ? sessionId : null;
    assignments[paneId] = next;
    if (next) assigned.add(next);
  }

  for (const sessionId of overflowSessionIds) {
    assigned.add(sessionId);
  }

  const empties = paneIds.filter((paneId) => assignments[paneId] == null);
  if (preferredPaneId) {
    const preferredIndex = empties.indexOf(preferredPaneId);
    if (preferredIndex > 0) {
      const [preferred] = empties.splice(preferredIndex, 1);
      empties.unshift(preferred);
    }
  }
  for (const sessionId of liveSessionIds) {
    if (assigned.has(sessionId)) continue;
    const paneId = empties.shift();
    if (paneId) {
      assignments[paneId] = sessionId;
    } else {
      overflowSessionIds.push(sessionId);
    }
    assigned.add(sessionId);
  }

  return {
    assignments,
    overflowSessionIds,
  };
}

// ---------------------------------------------------------------------------
// Weight adjustment (for resize handles)
// ---------------------------------------------------------------------------

export function adjustWeights(
  split: BSPSplit,
  indexA: number,
  indexB: number,
  fraction: number, // how much of the total to transfer from B to A (can be negative)
): BSPSplit {
  const totalWeight = split.children[indexA].weight + split.children[indexB].weight;
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

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

const LAYOUT_KEY = "blit.layout";
const LAYOUT_HISTORY_KEY = "blit.layouts";
type StoredRecentLayout = string | { name: string; dsl: string };

export function loadActiveLayout(): BSPLayout | null {
  try {
    const raw = localStorage.getItem(LAYOUT_KEY);
    if (!raw) return null;
    const saved = JSON.parse(raw) as { name: string; dsl: string };
    const { root, weight } = parseDSL(saved.dsl);
    return { name: saved.name, dsl: saved.dsl, root, weight };
  } catch {
    // Backwards compat: try plain DSL string
    try {
      const dsl = localStorage.getItem(LAYOUT_KEY);
      if (!dsl) return null;
      const { root, weight } = parseDSL(dsl);
      return { name: dsl, dsl, root, weight };
    } catch {
      return null;
    }
  }
}

export function saveActiveLayout(layout: BSPLayout | null): void {
  if (layout) {
    localStorage.setItem(LAYOUT_KEY, JSON.stringify({ name: layout.name, dsl: layout.dsl }));
  } else {
    localStorage.removeItem(LAYOUT_KEY);
  }
}

export function saveToHistory(layout: BSPLayout | string): void {
  pushRecentLayout(layout);
}

export function loadRecentLayouts(): BSPLayout[] {
  try {
    const raw = localStorage.getItem(LAYOUT_HISTORY_KEY);
    if (!raw) return [];
    const stored: StoredRecentLayout[] = JSON.parse(raw);
    return stored.flatMap((entry) => {
      const record = typeof entry === "string"
        ? { name: entry, dsl: entry }
        : entry;
      try {
        const { root, weight } = parseDSL(record.dsl);
        return [{ name: record.name, dsl: record.dsl, root, weight }];
      } catch {
        return [];
      }
    });
  } catch {
    return [];
  }
}

function pushRecentLayout(layout: BSPLayout | string): void {
  const record = typeof layout === "string"
    ? { name: layout, dsl: layout }
    : { name: layout.name, dsl: layout.dsl };
  try {
    const raw = localStorage.getItem(LAYOUT_HISTORY_KEY);
    const existing: StoredRecentLayout[] = raw ? JSON.parse(raw) : [];
    const next = [
      record,
      ...existing.filter((entry) => {
        const dsl = typeof entry === "string" ? entry : entry.dsl;
        return dsl !== record.dsl;
      }),
    ].slice(0, 10);
    localStorage.setItem(LAYOUT_HISTORY_KEY, JSON.stringify(next));
  } catch {
    // ignore storage errors
  }
}

// ---------------------------------------------------------------------------
// Layout from DSL string
// ---------------------------------------------------------------------------

export function layoutFromDSL(dsl: string): BSPLayout {
  const { root, weight } = parseDSL(dsl);
  return { name: dsl, dsl, root, weight };
}
