import {
  createSignal,
  createEffect,
  createMemo,
  createContext,
  useContext,
  onCleanup,
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
import type { SessionId, TerminalPalette } from "@blit-sh/core";
import type { BSPNode, BSPChild, BSPSplit, BSPLeaf } from "@blit-sh/core/bsp";
import { leafCount, serializeDSL } from "@blit-sh/core/bsp";
import type { BSPAssignments, BSPLayout } from "./layout";
import {
  adjustWeights,
  assignSessionsToPanes,
  buildCandidateOrder,
  enumeratePanes,
  loadAssignmentsFromHash,
  loadFocusedPaneFromHash,
  reconcileAssignments,
  saveActiveLayout,
  surfaceAssignment,
  isSurfaceAssignment,
  parseSurfaceAssignment,
} from "./layout";
import { ResizeHandle } from "./ResizeHandle";
import type { Theme } from "../theme";
import { themeFor, ui, uiScale } from "../theme";
import { t, tp } from "../i18n";

/** Props that stay constant through the BSPPane recursion tree.  Hoisted
 *  into context so each level only passes the values that actually change. */
interface BSPTreeCtx {
  connectionId: string;
  connectionLabels?: Map<string, string>;
  multiPane: boolean;
  onFocusPane: (paneId: string) => void;
  onCreateInPane?: (
    paneId: string,
    command?: string,
    connectionId?: string,
  ) => void;
  onSwitcher?: () => void;
  onHelp?: () => void;
  onResize: (
    split: BSPSplit,
    indexA: number,
    indexB: number,
    fraction: number,
  ) => void;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  tabMemory: Record<string, number>;
  onRender?: (renderMs?: number) => void;
}

const BSPTreeContext = createContext<BSPTreeCtx>();
function useBSPTree(): BSPTreeCtx {
  return useContext(BSPTreeContext)!;
}

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

export function BSPContainer(props: {
  layout: BSPLayout;
  onLayoutChange: (layout: BSPLayout | null) => void;
  connectionId: string;
  connectionLabels?: Map<string, string>;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;

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
  onMoveToPane?: (fn: (value: string, targetPaneId: string) => void) => void;
  onClearPaneAssignment?: (fn: (paneId: string) => void) => void;
  onFocusedPaneChange?: (paneId: string | null) => void;
  onRender?: (renderMs?: number) => void;
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
      if (isSurfaceAssignment(v)) {
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
  createEffect(() => {
    if (!pendingHash) return;
    const live = liveSessions();
    const snap = workspaceState();
    // Collect connection IDs referenced by pending *terminal* entries.
    const referencedConnIds = new Set<string>();
    for (const ref of Object.values(pendingHash)) {
      if (!ref.startsWith("t:")) continue;
      const body = ref.slice(2); // "connectionId:ptyId"
      const lastColon = body.lastIndexOf(":");
      if (lastColon > 0) referencedConnIds.add(body.slice(0, lastColon));
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
        // Terminal: "t:connectionId:ptyId" → session ID
        const body = ref.slice(2);
        const lastColon = body.lastIndexOf(":");
        if (lastColon <= 0) continue;
        const connId = body.slice(0, lastColon);
        const ptyId = parseInt(body.slice(lastColon + 1), 10);
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

  // Surfaces are only assigned to panes by explicit user action (switcher,
  // drag-and-drop, etc.) — never automatically.

  const assignedInPaneOrder = createMemo(() =>
    paneIds()
      .map((paneId) => layoutState().assignments[paneId])
      .filter((v): v is SessionId => v != null && !isSurfaceAssignment(v)),
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

  // Derive the focused session from the focused pane.
  // Returns null if the pane holds a surface rather than a session.
  const focusedPaneSessionId = createMemo(() => {
    const fpId = focusedPaneId();
    if (!fpId) return null;
    const value = layoutState().assignments[fpId] ?? null;
    return value && !isSurfaceAssignment(value) ? value : null;
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

  function moveToPane(value: string, targetPaneId: string) {
    setLayoutState((prev) => {
      if (prev.assignments[targetPaneId] === value) return prev;
      return {
        ...prev,
        assignments: {
          ...prev.assignments,
          [targetPaneId]: value,
        },
      };
    });
    setFocusedPaneId(targetPaneId);
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
      const idx = fpId ? ids.indexOf(fpId) : -1;
      const delta = bracket === "]" ? 1 : -1;
      const next = (idx + delta + ids.length) % ids.length;
      focusPane(ids[next]);
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
    const handler = (event: KeyboardEvent) => {
      if (!fsId) return;
      const session = live.find((item) => item.id === fsId);
      if (!session || session.state !== "exited") return;
      if (event.key === "Enter") {
        event.preventDefault();
        workspace.restartSession(fsId);
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
    onFocusPane: focusPane,
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
    tabMemory,
    get onRender() {
      return props.onRender;
    },
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
                {(child, index) => (
                  <>
                    <Show when={index > 0}>
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
                        flex: child().weight,
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
                        visible={props.visible}
                        path={[...(props.path ?? []), index]}
                      />
                    </div>
                  </>
                )}
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
                  <BSPPane
                    node={split().children[activeTab()].node}
                    assignments={props.assignments}
                    focusedPaneId={props.focusedPaneId}
                    visible={props.visible}
                    path={[...path(), activeTab()]}
                  />
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
  const surfaceId = () => surfaceParsed()?.surfaceId ?? null;
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
    if (props.sessionId || !props.leaf.command || autoCreated) return;
    if (connection()?.status !== "connected") return;
    autoCreated = true;
    ctx.onCreateInPane?.(props.paneId, props.leaf.command);
  });

  createEffect(() => {
    // Track these dependencies
    const focused = props.isFocused;
    const _sid = props.sessionId;
    const _vis = props.visible;
    if (focused && paneContainer) {
      // Focus the pane container's focusable child.
      // Bare "canvas" is excluded — the terminal canvas has no tabindex so
      // focus() is a no-op.  Surface canvases have tabindex and are matched
      // by the [tabindex] selector.
      const sel = "[tabindex], input, textarea";
      const focusable = paneContainer.querySelector<HTMLElement>(sel);
      if (focusable) {
        focusable.focus();
      } else {
        // BlitTerminal attaches its canvas in onMount which runs after
        // this effect.  Retry once the current reactive flush completes.
        queueMicrotask(() => {
          paneContainer.querySelector<HTMLElement>(sel)?.focus();
        });
      }
    }
  });

  return (
    <div
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
    >
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
            <div ref={paneContainer} style={{ width: "100%", height: "100%" }}>
              <BlitTerminal
                sessionId={props.sessionId}
                fontSize={resolveLeafFontSize(props.leaf, ctx.fontSize)}
                fontFamily={ctx.fontFamily}
                palette={ctx.palette}
                style={{ width: "100%", height: "100%" }}
                showCursor={props.isFocused}
                onRender={ctx.onRender}
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
                  onClick={() => void workspace.closeSession(props.sessionId!)}
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
          <div ref={paneContainer} style={{ width: "100%", height: "100%" }}>
            <BlitSurfaceView
              connectionId={surfaceConnectionId()}
              surfaceId={surfaceId()!}
              focus={props.isFocused}
              resizable
              style={{ width: "100%", height: "100%" }}
            />
          </div>
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
            placeholder={t("bsp.commandPlaceholder")}
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
