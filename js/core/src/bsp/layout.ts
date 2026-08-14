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

// IDE tiles (docs/ide-plan.md PR-6/7): non-session, non-surface pane content
// — a CodeMirror editor or a git diff — dispatched by assignment shape like
// surfaces. The argument is a filesystem path, so it may contain ":" and "/":
// the parser splits only the leading "<kind>:<conn>:" and keeps the rest
// verbatim (unlike the surface parser's lastIndexOf, which would corrupt it).
const EDITOR_PREFIX = "editor:";
const DIFF_PREFIX = "diff:";
const COMMIT_PREFIX = "commit:";
const PREVIEW_PREFIX = "preview:";

/** BSP assignment for an editor tile: "editor:<connectionId>:<path>". */
export function editorAssignment(connectionId: string, path: string): string {
  return `${EDITOR_PREFIX}${connectionId}:${path}`;
}

/** BSP assignment for a rendered preview of a file:
 *  "preview:<connectionId>:<path>". Same shape as an editor tile — it is
 *  the same file, shown rendered instead of as source, and the view
 *  switcher flips between them. */
export function previewAssignment(connectionId: string, path: string): string {
  return `${PREVIEW_PREFIX}${connectionId}:${path}`;
}

/** BSP assignment for a git diff tile: "diff:<connectionId>:<path>" for the
 *  unstaged (INDEX×WORKTREE) diff, or ":staged:<path>" for the staged
 *  (HEAD×INDEX) diff. `path` is absolute (starts with "/"), so the "staged:"
 *  marker is unambiguous. */
/** Which endpoints a diff tile compares.
 *  - "unstaged":  INDEX×WORKTREE (tracked, unstaged edits)
 *  - "staged":    HEAD×INDEX (git diff --cached)
 *  - "untracked": INDEX×WORKTREE + untracked walk (a new file, shown added)
 *  - "worktree":  HEAD×WORKTREE (all changes since HEAD, staged + unstaged) */
export type DiffSide = "unstaged" | "staged" | "untracked" | "worktree";

export function diffAssignment(
  connectionId: string,
  path: string,
  side: DiffSide = "unstaged",
): string {
  const prefix = side === "unstaged" ? "" : `${side}:`;
  return `${DIFF_PREFIX}${connectionId}:${prefix}${path}`;
}

/** Decode a diff tile's arg into { side, staged, path }. `staged` is kept as a
 *  convenience alias for `side === "staged"`. */
export function parseDiffArg(arg: string): {
  side: DiffSide;
  staged: boolean;
  path: string;
} {
  for (const side of ["staged", "untracked", "worktree"] as const) {
    const prefix = `${side}:`;
    if (arg.startsWith(prefix)) {
      return {
        side,
        staged: side === "staged",
        path: arg.slice(prefix.length),
      };
    }
  }
  return { side: "unstaged", staged: false, path: arg };
}

/** BSP assignment for a commit tile: "commit:<connectionId>:<oid>:<repoPath>".
 *  `oid` is hex (no ":"), so the first ":" of the arg splits oid from repo. */
export function commitAssignment(
  connectionId: string,
  oid: string,
  repoPath: string,
): string {
  return `${COMMIT_PREFIX}${connectionId}:${oid}:${repoPath}`;
}

/** True when the assignment is an editor/diff/commit tile (not a session). */
export function isTileAssignment(value: string | null): boolean {
  return (
    value != null &&
    (value.startsWith(EDITOR_PREFIX) ||
      value.startsWith(DIFF_PREFIX) ||
      value.startsWith(COMMIT_PREFIX) ||
      value.startsWith(PREVIEW_PREFIX))
  );
}

/** True when the assignment names pane content rather than a terminal session
 *  — a surface, an IDE tile, or a web pane. Anything that answers true here
 *  must be kept out of session assignment and focus bookkeeping. */
export function isContentAssignment(value: string | null): boolean {
  return (
    isSurfaceAssignment(value) ||
    isTileAssignment(value) ||
    isWebAssignment(value)
  );
}

export interface TileAssignment {
  kind: "editor" | "diff" | "commit" | "preview";
  connectionId: string;
  /** Verbatim argument (a path, or "<oid>:<repoPath>" for commit). */
  arg: string;
}

/** Parse an editor/diff/commit tile assignment, or null. */
export function parseTileAssignment(
  value: string | null,
): TileAssignment | null {
  let kind: TileAssignment["kind"];
  let prefix: string;
  if (value != null && value.startsWith(EDITOR_PREFIX)) {
    kind = "editor";
    prefix = EDITOR_PREFIX;
  } else if (value != null && value.startsWith(DIFF_PREFIX)) {
    kind = "diff";
    prefix = DIFF_PREFIX;
  } else if (value != null && value.startsWith(COMMIT_PREFIX)) {
    kind = "commit";
    prefix = COMMIT_PREFIX;
  } else if (value != null && value.startsWith(PREVIEW_PREFIX)) {
    kind = "preview";
    prefix = PREVIEW_PREFIX;
  } else {
    return null;
  }
  const rest = value.slice(prefix.length);
  const colon = rest.indexOf(":");
  if (colon <= 0) return null;
  return {
    kind,
    connectionId: rest.slice(0, colon),
    arg: rest.slice(colon + 1),
  };
}

// ---------------------------------------------------------------------------
// Web pane assignment helpers
// ---------------------------------------------------------------------------

// A web pane is an iframe onto something the server can reach — a dev server,
// an internal dashboard — served through the preview service worker
// (docs/design/net.md). Same dispatch-by-assignment-shape as surfaces and IDE
// tiles. The argument is a URL, so it contains ":" and "/" and the parser
// splits only the leading "web:<conn>:" and keeps the rest verbatim.
const WEB_PREFIX = "web:";

/** BSP assignment for a web pane: "web:<connectionId>:<url>". */
export function webAssignment(connectionId: string, url: string): string {
  return `${WEB_PREFIX}${connectionId}:${url}`;
}

/** Check whether a BSP assignment value represents a web pane. */
export function isWebAssignment(value: string | null): boolean {
  return value != null && value.startsWith(WEB_PREFIX);
}

/** Parse a web pane assignment into its connection and URL, or null. */
export function parseWebAssignment(
  value: string | null,
): { connectionId: string; url: string } | null {
  if (value == null || !value.startsWith(WEB_PREFIX)) return null;
  const rest = value.slice(WEB_PREFIX.length);
  const colon = rest.indexOf(":");
  if (colon <= 0) return null;
  const url = rest.slice(colon + 1);
  if (!url) return null;
  return { connectionId: rest.slice(0, colon), url };
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

/**
 * The assignments after a dropped `value` lands in `targetPaneId`, or `null`
 * when nothing changes.
 *
 * A drop that names the pane the drag left (`fromPaneId` — a pane's ✕
 * doubling as its drag handle) is a *move*, not another open: the source
 * pane takes what the target held, so the content lands in exactly one pane,
 * and dropping on an empty pane is a plain move. Gated on the source still
 * holding the dragged value — a layout change mid-drag must not evict
 * whatever else got there since.
 *
 * Surface assignments are unique views, so recover their source from the
 * current assignments if a browser omits the secondary source-pane drag MIME
 * (or if that pane id went stale). Generic tile drops deliberately remain
 * copies/opens when they have no valid source marker.
 */
export function assignmentsAfterDrop(
  prev: Readonly<Record<string, string | null>>,
  value: string,
  targetPaneId: string,
  fromPaneId: string | undefined,
  validPaneIds: readonly string[],
): Record<string, string | null> | null {
  const markedSourceIsCurrent =
    fromPaneId !== undefined &&
    fromPaneId !== targetPaneId &&
    validPaneIds.includes(fromPaneId) &&
    prev[fromPaneId] === value;
  const sourcePaneId = markedSourceIsCurrent
    ? fromPaneId
    : isSurfaceAssignment(value)
      ? validPaneIds.find(
          (paneId) => paneId !== targetPaneId && prev[paneId] === value,
        )
      : undefined;
  const swap = sourcePaneId !== undefined;
  if (prev[targetPaneId] === value && !swap) return null;
  const next: Record<string, string | null> = {
    ...prev,
    [targetPaneId]: value,
  };
  if (sourcePaneId !== undefined)
    next[sourcePaneId] = prev[targetPaneId] ?? null;
  return next;
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
