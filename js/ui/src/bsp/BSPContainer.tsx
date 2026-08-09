import {
  createSignal,
  createEffect,
  createMemo,
  onCleanup,
  untrack,
  batch,
  Show,
  For,
  Index,
} from "solid-js";
import {
  BlitTerminal,
  BlitSurfaceView,
  createBlitWorkspace,
  createBlitSessions,
  createBlitWorkspaceState,
} from "@blit-sh/solid";
import type {
  BlitTerminalSurface,
  SessionId,
  TerminalPalette,
} from "@blit-sh/core";
import type { BSPNode, BSPChild, BSPSplit, BSPLeaf } from "@blit-sh/core/bsp";
import { leafCount, serializeDSL } from "@blit-sh/core/bsp";
import type { BSPAssignments, BSPLayout } from "./layout";
import {
  adjustWeights,
  assignSessionsToPanes,
  assignmentsAfterDrop,
  buildCandidateOrder,
  enumeratePanes,
  loadAssignmentsFromHash,
  loadFocusedPaneFromHash,
  reconcileAssignments,
  saveActiveLayout,
  surfaceAssignment,
  isContentAssignment,
  isSurfaceAssignment,
  isWebAssignment,
  parseSurfaceAssignment,
  parseWebAssignment,
  isTileAssignment,
  parseTileAssignment,
} from "./layout";
import { BlitTile } from "../ide/BlitTile";
import { PaneTools } from "../PaneTools";
import { WebPaneHost, type WebPaneHostRegistrar } from "../WebPaneHost";
import {
  isTileDrag,
  paneDragSource,
  tileDragAssignment,
} from "../ide/tileDrag";
import { resolveTab, isPtyRef } from "../ide/tabRegistry";
import { ResizeHandle } from "./ResizeHandle";
import { BSPTreeContext, useBSPTree, type BSPTreeCtx } from "./treeContext";
import type { Theme } from "../theme";
import { themeFor, ui, uiScale, z } from "../theme";
import { t, tp } from "../i18n";
import { shellCapabilities } from "../shellCapabilities";

// The tree context lives in ./treeContext so its identity survives hot
// reloads of this module (see that file).

function resolveLeafFontSize(leaf: BSPLeaf, baseFontSize: number): number {
  const raw = leaf.fontSize;
  if (raw == null) return baseFontSize;
  let resolved: number;
  if (typeof raw === "number") {
    resolved = raw;
  } else if (raw.endsWith("%")) {
    resolved = Math.round((baseFontSize * parseFloat(raw)) / 100);
  } else if (raw.endsWith("pt")) {
    resolved = Math.round((parseFloat(raw) * 4) / 3);
  } else if (raw.endsWith("px")) {
    resolved = parseFloat(raw);
  } else {
    resolved = baseFontSize;
  }
  return Math.max(6, Math.min(72, Math.round(resolved)));
}

function sameAssignments(left: BSPAssignments, right: BSPAssignments): boolean {
  const leftKeys = Object.keys(left.assignments);
  const rightKeys = Object.keys(right.assignments);
  if (leftKeys.length !== rightKeys.length) return false;
  for (const key of leftKeys) {
    if (left.assignments[key] !== right.assignments[key]) return false;
  }
  return true;
}

/** Resolve a pane id (child-index path, `enumeratePanes` scheme) to the index
 *  path of its leaf, or null when it doesn't name a leaf. */
function leafPath(node: BSPNode, paneId: string): number[] | null {
  if (node.type === "leaf") return paneId === "0" ? [] : null;
  const path = paneId.split(".").map(Number);
  if (path.some((n) => !Number.isInteger(n))) return null;
  let cur: BSPNode = node;
  for (const idx of path) {
    if (cur.type !== "split" || !cur.children[idx]) return null;
    cur = cur.children[idx].node;
  }
  return cur.type === "leaf" ? path : null;
}

/** Return a copy of `node` with the subtree at `path` replaced. */
function replaceNodeAtPath(
  node: BSPNode,
  path: readonly number[],
  replacement: BSPNode,
): BSPNode {
  if (path.length === 0) return replacement;
  if (node.type !== "split") return node;
  const [head, ...rest] = path;
  return {
    ...node,
    children: node.children.map((child, i) =>
      i === head
        ? { ...child, node: replaceNodeAtPath(child.node, rest, replacement) }
        : child,
    ),
  };
}

export function BSPContainer(props: {
  layout: BSPLayout;
  onLayoutChange: (layout: BSPLayout | null) => void;
  connectionId: string;
  connectionLabels?: Map<string, string>;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  /** Surface zoom factor (1 = the pane's DPI alone). Defaults to 1. */
  surfaceZoom?: number;

  focusedSessionId: SessionId | null;
  lruSessionIds: readonly SessionId[];
  /** Live surface keys ("connectionId:surfaceId") for cleanup of dead surface assignments. */
  liveSurfaceKeys?: readonly string[];
  /** Additional session IDs to keep visible (e.g. side panel thumbnails). */
  extraVisibleSessions?: readonly SessionId[];
  manageVisibility?: boolean;
  onAssignmentsChange?: (assignments: BSPAssignments) => void;
  /** Called when hash-based assignment resolution completes (or immediately
   *  if there was nothing to resolve). */
  onAssignmentsResolved?: (resolved: boolean) => void;
  onFocusSession: (id: SessionId | null) => void;
  onCreateInPane?: (
    paneId: string,
    command?: string,
    connectionId?: string,
  ) => void;
  onSwitcher?: () => void;
  onHelp?: () => void;
  /** Called with control functions so the parent can direct pane focus/assignments. */
  onFocusBySession?: (fn: (sessionId: SessionId) => void) => void;
  onFocusPane?: (fn: (paneId: string) => void) => void;
  onMoveSessionToPane?: (
    fn: (sessionId: SessionId, targetPaneId: string) => void,
  ) => void;
  onMoveToPane?: (
    fn: (value: string, targetPaneId: string, fromPaneId?: string) => void,
  ) => void;
  /** Called with a function that splits a pane, placing `value` in a new
   *  pane beside the target's current occupant (which is preserved). */
  onSplitPane?: (fn: (value: string, targetPaneId: string) => void) => void;
  onClearPaneAssignment?: (fn: (paneId: string) => void) => void;
  onFocusedPaneChange?: (paneId: string | null) => void;
  onRender?: (renderMs?: number) => void;
  /** Receives each terminal pane's surface as it mounts, so hyperlink hover
   *  and activation work in every split. */
  onTerminalSurface?: (surface: BlitTerminalSurface | null) => void;
  /** Open an IDE tile from within a tile (commit view → editor). */
  onOpenTile?: (assignment: string) => void;
  /** Register visual hosts for Workspace-owned persistent web panes. */
  registerWebPaneHost?: WebPaneHostRegistrar;
  /** Drop a dragged IDE tile assignment into a specific pane. */
  onDropTile?: (
    assignment: string,
    paneId: string,
    sourcePaneId?: string,
  ) => void;
  /** Coarse pointer — keeps each pane's ✕ visible without a hover. */
  isMobileTouch?: boolean;
  /** Whether a session's connection is read-only (see BSPTreeCtx). */
  isSessionReadOnly?: (sessionId: string) => boolean;
  /** Close an IDE/web tab host-wide (Workspace owns the tab registry). */
  onCloseTab?: (assignment: string) => void;
}) {
  const workspace = createBlitWorkspace();
  const workspaceState = createBlitWorkspaceState(workspace);
  const sessions = createBlitSessions(workspace);

  const connection = createMemo(() => {
    const snap = workspaceState();
    return snap.connections.find((c) => c.id === props.connectionId) ?? null;
  });
  // Include "authenticating" so reconciliation can run during the S2C_HELLO →
  // S2C_READY handshake window.  The per-connection `readyConnectionIds`
  // filter inside reconcileAssignments preserves assignments for connections
  // that haven't completed the handshake, so this is safe and lets surfaces
  // propagate to the UI (e.g. PreviewPanel) before S2C_READY arrives.
  const connected = () => {
    const status = connection()?.status;
    return status === "connected" || status === "authenticating";
  };

  const liveSessions = createMemo(() =>
    sessions().filter((session) => session.state !== "closed"),
  );
  const liveSessionIds = createMemo(() =>
    liveSessions().map((session) => session.id),
  );

  const [root, setRoot] = createSignal(props.layout.root);
  const panes = createMemo(() => enumeratePanes(root()));
  const paneIds = createMemo(() => panes().map((pane) => pane.id));

  // Saved assignments store connectionId:ptyId pairs. We resolve them to
  // session IDs once sessions arrive from the server.
  // Prefer hash (shareable URLs), fall back to localStorage (survives new tabs).
  let pendingHash: Record<string, string> | null = loadAssignmentsFromHash();
  // Reactive flag so that effects depending on pendingHash being cleared
  // (e.g. reconciliation) re-run once resolution is complete.
  const [resolvingHash, setResolvingHash] = createSignal(pendingHash !== null);

  const [layoutState, setLayoutState] = createSignal<BSPAssignments>(
    (() => {
      // Don't resolve hash assignments yet — sessions haven't arrived.
      // Start with empty assignments; the effect below will resolve them.
      if (pendingHash) {
        const assignments: Record<string, SessionId | null> = {};
        for (const paneId of paneIds()) {
          assignments[paneId] = null;
        }
        return { assignments };
      }
      const orderedSessionIds = buildCandidateOrder({
        liveSessionIds: liveSessionIds(),
        focusedSessionId: props.focusedSessionId,
        lruSessionIds: props.lruSessionIds,
      });
      return assignSessionsToPanes(panes(), orderedSessionIds);
    })(),
  );

  let lastDsl = props.layout.dsl;
  let lastLayout = props.layout;
  // Monotonic tag source for leaves created by splitPane (must be unique in
  // the tree so the serialized DSL round-trips).
  let splitTagCounter = 0;

  // React to external layout changes.
  createEffect(() => {
    const layout = props.layout;
    if (layout === lastLayout) return;

    const currentPanes = enumeratePanes(root());
    const live = new Set(liveSessionIds());
    const prev = layoutState().assignments;
    // Carry forward the previous panes' contents in traversal order so
    // surfaces and sessions migrate positionally into the new layout.
    const carried: string[] = [];
    const seenSessions = new Set<string>();
    for (const pane of currentPanes) {
      const v = prev[pane.id];
      if (v == null) continue;
      if (isSurfaceAssignment(v) || isTileAssignment(v)) {
        // Surfaces and IDE tiles (editor/diff) are not sessions — carry them
        // forward positionally so they survive a layout change (the drop the
        // carry-forward would otherwise cause; docs/ide-plan.md F4).
        carried.push(v);
      } else if (live.has(v) && !seenSessions.has(v)) {
        seenSessions.add(v);
        carried.push(v);
      }
    }
    // Append remaining live sessions (focus/LRU-ordered) so any new
    // empty panes still get populated.
    const extra = buildCandidateOrder({
      liveSessionIds: liveSessionIds(),
      focusedSessionId: props.focusedSessionId,
      currentAssignedInPaneOrder: [...seenSessions],
      lruSessionIds: props.lruSessionIds,
    });
    for (const id of extra) {
      if (!seenSessions.has(id)) {
        seenSessions.add(id);
        carried.push(id);
      }
    }
    const nextRoot = layout.root;
    const nextPanes = enumeratePanes(nextRoot);

    lastLayout = layout;
    lastDsl = layout.dsl;
    setRoot(nextRoot);
    setLayoutState(assignSessionsToPanes(nextPanes, carried));
  });

  const knownSessionIds = createMemo(() => sessions().map((s) => s.id));

  // Resolve pending hash assignments to live session IDs / surface assignment
  // strings.  Hash values use "t:connectionId:ptyId" for terminals and
  // "s:connectionId:surfaceId" for compositor surfaces.
  //
  // Terminals are resolved progressively as sessions arrive from the server.
  // Surface entries are resolved immediately (they don't depend on a session
  // list).  Once all referenced connections are ready, any remaining
  // unmatched terminal entries are given up on and pendingHash is cleared so
  // normal reconciliation takes over.
  // Tab refs ("t:<conn>:<id>" with a non-digit id) resolve asynchronously
  // against the server's tabs/ registry (docs/design/kv.md) — one kvFetch
  // per pane, fired once the connection advertises the kv capability (at
  // S2C_HELLO; sessions aren't needed, so this beats terminal resolution).
  const tabFetchesInFlight = new Set<string>();
  function applyResolvedTab(paneId: string, assignment: string | null) {
    if (!pendingHash || !(paneId in pendingHash)) return;
    delete pendingHash[paneId];
    if (assignment) {
      setLayoutState((prev) => ({
        assignments: { ...prev.assignments, [paneId]: assignment },
      }));
    }
    // Unresolvable (deleted tab, no kv): the pane degrades to empty and
    // reconciliation fills it once resolvingHash flips false.
    if (Object.keys(pendingHash).length === 0) {
      pendingHash = null;
      setResolvingHash(false);
    }
  }

  createEffect(() => {
    if (!pendingHash) return;
    const live = liveSessions();
    const snap = workspaceState();
    // Collect connection IDs referenced by pending *terminal* entries (tab
    // refs resolve on their own path and must not join the ready-sweep).
    const referencedConnIds = new Set<string>();
    for (const ref of Object.values(pendingHash)) {
      if (!ref.startsWith("t:")) continue;
      const body = ref.slice(2); // "connectionId:ptyId"
      const lastColon = body.lastIndexOf(":");
      if (lastColon <= 0) continue;
      if (!isPtyRef(body.slice(lastColon + 1))) continue;
      referencedConnIds.add(body.slice(0, lastColon));
    }

    const resolved: Record<string, string> = {};
    for (const [paneId, ref] of Object.entries(pendingHash)) {
      if (ref.startsWith("s:")) {
        // Surface: "s:connectionId:surfaceId" → surfaceAssignment(connId, id)
        const body = ref.slice(2);
        const lastColon = body.lastIndexOf(":");
        if (lastColon <= 0) continue;
        const connId = body.slice(0, lastColon);
        const surfId = parseInt(body.slice(lastColon + 1), 10);
        if (Number.isFinite(surfId)) {
          resolved[paneId] = surfaceAssignment(connId, surfId);
        }
        continue;
      }
      if (ref.startsWith("t:")) {
        const body = ref.slice(2);
        const lastColon = body.lastIndexOf(":");
        if (lastColon <= 0) continue;
        const connId = body.slice(0, lastColon);
        const seg = body.slice(lastColon + 1);
        if (!isPtyRef(seg)) {
          // Tab ref: fetch the assignment from tabs/<id> exactly once.
          const c = snap.connections.find((c) => c.id === connId);
          if (c && c.supportsKv && !tabFetchesInFlight.has(paneId)) {
            tabFetchesInFlight.add(paneId);
            resolveTab(workspace, connId, seg)
              .then((assignment) => {
                tabFetchesInFlight.delete(paneId);
                applyResolvedTab(paneId, assignment);
              })
              .catch(() => {
                // Transient (re-establish mid-flight): re-arm; the effect
                // refires on the next snapshot change and retries.
                tabFetchesInFlight.delete(paneId);
              });
          } else if (c && c.ready && !c.supportsKv) {
            // Ready without the kv store: the ref can never resolve.
            applyResolvedTab(paneId, null);
          }
          continue;
        }
        // Terminal: "t:connectionId:ptyId" → session ID
        const ptyId = parseInt(seg, 10);
        const session = live.find(
          (s) => s.connectionId === connId && s.ptyId === ptyId,
        );
        if (session) resolved[paneId] = session.id;
      }
    }

    if (Object.keys(resolved).length > 0) {
      // Apply newly resolved assignments and remove them from pendingHash.
      for (const paneId of Object.keys(resolved)) {
        delete pendingHash[paneId];
      }
      setLayoutState((prev) => ({
        assignments: { ...prev.assignments, ...resolved },
      }));
    }

    if (Object.keys(pendingHash).length === 0) {
      // All entries resolved — done.
      pendingHash = null;
      setResolvingHash(false);
      return;
    }

    // Check whether all referenced connections have received their initial
    // session list (ready=true).  Only then can we be sure that unmatched
    // ptyIds are genuinely gone — give up on those specific entries and let
    // normal reconciliation fill the empty panes.
    //
    // Missing connections (not yet added to the workspace) are treated as
    // *not* ready — their sessions may still arrive once the connection is
    // established.  Only connections that are present AND ready count.
    const readyConnIds = new Set<string>();
    for (const connId of referencedConnIds) {
      const c = snap.connections.find((c) => c.id === connId);
      if (c?.ready === true) readyConnIds.add(connId);
    }
    if (readyConnIds.size > 0) {
      // Drop pending terminal entries whose connection is ready — those
      // PTYs are genuinely gone.  Keep entries for connections that are
      // missing or still connecting.
      for (const [paneId, ref] of Object.entries(pendingHash)) {
        if (!ref.startsWith("t:")) continue;
        const body = ref.slice(2);
        const lastColon = body.lastIndexOf(":");
        if (lastColon <= 0) continue;
        // Never sweep a tab ref — its kvFetch may be mid-flight.
        if (!isPtyRef(body.slice(lastColon + 1))) continue;
        const connId = body.slice(0, lastColon);
        if (readyConnIds.has(connId)) {
          delete pendingHash[paneId];
        }
      }
      if (Object.keys(pendingHash).length === 0) {
        pendingHash = null;
        setResolvingHash(false);
      }
    }
  });

  // Durable mapping from session ID → "connectionId:ptyId".  Survives
  // connection removal so that when a remote is re-added we can remap stale
  // pane assignments to newly created sessions for the same PTY.
  const durableSessionKeys = new Map<string, string>();

  // Single memo that builds both the session-replacement map (closed →
  // live session ID for the same PTY) and the session→connectionId map
  // (including entries for removed connections).  Both share the same
  // durableSessionKeys bookkeeping, so computing them together avoids
  // iterating sessions() twice.
  const sessionMaps = createMemo(() => {
    const allSessions = sessions();
    // Record every session we've ever seen so we can remap after a
    // remove-then-readd of a connection.
    for (const s of allSessions) {
      if (s.ptyId != null) {
        durableSessionKeys.set(s.id, `${s.connectionId}:${s.ptyId}`);
      }
    }
    const liveByKey = new Map<string, string>();
    const connectionIds = new Map<string, string>();
    for (const s of allSessions) {
      connectionIds.set(s.id, s.connectionId);
      if (s.state !== "closed") {
        liveByKey.set(`${s.connectionId}:${s.ptyId}`, s.id);
      }
    }
    const replacements = new Map<string, string>();
    for (const s of allSessions) {
      if (s.state === "closed") {
        const replacement = liveByKey.get(`${s.connectionId}:${s.ptyId}`);
        if (replacement && replacement !== s.id) {
          replacements.set(s.id, replacement);
        }
      }
    }
    // Remap sessions that were completely removed (connection destroyed)
    // but whose underlying PTY now has a live session again.  Also fill
    // in connectionIds for removed sessions.
    const currentIds = new Set(allSessions.map((s) => s.id));
    for (const [oldId, key] of durableSessionKeys) {
      if (!currentIds.has(oldId)) {
        if (!replacements.has(oldId)) {
          const replacement = liveByKey.get(key);
          if (replacement) replacements.set(oldId, replacement);
        }
        const colonIdx = key.indexOf(":");
        if (colonIdx > 0) connectionIds.set(oldId, key.slice(0, colonIdx));
      }
    }
    return { replacements, connectionIds };
  });

  createEffect(() => {
    if (!connected()) return;
    // Skip reconciliation while we still have pending hash assignments to resolve.
    if (resolvingHash()) return;
    const p = panes();
    const live = liveSessionIds();
    const known = knownSessionIds();
    const surfaceKeys = props.liveSurfaceKeys;
    const { replacements, connectionIds: sessionConns } = sessionMaps();
    // Only include connections that are both present AND ready.  A
    // connection that is present but not ready (reconnecting) has its
    // surface list momentarily empty — treating it as "ready" would
    // cause reconciliation to nuke surface assignments that will
    // reappear once the handshake finishes.
    const readyConns = new Set(
      workspaceState()
        .connections.filter((c) => c.ready)
        .map((c) => c.id),
    );
    setLayoutState((previous) => {
      const next = reconcileAssignments({
        panes: p,
        previous,
        liveSessionIds: live,
        knownSessionIds: known,
        liveSurfaceKeys: surfaceKeys,
        readyConnectionIds: readyConns,
        sessionReplacements: replacements,
        sessionConnectionIds: sessionConns,
      });
      return sameAssignments(previous, next) ? previous : next;
    });
  });

  // BSPContainer does not discover surfaces on its own; callers assign them
  // explicitly via moveToPane.

  const assignedInPaneOrder = createMemo(() =>
    paneIds()
      .map((paneId) => layoutState().assignments[paneId])
      .filter((v): v is SessionId => v != null && !isContentAssignment(v)),
  );

  // focusedPaneId is the single source of truth for which pane is active.
  const [focusedPaneId, setFocusedPaneId] = createSignal<string | null>(
    (() => {
      const fromHash = loadFocusedPaneFromHash();
      if (fromHash && paneIds().includes(fromHash)) return fromHash;
      if (!props.focusedSessionId) return paneIds()[0] ?? null;
      return (
        paneIds().find(
          (id) => layoutState().assignments[id] === props.focusedSessionId,
        ) ??
        paneIds()[0] ??
        null
      );
    })(),
  );

  /**
   * The soloed pane: rendered filling the workspace, siblings hidden.
   *
   * Hidden, not unmounted, and the tree is never rewritten. Both matter.
   * Replacing `root()` with the soloed subtree would renumber every pane id
   * (they are positional paths — see `enumeratePanes`) and unmount the
   * siblings, disposing terminal surfaces and resetting editors; a one-child
   * split is not even expressible in the DSL. Hiding costs nothing to undo.
   *
   * Not persisted, like the PaneTools corner: outliving a hover is the point,
   * surviving a reload is not.
   */
  const [soloedPaneId, setSoloedPaneId] = createSignal<string | null>(null);
  function toggleSolo(paneId: string) {
    // Nothing to solo against in a single-pane layout.
    if (paneIds().length < 2) return;
    setSoloedPaneId((cur) => (cur === paneId ? null : paneId));
    focusPane(paneId);
  }
  // A pane id only means something against the tree that minted it, so any
  // change of shape drops the solo rather than soloing whatever now sits at
  // that path.
  createEffect(() => {
    const ids = paneIds();
    const solo = untrack(soloedPaneId);
    if (solo && (!ids.includes(solo) || ids.length < 2)) setSoloedPaneId(null);
  });

  // Derive the focused session from the focused pane.
  // Returns null if the pane holds a surface rather than a session.
  const focusedPaneSessionId = createMemo(() => {
    const fpId = focusedPaneId();
    if (!fpId) return null;
    const value = layoutState().assignments[fpId] ?? null;
    return value && !isContentAssignment(value) ? value : null;
  });

  // Keep focusedPaneId valid when panes change.
  createEffect(() => {
    const fpId = focusedPaneId();
    if (fpId != null && !paneIds().includes(fpId)) {
      setFocusedPaneId(paneIds()[0] ?? null);
    }
  });

  // Push our derived session up to Workspace.
  createEffect(() => {
    const fpSessionId = focusedPaneSessionId();
    if (fpSessionId !== props.focusedSessionId) {
      props.onFocusSession(fpSessionId);
    }
  });

  // Allow Workspace to focus a specific session's pane (e.g. from menu).
  // If the session is already visible in a pane, focus that pane.
  // Otherwise swap it into the currently focused pane so sidebar clicks work.
  function focusBySession(sessionId: SessionId) {
    const paneId = paneIds().find(
      (id) => layoutState().assignments[id] === sessionId,
    );
    if (paneId) {
      setFocusedPaneId(paneId);
    } else {
      const fpId = focusedPaneId();
      if (fpId) moveToPane(sessionId, fpId);
    }
  }

  createEffect(() => {
    props.onFocusBySession?.(focusBySession);
  });

  function moveToPane(
    value: string,
    targetPaneId: string,
    fromPaneId?: string,
  ) {
    // Guard against a stale pane id (e.g. a caller still holding a pane path
    // from a previous layout): writing the tile to a non-existent pane would
    // silently render nothing. Fall back to the focused pane, then the first.
    const valid = paneIds();
    let pane = targetPaneId;
    if (!valid.includes(pane)) {
      const fp = focusedPaneId();
      pane = fp && valid.includes(fp) ? fp : (valid[0] ?? targetPaneId);
    }
    // Batched (like splitPane): unbatched, the assignment write flushes
    // first and the still-focused OLD pane's focus effect re-asserts DOM
    // focus into its terminal before the focus moves — stealing the caret
    // on every cross-pane open (Explorer click, dock restore).
    batch(() => {
      setLayoutState((prev) => {
        const assignments = assignmentsAfterDrop(
          prev.assignments,
          value,
          pane,
          fromPaneId,
          valid,
        );
        return assignments ? { ...prev, assignments } : prev;
      });
      setFocusedPaneId(pane);
    });
  }

  function moveSessionToPane(sessionId: SessionId, targetPaneId: string) {
    moveToPane(sessionId, targetPaneId);
  }

  createEffect(() => {
    props.onMoveSessionToPane?.(moveSessionToPane);
  });
  createEffect(() => {
    props.onMoveToPane?.(moveToPane);
  });

  // Split the target pane in two, keeping its current occupant in the first
  // child and placing `value` in a new second child (so opening a tile never
  // evicts the terminal). Inserting a split at a leaf's path only changes that
  // leaf's own pane id (siblings keep their index paths), so the only
  // assignment that must be rekeyed is the target's — from `targetPaneId` to
  // `<path>.0`; the new pane `<path>.1` gets `value`.
  function splitPane(value: string, targetPaneId: string) {
    const cur = root();
    const path = leafPath(cur, targetPaneId);
    if (path === null) {
      // Can't locate the pane in the tree — fall back to a plain replace.
      moveToPane(value, targetPaneId);
      return;
    }
    let oldLeaf: BSPNode = cur;
    for (const idx of path) {
      oldLeaf = (oldLeaf as BSPSplit).children[idx].node;
    }
    const newLeaf: BSPLeaf = { type: "leaf", tag: `ide${++splitTagCounter}` };
    const split: BSPSplit = {
      type: "split",
      direction: "horizontal",
      children: [
        { node: oldLeaf, weight: 1 },
        { node: newLeaf, weight: 1 },
      ],
    };
    const newRoot = replaceNodeAtPath(cur, path, split);
    const oldNewId = [...path, 0].join(".");
    const newId = [...path, 1].join(".");
    // Batch: the target pane's id changes during the split, so root and
    // assignments must flush in one reactive cycle. Otherwise the intermediate
    // state (rekeyed assignment, stale panes) transiently hides the sibling
    // terminal and flip-flops focus (delegated event handlers aren't batched).
    batch(() => {
      setLayoutState((prev) => {
        const assignments = { ...prev.assignments };
        const occupant = assignments[targetPaneId];
        if (oldNewId !== targetPaneId) {
          delete assignments[targetPaneId];
          if (occupant != null) assignments[oldNewId] = occupant;
        }
        assignments[newId] = value;
        return { ...prev, assignments };
      });
      updateRoot(newRoot);
      setFocusedPaneId(newId);
    });
  }

  createEffect(() => {
    props.onSplitPane?.(splitPane);
  });

  function clearPaneAssignment(paneId: string) {
    setLayoutState((prev) => {
      if (prev.assignments[paneId] == null) return prev;
      return {
        ...prev,
        assignments: { ...prev.assignments, [paneId]: null },
      };
    });
  }

  createEffect(() => {
    props.onClearPaneAssignment?.(clearPaneAssignment);
  });

  /**
   * Close whatever occupies `paneId`. The dispatch mirrors Ctrl+Alt+Shift+Q
   * (createKeyboardShortcuts) target for target, so the ✕ and the chord can't
   * mean different things: a tile or web pane closes its tab host-wide, a
   * surface closes on its own connection, a terminal closes its session.
   *
   * An assignment that parses as none of those is an unresolved ref (a tab id
   * or connectionId:ptyId whose session never arrived). There is nothing to
   * close on the server, so emptying the pane is the whole job.
   */
  function closePane(paneId: string) {
    const assign = layoutState().assignments[paneId] ?? null;
    if (assign == null) return;
    if (isTileAssignment(assign) || isWebAssignment(assign)) {
      clearPaneAssignment(paneId);
      props.onCloseTab?.(assign);
      return;
    }
    if (isSurfaceAssignment(assign)) {
      const parsed = parseSurfaceAssignment(assign);
      if (parsed) {
        workspace.closeSurface(parsed.connectionId, parsed.surfaceId);
        return;
      }
      clearPaneAssignment(paneId);
      return;
    }
    const session = liveSessions().find((item) => item.id === assign);
    if (!session) {
      clearPaneAssignment(paneId);
      return;
    }
    void workspace.closeSession(session.id);
  }

  function focusPane(paneId: string) {
    setFocusedPaneId(paneId);
  }

  // Report focused pane changes.
  createEffect(() => {
    props.onFocusedPaneChange?.(focusedPaneId());
  });

  createEffect(() => {
    props.onFocusPane?.(focusPane);
  });

  // Remember last active tab per tabs container so switching away doesn't reset.
  const tabMemory: Record<string, number> = {};

  // Ctrl-[ / Ctrl-] to cycle panes. Tabs containers automatically
  // switch to show the focused pane.
  createEffect(() => {
    const ids = paneIds();
    const fpId = focusedPaneId();
    const handler = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
      // When Ctrl is held many browsers report a control character for
      // e.key instead of the literal bracket.  Fall back to e.code so the
      // shortcut works regardless.
      const bracket =
        e.key === "[" || e.code === "BracketLeft"
          ? "["
          : e.key === "]" || e.code === "BracketRight"
            ? "]"
            : null;
      if (!bracket) return;
      e.preventDefault();
      // Stop the event outright: a focused editor would otherwise also
      // treat Ctrl-[ / Ctrl-] as indent (CodeMirror's Mod-[ on
      // non-mac), and a focused terminal would forward Ctrl-[ as ESC.
      e.stopPropagation();
      const idx = fpId ? ids.indexOf(fpId) : -1;
      const delta = bracket === "]" ? 1 : -1;
      const next = (idx + delta + ids.length) % ids.length;
      focusPane(ids[next]);
    };
    window.addEventListener("keydown", handler, true);
    onCleanup(() => window.removeEventListener("keydown", handler, true));
  });

  // Ctrl/Cmd+Shift+K: solo the focused pane, or lift the solo. Lives here
  // rather than in createKeyboardShortcuts because it is meaningless without
  // a layout — outside BSP there is no container listening, so the chord is
  // simply free again.
  createEffect(() => {
    const fpId = focusedPaneId();
    const handler = (e: KeyboardEvent) => {
      if (!(e.ctrlKey || e.metaKey) || !e.shiftKey || e.altKey) return;
      if (e.key !== "K" && e.key !== "k" && e.code !== "KeyK") return;
      if (!fpId) return;
      e.preventDefault();
      // A focused terminal would otherwise also receive the chord.
      e.stopPropagation();
      toggleSolo(fpId);
    };
    window.addEventListener("keydown", handler, true);
    onCleanup(() => window.removeEventListener("keydown", handler, true));
  });

  createEffect(() => {
    const state = layoutState();
    // Always report assignments so that Workspace can derive the focused
    // surface (for the status bar) and filter offScreenSurfaces even
    // while hash resolution is in progress.  The URL-hash writer in
    // Workspace guards against overwriting unresolved entries separately
    // via onAssignmentsResolved.
    props.onAssignmentsChange?.(state);
  });

  createEffect(() => {
    props.onAssignmentsResolved?.(!resolvingHash());
  });

  createEffect(() => {
    const manageVisibility = props.manageVisibility ?? true;
    if (!manageVisibility) return;
    const ids = assignedInPaneOrder();
    const extra = props.extraVisibleSessions;
    if (extra && extra.length > 0) {
      workspace.setVisibleSessions([...ids, ...extra]);
    } else {
      workspace.setVisibleSessions(ids);
    }
  });

  function updateRoot(next: BSPNode) {
    setRoot(next);
    const dsl = serializeDSL(next);
    const updated: BSPLayout = { ...props.layout, root: next, dsl };
    lastLayout = updated;
    lastDsl = dsl;
    saveActiveLayout(updated);
    props.onLayoutChange(updated);
  }

  function handleResize(
    split: BSPSplit,
    indexA: number,
    indexB: number,
    fraction: number,
  ) {
    const updated = adjustWeights(split, indexA, indexB, fraction);
    const replaceNode = (node: BSPNode): BSPNode => {
      if (node === split) return updated;
      if (node.type === "leaf") return node;
      return {
        ...node,
        children: node.children.map((child) => ({
          ...child,
          node: replaceNode(child.node),
        })),
      };
    };
    updateRoot(replaceNode(root()));
  }

  createEffect(() => {
    const fsId = props.focusedSessionId;
    const live = liveSessions();
    const fpId = focusedPaneId();
    const handler = (event: KeyboardEvent) => {
      if (!fsId) return;
      const session = live.find((item) => item.id === fsId);
      if (!session || session.state !== "exited") return;
      if (event.key === "Enter") {
        event.preventDefault();
        workspace.restartSession(fsId);
      } else if (event.key === "Escape") {
        event.preventDefault();
        // Immediately clear the pane assignment so the exited terminal
        // disappears without waiting for the server round-trip.
        if (fpId) {
          setLayoutState((prev) => {
            if (prev.assignments[fpId] !== fsId) return prev;
            return {
              assignments: { ...prev.assignments, [fpId]: null },
            };
          });
        }
        void workspace.closeSession(fsId);
      }
    };
    window.addEventListener("keydown", handler);
    onCleanup(() => window.removeEventListener("keydown", handler));
  });

  const multiPane = () => leafCount(root()) > 1;

  // Each reactive field is exposed via a getter so consumers reading
  // `ctx.foo` see the current value.  Solid's Provider captures `props.value`
  // once under `untrack`, so a plain-object literal would freeze every field
  // to the mount-time snapshot — breaking e.g. connectionLabels when a new
  // remote is added after BSPContainer mounts.
  const ctxValue: BSPTreeCtx = {
    get connectionId() {
      return props.connectionId;
    },
    get connectionLabels() {
      return props.connectionLabels;
    },
    get multiPane() {
      return multiPane();
    },
    get isMobileTouch() {
      return props.isMobileTouch;
    },
    get isSessionReadOnly() {
      return props.isSessionReadOnly;
    },
    onFocusPane: focusPane,
    onClosePane: closePane,
    get onCreateInPane() {
      return props.onCreateInPane;
    },
    get onSwitcher() {
      return props.onSwitcher;
    },
    get onHelp() {
      return props.onHelp;
    },
    onResize: handleResize,
    get palette() {
      return props.palette;
    },
    get fontFamily() {
      return props.fontFamily;
    },
    get fontSize() {
      return props.fontSize;
    },
    get surfaceZoom() {
      return props.surfaceZoom ?? 1;
    },
    tabMemory,
    get onRender() {
      return props.onRender;
    },
    get onTerminalSurface() {
      return props.onTerminalSurface;
    },
    get registerWebPaneHost() {
      return props.registerWebPaneHost;
    },
    get onOpenTile() {
      return props.onOpenTile;
    },
    get onDropTile() {
      return props.onDropTile;
    },
    get soloedPaneId() {
      return soloedPaneId();
    },
    onToggleSolo: toggleSolo,
  };
  return (
    <BSPTreeContext.Provider value={ctxValue}>
      <div style={{ width: "100%", height: "100%", display: "flex" }}>
        <BSPPane
          node={root()}
          assignments={layoutState().assignments}
          focusedPaneId={focusedPaneId()}
          visible={props.manageVisibility ?? true}
        />
      </div>
    </BSPTreeContext.Provider>
  );
}

function BSPPane(props: {
  node: BSPNode;
  assignments: Record<string, SessionId | null>;
  focusedPaneId: string | null;
  visible: boolean;
  path?: number[];
}) {
  const ctx = useBSPTree();
  // All branching uses <Show> so Solid re-evaluates when props.node changes
  // (e.g. on layout switch or resize).  <Index> is used for split children
  // so that components persist by position — only the item signal updates,
  // avoiding unnecessary recreation during resize drags.

  const path = () => props.path ?? [];
  const paneId = () => {
    const p = path();
    return p.length > 0 ? p.join(".") : "0";
  };

  /**
   * Index of the child containing the soloed pane, or -1 when this split has
   * no say. Matching by path prefix is what lets a solo deep in the tree
   * clear every ancestor's siblings on the way down.
   */
  const soloChild = (children: readonly BSPChild[]): number => {
    const solo = ctx.soloedPaneId;
    if (!solo) return -1;
    for (let i = 0; i < children.length; i++) {
      const prefix = [...path(), i].join(".");
      if (solo === prefix || solo.startsWith(prefix + ".")) return i;
    }
    return -1;
  };

  return (
    <Show
      when={props.node.type === "split" ? (props.node as BSPSplit) : undefined}
      fallback={
        <LeafPane
          paneId={paneId()}
          leaf={props.node as BSPLeaf}
          sessionId={props.assignments[paneId()] ?? null}
          isFocused={paneId() === props.focusedPaneId}
          visible={props.visible}
        />
      }
    >
      {(split) => (
        <Show
          when={split().direction === "tabs"}
          fallback={
            <div
              style={{
                display: "flex",
                "flex-direction":
                  split().direction === "horizontal" ? "row" : "column",
                width: "100%",
                height: "100%",
              }}
            >
              <Index each={split().children}>
                {(child, index) => {
                  const solo = () => soloChild(split().children);
                  const hidden = () => solo() >= 0 && index !== solo();
                  return (
                    <>
                      {/* No handle to drag while one pane fills the split. */}
                      <Show when={index > 0 && solo() < 0}>
                        <ResizeHandle
                          direction={
                            split().direction as "horizontal" | "vertical"
                          }
                          onDrag={(fraction) =>
                            ctx.onResize(split(), index - 1, index, fraction)
                          }
                        />
                      </Show>
                      <div
                        style={{
                          // The soloed branch takes the whole split; its
                          // siblings keep their weights for the moment the
                          // solo is lifted.
                          flex: solo() >= 0 ? 1 : child().weight,
                          display: hidden() ? "none" : undefined,
                          overflow: "hidden",
                          position: "relative",
                          "min-width": 0,
                          "min-height": 0,
                        }}
                      >
                        <BSPPane
                          node={child().node}
                          assignments={props.assignments}
                          focusedPaneId={props.focusedPaneId}
                          // Not merely cosmetic: `visible` gates
                          // `resizable`, and a hidden-but-resizable terminal
                          // measures 0×0. The client sends the *minimum*
                          // across a session's views, so leaving these true
                          // would pin the soloed PTY to 1×1.
                          visible={props.visible && !hidden()}
                          path={[...(props.path ?? []), index]}
                        />
                      </div>
                    </>
                  );
                }}
              </Index>
            </div>
          }
        >
          {(() => {
            const theme = () => themeFor(ctx.palette);
            const scale = () => uiScale(ctx.fontSize);
            const tabKey = () => path().join(".") || "root";

            const activeTab = () => {
              const focusedPrefix = props.focusedPaneId ?? "";
              const s = split();
              let active = -1;
              for (let i = 0; i < s.children.length; i++) {
                const childPrefix = [...path(), i].join(".");
                if (
                  focusedPrefix === childPrefix ||
                  focusedPrefix.startsWith(childPrefix + ".")
                ) {
                  active = i;
                  break;
                }
              }
              if (active >= 0) {
                ctx.tabMemory[tabKey()] = active;
                return active;
              }
              return Math.min(
                ctx.tabMemory[tabKey()] ?? 0,
                s.children.length - 1,
              );
            };

            const tabLabel = (child: BSPChild, index: number): string => {
              if (child.label) return child.label;
              if (child.node.type === "leaf" && child.node.tag)
                return child.node.tag;
              return tp("bsp.tab", { index: index + 1 });
            };

            return (
              <div
                style={{
                  display: "flex",
                  "flex-direction": "column",
                  width: "100%",
                  height: "100%",
                }}
              >
                <div
                  style={{
                    display: "flex",
                    gap: "1px",
                    "flex-shrink": 0,
                    "background-color": theme().solidPanelBg,
                    "border-bottom": `1px solid ${theme().subtleBorder}`,
                    "font-size": `${scale().sm}px`,
                  }}
                >
                  <For each={split().children}>
                    {(child, index) => {
                      const childPath = () => [...path(), index()].join(".");
                      return (
                        <button
                          onClick={() => ctx.onFocusPane(childPath())}
                          style={{
                            ...ui.btn,
                            flex: 1,
                            "min-width": 0,
                            padding: `${scale().controlY}px ${scale().controlX}px`,
                            "font-size": `${scale().sm}px`,
                            "text-align": "center",
                            overflow: "hidden",
                            "text-overflow": "ellipsis",
                            "white-space": "nowrap",
                            opacity: index() === activeTab() ? 1 : 0.5,
                            "border-bottom":
                              index() === activeTab()
                                ? `1px solid ${theme().accent}`
                                : "1px solid transparent",
                          }}
                        >
                          {tabLabel(child, index())}
                        </button>
                      );
                    }}
                  </For>
                </div>
                <div
                  style={{
                    flex: 1,
                    overflow: "hidden",
                    position: "relative",
                    "min-height": 0,
                  }}
                >
                  {/* Keep every tab body mounted. In particular, a restored web
                      pane must create its iframe and bind to the preview worker
                      without waiting for the user to focus its tab. Persistent
                      bodies also preserve in-frame navigation when switching
                      between tabs; inactive bodies are only hidden. */}
                  <For each={split().children}>
                    {(child, index) => {
                      const active = () => index() === activeTab();
                      return (
                        <div
                          style={{
                            position: "absolute",
                            inset: 0,
                            display: active() ? "block" : "none",
                          }}
                        >
                          <BSPPane
                            node={child.node}
                            assignments={props.assignments}
                            focusedPaneId={props.focusedPaneId}
                            visible={props.visible && active()}
                            path={[...path(), index()]}
                          />
                        </div>
                      );
                    }}
                  </For>
                </div>
              </div>
            );
          })()}
        </Show>
      )}
    </Show>
  );
}

function LeafPane(props: {
  paneId: string;
  leaf: BSPLeaf;
  sessionId: SessionId | null;
  isFocused: boolean;
  visible: boolean;
}) {
  const ctx = useBSPTree();
  const theme = () => themeFor(ctx.palette);
  const scale = () => uiScale(ctx.fontSize);
  const workspace = createBlitWorkspace();
  const sessions = createBlitSessions(workspace);
  const workspaceState = createBlitWorkspaceState(workspace);

  const surfaceParsed = () => parseSurfaceAssignment(props.sessionId);
  const isSurface = () => surfaceParsed() != null;
  const tileParsed = () => parseTileAssignment(props.sessionId);
  const webParsed = () => parseWebAssignment(props.sessionId);
  const surfaceId = () => surfaceParsed()?.surfaceId ?? null;
  // Highlighted while a tile drag hovers this pane (a valid drop target).
  const [tileDragOver, setTileDragOver] = createSignal(false);
  // Reveals the corner tools on pointer devices (see PaneTools).
  const [hovered, setHovered] = createSignal(false);
  const surfaceConnectionId = () =>
    surfaceParsed()?.connectionId ?? ctx.connectionId;

  /** True when the surface's owning connection is present in the workspace.
   *  When the remote is removed the connection disappears — we hide the
   *  surface view (the assignment is still preserved so it can reattach
   *  once the remote is re-added). */
  const surfaceConnPresent = () => {
    const parsed = surfaceParsed();
    if (!parsed) return false;
    const snap = workspaceState();
    return snap.connections.some((c) => c.id === parsed.connectionId);
  };

  const session = () =>
    isSurface()
      ? null
      : (sessions().find((item) => item.id === props.sessionId) ?? null);

  const connection = () => {
    const snap = workspaceState();
    return snap.connections.find((c) => c.id === ctx.connectionId) ?? null;
  };

  let paneContainer!: HTMLDivElement;
  let autoCreated = false;

  createEffect(() => {
    // Tabs keep their bodies mounted so web panes can materialize eagerly, but
    // an inactive command leaf should retain the old lazy-start behavior.
    if (!props.visible) return;
    if (props.sessionId || !props.leaf.command || autoCreated) return;
    if (connection()?.status !== "connected") return;
    autoCreated = true;
    ctx.onCreateInPane?.(props.paneId, props.leaf.command);
  });

  // Per-pane memos (default equality): the raw props read through the shared
  // assignments object, whose identity changes on ANY pane's reassignment —
  // without the memo, every pane's focus effect re-runs on every layout
  // mutation and the focused pane re-asserts DOM focus it never lost.
  const paneSession = createMemo(() => props.sessionId);
  const paneVisible = createMemo(() => props.visible);
  createEffect(() => {
    // Track these dependencies
    const focused = props.isFocused;
    const _sid = paneSession();
    const _vis = paneVisible();
    if (focused && paneContainer) {
      // Focus the pane container's focusable child. An editable CodeMirror
      // content div comes FIRST: a comma-list querySelector returns the
      // first match in *document* order, and an editor tile has [tabindex]
      // elements (the scroller) before `.cm-content` — focusing those
      // leaves the editor without keyboard focus or a visible cursor.
      // Read-only CM contents (diff views) are contenteditable=false and
      // unfocusable, so they fall through to the [tabindex] pass (the
      // diff root). Bare "canvas" is excluded — the terminal canvas has
      // no tabindex so focus() is a no-op; surface canvases have tabindex.
      const pick = (): HTMLElement | null =>
        paneContainer.querySelector<HTMLElement>(
          '.cm-content[contenteditable="true"]',
        ) ??
        paneContainer.querySelector<HTMLElement>("[tabindex], input, textarea");
      const focusable = pick();
      if (focusable) {
        focusable.focus();
      } else {
        // BlitTerminal attaches its canvas in onMount which runs after
        // this effect.  Retry once the current reactive flush completes.
        queueMicrotask(() => pick()?.focus());
      }
    }
  });

  return (
    <div
      ref={paneContainer}
      data-blit-bsp-pane-id={props.paneId}
      data-blit-bsp-focused={props.isFocused ? "true" : undefined}
      style={{
        width: "100%",
        height: "100%",
        position: "relative",
        border: ctx.multiPane
          ? props.isFocused
            ? `1px solid ${theme().accent}`
            : "1px solid transparent"
          : "none",
      }}
      onPointerDown={() => ctx.onFocusPane(props.paneId)}
      onFocusIn={() => ctx.onFocusPane(props.paneId)}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      onDragOver={(e) => {
        if (!ctx.onDropTile || !isTileDrag(e)) return;
        e.preventDefault(); // allow the drop
        e.dataTransfer!.dropEffect = "copy";
        if (!tileDragOver()) setTileDragOver(true);
      }}
      onDragLeave={(e) => {
        // Ignore leaves into child elements; only clear when truly leaving.
        if (!e.currentTarget.contains(e.relatedTarget as Node | null))
          setTileDragOver(false);
      }}
      onDrop={(e) => {
        const assignment = tileDragAssignment(e);
        setTileDragOver(false);
        if (assignment && ctx.onDropTile) {
          e.preventDefault();
          ctx.onDropTile(
            assignment,
            props.paneId,
            paneDragSource(e) ?? undefined,
          );
        }
      }}
    >
      <Show when={tileDragOver()}>
        <div
          style={{
            position: "absolute",
            inset: 0,
            "z-index": 5,
            "pointer-events": "none",
            background: `color-mix(in srgb, ${theme().accent} 14%, transparent)`,
            border: `2px solid ${theme().accent}`,
            "box-sizing": "border-box",
          }}
        />
      </Show>
      {/* Every occupied pane gets the ✕, whatever it holds. Gated on something
          actually being rendered rather than on the assignment being non-null:
          a pane still resolving a tab ref falls through to EmptyPane, which
          offers "New terminal" and has nothing to close. */}
      <Show
        when={tileParsed() || webParsed() || isSurface() || session() != null}
      >
        <PaneTools
          theme={theme()}
          scale={scale()}
          alwaysVisible={ctx.isMobileTouch ?? false}
          hovered={hovered()}
          drag={
            props.sessionId
              ? { assignment: props.sessionId, paneId: props.paneId }
              : undefined
          }
          solo={
            ctx.multiPane
              ? {
                  active: ctx.soloedPaneId === props.paneId,
                  onToggle: () => ctx.onToggleSolo(props.paneId),
                }
              : undefined
          }
          onClose={() => ctx.onClosePane(props.paneId)}
        />
      </Show>
      {/* IDE tile (editor/diff/commit) overlays the pane; its value is mutually
          exclusive with sessions and surfaces (docs/ide-plan.md PR-6). Rendered
          via the shared BlitTile so BSP panes and the non-BSP focused-tile view
          never drift. */}
      <Show when={tileParsed()}>
        <div
          style={{
            position: "absolute",
            inset: 0,
            overflow: "hidden",
            "background-color": theme().bg,
          }}
        >
          <BlitTile
            workspace={workspace}
            assignment={props.sessionId!}
            theme={theme()}
            palette={ctx.palette}
            scale={scale()}
            fontFamily={ctx.fontFamily}
            fontSize={ctx.fontSize}
            onOpenTile={(a) => ctx.onOpenTile?.(a)}
          />
        </div>
      </Show>
      <Show when={webParsed()}>
        {(_) => (
          <div
            style={{
              position: "absolute",
              inset: 0,
              overflow: "hidden",
              "background-color": theme().bg,
            }}
          >
            <WebPaneHost
              assignment={props.sessionId!}
              hostId={`bsp:${props.paneId}`}
              register={ctx.registerWebPaneHost!}
              focused={props.isFocused}
              onFocusRequest={() => ctx.onFocusPane(props.paneId)}
            />
          </div>
        )}
      </Show>
      {/* Terminal / surface / empty layer. Gated on !tileParsed(): a tile is
          mutually exclusive with sessions and surfaces, and because EmptyPane
          is position:relative and follows the tile overlay in the DOM it would
          otherwise paint *over* the tile (showing "New terminal / Menu / Help"
          instead of the editor/diff/commit). */}
      <Show when={!tileParsed() && !webParsed()}>
        <Show
          when={isSurface()}
          fallback={
            <Show
              when={props.sessionId && session()}
              fallback={
                <EmptyPane
                  paneId={props.paneId}
                  label={props.leaf.tag || null}
                  isFocused={props.isFocused}
                  theme={theme()}
                  palette={ctx.palette}
                  fontSize={ctx.fontSize}
                  connectionId={ctx.connectionId}
                  connectionLabels={ctx.connectionLabels}
                  onCreateInPane={ctx.onCreateInPane}
                  onSwitcher={ctx.onSwitcher}
                  onHelp={ctx.onHelp}
                />
              }
            >
              <div style={{ width: "100%", height: "100%" }}>
                <BlitTerminal
                  sessionId={props.sessionId}
                  readOnly={
                    (props.sessionId !== null &&
                      ctx.isSessionReadOnly?.(props.sessionId)) ||
                    false
                  }
                  resizable={props.visible}
                  fontSize={resolveLeafFontSize(props.leaf, ctx.fontSize)}
                  fontFamily={ctx.fontFamily}
                  palette={ctx.palette}
                  style={{ width: "100%", height: "100%" }}
                  showCursor={props.isFocused}
                  onRender={ctx.onRender}
                  surfaceRef={(s) => ctx.onTerminalSurface?.(s)}
                />
              </div>
              <Show when={session()?.state === "exited"}>
                <div
                  style={{
                    position: "absolute",
                    bottom: "8px",
                    left: "50%",
                    transform: "translateX(-50%)",
                    background: theme().solidPanelBg,
                    border: `1px solid ${theme().border}`,
                    padding: `${scale().controlY}px ${scale().controlX}px`,
                    "font-size": `${scale().sm}px`,
                    display: "flex",
                    "align-items": "center",
                    gap: `${scale().gap}px`,
                    // Above the terminal's scroll surface (z-index 1), which
                    // otherwise hit-tests over the banner and swallows the
                    // tap — invisible, but the top layer. Same treatment as
                    // the non-BSP banner in Workspace.
                    "z-index": z.exitedBanner,
                  }}
                >
                  <mark
                    style={{
                      ...ui.badge,
                      "background-color": "rgba(255,100,100,0.3)",
                    }}
                  >
                    {t("bsp.exited")}
                  </mark>
                  <Show when={connection()?.supportsRestart}>
                    <button
                      onClick={() => workspace.restartSession(props.sessionId!)}
                      style={{ ...ui.btn, "font-size": `${scale().sm}px` }}
                    >
                      {t("bsp.restart")} <kbd style={ui.kbd}>Enter</kbd>
                    </button>
                  </Show>
                  <button
                    onClick={() =>
                      void workspace.closeSession(props.sessionId!)
                    }
                    style={{
                      ...ui.btn,
                      "font-size": `${scale().sm}px`,
                      opacity: 0.5,
                    }}
                  >
                    {t("bsp.close")} <kbd style={ui.kbd}>Esc</kbd>
                  </button>
                </div>
              </Show>
            </Show>
          }
        >
          <Show
            when={surfaceConnPresent()}
            fallback={
              <EmptyPane
                paneId={props.paneId}
                label={props.leaf.tag || null}
                isFocused={props.isFocused}
                theme={theme()}
                palette={ctx.palette}
                fontSize={ctx.fontSize}
                connectionId={ctx.connectionId}
                connectionLabels={ctx.connectionLabels}
                onCreateInPane={ctx.onCreateInPane}
                onSwitcher={ctx.onSwitcher}
                onHelp={ctx.onHelp}
              />
            }
          >
            <div style={{ width: "100%", height: "100%" }}>
              <BlitSurfaceView
                connectionId={surfaceConnectionId()}
                surfaceId={surfaceId()!}
                focus={props.isFocused}
                resizable
                zoom={ctx.surfaceZoom}
                style={{ width: "100%", height: "100%" }}
              />
            </div>
          </Show>
        </Show>
      </Show>
    </div>
  );
}

export function EmptyPane(props: {
  paneId: string;
  label: string | null;
  isFocused: boolean;
  theme: Theme;
  palette: TerminalPalette;
  fontSize: number;
  connectionId: string;
  connectionLabels?: Map<string, string>;
  onCreateInPane?: (
    paneId: string,
    command?: string,
    connectionId?: string,
  ) => void;
  onSwitcher?: () => void;
  onHelp?: () => void;
}) {
  const [cmd, setCmd] = createSignal("");
  const [acIdx, setAcIdx] = createSignal(-1);
  const [hovered, setHovered] = createSignal(false);
  let inputRef!: HTMLInputElement;
  const scale = () => uiScale(props.fontSize);
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";
  const active = () => props.isFocused || hovered();

  /**
   * Autocomplete suggestions: connection labels that start with whatever the
   * user has typed before the first `>`, or with the full raw input when
   * there is no `>` yet. Hidden once a valid `label> ` prefix is committed.
   */
  const acSuggestions = createMemo(
    (): Array<{ connId: string; label: string }> => {
      const labels = props.connectionLabels;
      if (!labels || labels.size < 2) return [];
      const raw = cmd();
      const gtIdx = raw.indexOf(">");
      // Once the user has typed `label> ` the prefix is resolved — hide list.
      if (gtIdx !== -1) {
        const part = raw.slice(0, gtIdx).trim().toLowerCase();
        const exact = [...labels].some(([, l]) => l.toLowerCase() === part);
        if (exact) return [];
      }
      const query = (gtIdx === -1 ? raw : raw.slice(0, gtIdx))
        .trim()
        .toLowerCase();
      return [...labels]
        .filter(([, l]) => l.toLowerCase().startsWith(query))
        .map(([connId, label]) => ({ connId, label }));
    },
  );

  // Reset highlighted index when the suggestion list changes.
  createEffect(() => {
    acSuggestions();
    setAcIdx(-1);
  });

  /** Match `remote>command` syntax against connection labels. */
  const destPrefix = createMemo(
    (): { connId: string; label: string } | null => {
      if (!props.connectionLabels) return null;
      const raw = cmd();
      if (!raw.includes(">")) return null;
      const part = raw.slice(0, raw.indexOf(">")).trim().toLowerCase();
      if (!part) return null;
      for (const [connId, label] of props.connectionLabels) {
        if (label.toLowerCase() === part) return { connId, label };
      }
      return null;
    },
  );

  const inlineCmd = () => {
    const raw = cmd();
    if (!raw.includes(">")) return "";
    return raw.slice(raw.indexOf(">") + 1).trim();
  };

  const commitSuggestion = (label: string) => {
    setCmd(`${label}> `);
    inputRef?.focus();
    // Move caret to end.
    queueMicrotask(() => {
      inputRef?.setSelectionRange(inputRef.value.length, inputRef.value.length);
    });
  };

  createEffect(() => {
    if (props.isFocused) inputRef?.focus();
  });

  return (
    <div
      onClick={() => inputRef?.focus()}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      style={{
        width: "100%",
        height: "100%",
        position: "relative",
        "background-color": `rgb(${props.palette.bg[0]},${props.palette.bg[1]},${props.palette.bg[2]})`,
        display: "flex",
        "flex-direction": "column",
        "align-items": "center",
        "justify-content": "center",
        gap: `${scale().gap}px`,
      }}
    >
      <Show when={active()}>
        <div
          style={{
            flex: 1,
            display: "flex",
            "flex-direction": "column",
            "align-items": "center",
            "justify-content": "center",
            gap: `${scale().tightGap}px`,
            "font-size": `${scale().sm}px`,
          }}
        >
          <button
            onClick={(e) => {
              e.stopPropagation();
              // When multiple connections exist, omit connectionId so the
              // Workspace callback opens the remote picker instead of
              // creating a terminal on the current connection directly.
              const multiConn =
                props.connectionLabels && props.connectionLabels.size > 1;
              props.onCreateInPane?.(
                props.paneId,
                undefined,
                multiConn ? undefined : props.connectionId,
              );
            }}
            style={{ ...ui.btn, "font-size": `${scale().md}px` }}
          >
            {t("workspace.newTerminal")} <kbd style={ui.kbd}>{mod}+Enter</kbd>
          </button>
          <Show when={props.onSwitcher}>
            <button
              onClick={(e) => {
                e.stopPropagation();
                props.onSwitcher!();
              }}
              style={{ ...ui.btn, "font-size": `${scale().md}px` }}
            >
              {t("workspace.menu")} <kbd style={ui.kbd}>{mod}+K</kbd>
            </button>
          </Show>
          <Show when={props.onHelp}>
            <button
              onClick={(e) => {
                e.stopPropagation();
                props.onHelp!();
              }}
              style={{ ...ui.btn, "font-size": `${scale().md}px` }}
            >
              {t("workspace.help")} <kbd style={ui.kbd}>Ctrl+?</kbd>
            </button>
          </Show>
        </div>
        <div
          style={{
            "flex-shrink": 0,
            "align-self": "center",
            "margin-bottom": "0.5em",
            "font-size": `${scale().sm}px`,
            display: "flex",
            "flex-direction": "column",
            "min-width": "min(50vw, 220px)",
            background: props.theme.solidInputBg,
            border: `1px solid ${props.theme.subtleBorder}`,
            overflow: "hidden",
          }}
        >
          {/* Autocomplete list — rendered above the input */}
          <Show when={acSuggestions().length > 0}>
            <div
              style={{
                display: "flex",
                "flex-direction": "column",
                "border-bottom": `1px solid ${props.theme.subtleBorder}`,
              }}
            >
              <For each={acSuggestions()}>
                {(item, i) => (
                  <button
                    style={{
                      ...ui.btn,
                      padding: `${scale().controlY}px ${scale().controlX}px`,
                      "text-align": "left",
                      "font-size": `${scale().sm}px`,
                      background:
                        i() === acIdx() ? props.theme.hoverBg : "transparent",
                      color: props.theme.fg,
                      cursor: "pointer",
                      opacity: 1,
                    }}
                    onMouseEnter={() => setAcIdx(i())}
                    onMouseLeave={() => setAcIdx(-1)}
                    onClick={(e) => {
                      e.stopPropagation();
                      commitSuggestion(item.label);
                    }}
                  >
                    {item.label}
                  </button>
                )}
              </For>
            </div>
          </Show>
          <input
            ref={inputRef}
            name={`blit-pane-cmd-${props.paneId}`}
            type="text"
            value={cmd()}
            onInput={(e) => setCmd(e.currentTarget.value)}
            onKeyDown={(e) => {
              const sugs = acSuggestions();
              if (sugs.length > 0) {
                if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setAcIdx((n) => (n <= 0 ? sugs.length - 1 : n - 1));
                  return;
                }
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setAcIdx((n) => (n >= sugs.length - 1 ? 0 : n + 1));
                  return;
                }
                if (e.key === "Tab") {
                  e.preventDefault();
                  const idx = acIdx() >= 0 ? acIdx() : 0;
                  commitSuggestion(sugs[idx].label);
                  return;
                }
                if (e.key === "Enter" && acIdx() >= 0) {
                  e.preventDefault();
                  e.stopPropagation();
                  commitSuggestion(sugs[acIdx()].label);
                  return;
                }
              }
              if (e.key === "Escape") {
                e.stopPropagation();
                return;
              }
              if (e.key === "Enter" && !e.metaKey && !e.ctrlKey) {
                e.preventDefault();
                e.stopPropagation();
                const dp = destPrefix();
                const command = dp
                  ? inlineCmd() || undefined
                  : cmd().trim() || undefined;
                const connId = dp?.connId ?? props.connectionId;
                props.onCreateInPane?.(props.paneId, command, connId);
              }
            }}
            placeholder={t(
              shellCapabilities().remotes
                ? "bsp.commandPlaceholder"
                : "bsp.commandPlaceholderNoRemotes",
            )}
            style={{
              ...ui.input,
              display: "block",
              background: "transparent",
              border: "none",
              color: "inherit",
              padding: `${scale().controlY}px ${scale().controlX}px`,
              "font-size": `${scale().sm}px`,
              "font-family": "inherit",
              width: "100%",
              "box-sizing": "border-box",
            }}
          />
        </div>
      </Show>
    </div>
  );
}
