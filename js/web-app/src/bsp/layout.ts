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
}

// ---------------------------------------------------------------------------
// Presets
// ---------------------------------------------------------------------------

function preset(name: string, dsl: string): BSPLayout {
  return { name, dsl, ...parseDSL(dsl) };
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

// ---------------------------------------------------------------------------
// Pane enumeration and assignment
// ---------------------------------------------------------------------------

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
  orderedSessionIds: readonly SessionId[],
): BSPAssignments {
  const assignments: Record<string, SessionId | null> = {};
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
  liveSessionIds: readonly SessionId[];
  focusedSessionId: SessionId | null;
  currentAssignedInPaneOrder?: readonly SessionId[];
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
  lruSessionIds.forEach(push);
  liveSessionIds.forEach(push);

  return ordered;
}

/**
 * Keep existing assignments, clearing only sessions the server confirms are
 * gone (known to the server but no longer live).  Sessions the server hasn't
 * mentioned yet are preserved — they may still be loading on reload.
 */
export function reconcileAssignments({
  panes,
  previous,
  liveSessionIds,
  knownSessionIds,
}: {
  panes: readonly BSPPane[];
  previous: BSPAssignments;
  liveSessionIds: readonly SessionId[];
  /** All session IDs the server has told us about (any state). */
  knownSessionIds: readonly SessionId[];
}): BSPAssignments {
  const live = new Set(liveSessionIds);
  const known = new Set(knownSessionIds);
  const assignments: Record<string, SessionId | null> = {};

  for (const pane of panes) {
    const sessionId = previous.assignments[pane.id];
    // Live → keep.  Not yet known to server → keep (still loading).
    // Known but not live → dead, clear.
    const keep =
      sessionId != null && (live.has(sessionId) || !known.has(sessionId));
    assignments[pane.id] = keep ? sessionId : null;
  }

  return { assignments };
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

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

import { readStorage, writeStorage } from "../storage";

const LAYOUT_KEY = "blit.layout";
const LAYOUT_HISTORY_KEY = "blit.layouts";
type StoredRecentLayout = string | { name: string; dsl: string };

function parseHash(): Record<string, string> {
  const hash = typeof location !== "undefined" ? location.hash.slice(1) : "";
  if (!hash) return {};
  const result: Record<string, string> = {};
  for (const part of hash.split("&")) {
    const eq = part.indexOf("=");
    if (eq > 0)
      result[decodeURIComponent(part.slice(0, eq))] = decodeURIComponent(
        part.slice(eq + 1),
      );
  }
  return result;
}

function layoutFromDSLString(dsl: string, name?: string): BSPLayout | null {
  try {
    const { root, weight } = parseDSL(dsl);
    return { name: name ?? dsl, dsl, root, weight };
  } catch {
    return null;
  }
}

export function loadActiveLayout(): BSPLayout | null {
  // Prefer layout from URL hash.
  const hash = parseHash();
  if (hash.l) {
    const layout = layoutFromDSLString(hash.l);
    if (layout) return layout;
  }

  try {
    const raw = readStorage(LAYOUT_KEY);
    if (!raw) return null;
    const saved = JSON.parse(raw) as { name: string; dsl: string };
    return layoutFromDSLString(saved.dsl, saved.name);
  } catch {
    const dsl = readStorage(LAYOUT_KEY);
    return dsl ? layoutFromDSLString(dsl) : null;
  }
}

export function loadFocusedPaneFromHash(): string | null {
  return parseHash().p || null;
}

export function loadAssignmentsFromHash(): Record<string, string> | null {
  const a = parseHash().a;
  if (!a) return null;
  const result: Record<string, string> = {};
  for (const pair of a.split(",")) {
    const colon = pair.indexOf(":");
    if (colon > 0) {
      result[pair.slice(0, colon)] = pair.slice(colon + 1);
    }
  }
  return Object.keys(result).length > 0 ? result : null;
}

export function saveActiveLayout(layout: BSPLayout | null): void {
  if (layout) {
    writeStorage(
      LAYOUT_KEY,
      JSON.stringify({ name: layout.name, dsl: layout.dsl }),
    );
  } else {
    try { localStorage.removeItem(LAYOUT_KEY); } catch {}
  }
}

export function saveToHistory(layout: BSPLayout | string): void {
  pushRecentLayout(layout);
}

export function loadRecentLayouts(): BSPLayout[] {
  try {
    const raw = readStorage(LAYOUT_HISTORY_KEY);
    if (!raw) return [];
    const stored: StoredRecentLayout[] = JSON.parse(raw);
    return stored.flatMap((entry) => {
      const record =
        typeof entry === "string" ? { name: entry, dsl: entry } : entry;
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
  const record =
    typeof layout === "string"
      ? { name: layout, dsl: layout }
      : { name: layout.name, dsl: layout.dsl };
  try {
    const raw = readStorage(LAYOUT_HISTORY_KEY);
    const existing: StoredRecentLayout[] = raw ? JSON.parse(raw) : [];
    const next = [
      record,
      ...existing.filter((entry) => {
        const dsl = typeof entry === "string" ? entry : entry.dsl;
        return dsl !== record.dsl;
      }),
    ].slice(0, 10);
    writeStorage(LAYOUT_HISTORY_KEY, JSON.stringify(next));
  } catch {}
}

// ---------------------------------------------------------------------------
// Layout from DSL string
// ---------------------------------------------------------------------------

export function layoutFromDSL(dsl: string): BSPLayout {
  const { root, weight } = parseDSL(dsl);
  return { name: dsl, dsl, root, weight };
}
