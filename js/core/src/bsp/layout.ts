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

// ---------------------------------------------------------------------------
// Surface assignment helpers
// ---------------------------------------------------------------------------

const SURFACE_PREFIX = "surface:";

/** Create a BSP assignment value representing a compositor surface.
 *  Format: "surface:<connectionId>:<surfaceId>" */
export function surfaceAssignment(
  connectionId: string,
  surfaceId: number,
): string {
  return `${SURFACE_PREFIX}${connectionId}:${surfaceId}`;
}

/** Check whether a BSP assignment value represents a surface. */
export function isSurfaceAssignment(value: string | null): boolean {
  return value != null && value.startsWith(SURFACE_PREFIX);
}

/** Extract the numeric surface ID from a surface assignment string, or null. */
export function parseSurfaceAssignment(
  value: string | null,
): { connectionId: string; surfaceId: number } | null {
  if (value == null || !value.startsWith(SURFACE_PREFIX)) return null;
  const rest = value.slice(SURFACE_PREFIX.length);
  const colon = rest.lastIndexOf(":");
  if (colon <= 0) return null;
  const connectionId = rest.slice(0, colon);
  const n = parseInt(rest.slice(colon + 1), 10);
  return Number.isFinite(n) ? { connectionId, surfaceId: n } : null;
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
  liveSurfaceKeys,
  readyConnectionIds,
  sessionReplacements,
  sessionConnectionIds,
}: {
  panes: readonly BSPPane[];
  previous: BSPAssignments;
  liveSessionIds: readonly string[];
  knownSessionIds: readonly string[];
  /** When provided, surface assignments for destroyed surfaces are cleared.
   *  Each key is "connectionId:surfaceId". */
  liveSurfaceKeys?: readonly string[];
  /** Connections that are both present AND ready.  Surface assignments
   *  whose connection is absent OR not yet ready (reconnecting) are
   *  preserved — the surface may reappear once the connection finishes
   *  its handshake or is re-added. */
  readyConnectionIds?: ReadonlySet<string>;
  /** Maps old (closed) session IDs to replacement live session IDs.
   *  Used to re-map pane assignments after a reconnect where PTYs get
   *  new session IDs but represent the same underlying terminal. */
  sessionReplacements?: ReadonlyMap<string, string>;
  /** Maps session IDs to their owning connection ID.  Used together with
   *  `readyConnectionIds` to preserve terminal assignments whose
   *  connection is absent or still reconnecting — mirroring the surface
   *  assignment protection so terminals survive reconnect cycles too. */
  sessionConnectionIds?: ReadonlyMap<string, string>;
}): BSPAssignments {
  const live = new Set(liveSessionIds);
  const known = new Set(knownSessionIds);
  const liveSurfaces = liveSurfaceKeys ? new Set(liveSurfaceKeys) : null;
  const assignments: Record<string, string | null> = {};

  for (const pane of panes) {
    const value = previous.assignments[pane.id];
    if (isSurfaceAssignment(value)) {
      if (liveSurfaces) {
        const parsed = parseSurfaceAssignment(value);
        const key =
          parsed != null ? `${parsed.connectionId}:${parsed.surfaceId}` : null;
        if (key != null && liveSurfaces.has(key)) {
          // Surface is live — keep.
          assignments[pane.id] = value;
        } else if (
          parsed &&
          readyConnectionIds &&
          !readyConnectionIds.has(parsed.connectionId)
        ) {
          // Surface's connection is absent or still reconnecting —
          // preserve the assignment so it survives until the connection
          // is fully ready (or re-added).
          assignments[pane.id] = value;
        } else {
          // Surface is gone and its connection is present+ready — clear.
          assignments[pane.id] = null;
        }
      } else {
        assignments[pane.id] = value;
      }
      continue;
    }
    if (value != null && !live.has(value)) {
      // The assigned session is gone. Try to replace it with a live
      // session for the same underlying PTY (reconnect gave it a new ID).
      const replacement = sessionReplacements?.get(value);
      if (replacement && live.has(replacement)) {
        assignments[pane.id] = replacement;
        continue;
      }
      // Session's connection is absent or still reconnecting — preserve
      // the assignment so it survives until the connection is fully
      // ready (or re-added), mirroring the surface protection above.
      if (readyConnectionIds && sessionConnectionIds) {
        const connId = sessionConnectionIds.get(value);
        if (connId != null && !readyConnectionIds.has(connId)) {
          assignments[pane.id] = value;
          continue;
        }
      }
    }
    const keep = value != null && (live.has(value) || !known.has(value));
    assignments[pane.id] = keep ? value : null;
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
