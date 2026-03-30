import {
  Fragment,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  BlitTerminal,
  type BlitTerminalHandle,
  useBlitConnection,
  useBlitSessions,
  useBlitWorkspace,
  type SessionId,
  type TerminalPalette,
} from "blit-react";
import type { BSPNode, BSPChild, BSPSplit, BSPLeaf } from "./dsl";
import { leafCount, serializeDSL } from "./dsl";
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
} from "./layout";
import { ResizeHandle } from "./ResizeHandle";
import type { Theme } from "../theme";
import { themeFor, ui, uiScale } from "../theme";
import { t, tp } from "../i18n";

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

export function BSPContainer({
  layout,
  onLayoutChange,
  connectionId,
  palette,
  fontFamily,
  fontSize,
  focusedSessionId,
  lruSessionIds,
  manageVisibility = true,
  preferredEmptyPaneId = null,
  onAssignmentsChange,
  onPreferredEmptyPaneResolved,
  onFocusSession,
  onCreateInPane,
  onFocusBySession,
  onFocusPane: onFocusPaneCb,
  onMoveSessionToPane,
  onFocusedPaneChange,
}: {
  layout: BSPLayout;
  onLayoutChange: (layout: BSPLayout | null) => void;
  connectionId: string;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;

  focusedSessionId: SessionId | null;
  lruSessionIds: readonly SessionId[];
  manageVisibility?: boolean;
  preferredEmptyPaneId?: string | null;
  onAssignmentsChange?: (assignments: BSPAssignments) => void;
  onPreferredEmptyPaneResolved?: () => void;
  onFocusSession: (id: SessionId | null) => void;
  onCreateInPane?: (paneId: string, command?: string) => void;
  /** Called with control functions so the parent can direct pane focus/assignments. */
  onFocusBySession?: (fn: (sessionId: SessionId) => void) => void;
  onFocusPane?: (fn: (paneId: string) => void) => void;
  onMoveSessionToPane?: (
    fn: (sessionId: SessionId, targetPaneId: string) => void,
  ) => void;
  onFocusedPaneChange?: (paneId: string | null) => void;
}) {
  const workspace = useBlitWorkspace();
  const connection = useBlitConnection(connectionId);
  const connected = connection?.status === "connected";
  const sessions = useBlitSessions();
  const liveSessions = useMemo(
    () =>
      sessions.filter(
        (session) =>
          session.connectionId === connectionId && session.state !== "closed",
      ),
    [connectionId, sessions],
  );
  const liveSessionIds = useMemo(
    () => liveSessions.map((session) => session.id),
    [liveSessions],
  );

  const [root, setRoot] = useState(layout.root);
  const panes = useMemo(() => enumeratePanes(root), [root]);
  const paneIds = useMemo(() => panes.map((pane) => pane.id), [panes]);
  const [layoutState, setLayoutState] = useState<BSPAssignments>(() => {
    const hashAssignments = loadAssignmentsFromHash();
    if (hashAssignments) {
      const assignments: Record<string, SessionId | null> = {};
      for (const paneId of paneIds) {
        assignments[paneId] = (hashAssignments[paneId] as SessionId) ?? null;
      }
      return { assignments };
    }
    const orderedSessionIds = buildCandidateOrder({
      liveSessionIds,
      focusedSessionId,
      lruSessionIds,
    });
    return assignSessionsToPanes(panes, orderedSessionIds);
  });

  const lastDslRef = useRef(layout.dsl);
  const lastLayoutRef = useRef(layout);
  const rootRef = useRef(root);
  const layoutStateRef = useRef(layoutState);
  rootRef.current = root;
  layoutStateRef.current = layoutState;

  useEffect(() => {
    if (layout === lastLayoutRef.current) return;

    const currentPanes = enumeratePanes(rootRef.current);
    const currentAssignedInPaneOrder = currentPanes
      .map((pane) => layoutStateRef.current.assignments[pane.id])
      .filter((sessionId): sessionId is SessionId => sessionId != null);
    const orderedSessionIds = buildCandidateOrder({
      liveSessionIds,
      focusedSessionId,
      currentAssignedInPaneOrder,
      lruSessionIds,
    });
    const nextRoot = layout.root;
    const nextPanes = enumeratePanes(nextRoot);

    lastLayoutRef.current = layout;
    lastDslRef.current = layout.dsl;
    setRoot(nextRoot);
    setLayoutState(assignSessionsToPanes(nextPanes, orderedSessionIds));
  }, [focusedSessionId, layout, liveSessionIds, lruSessionIds]);

  const knownSessionIds = useMemo(
    () =>
      sessions.filter((s) => s.connectionId === connectionId).map((s) => s.id),
    [connectionId, sessions],
  );

  useEffect(() => {
    if (!connected) return;
    setLayoutState((previous) => {
      const next = reconcileAssignments({
        panes,
        previous,
        liveSessionIds,
        knownSessionIds,
      });
      return sameAssignments(previous, next) ? previous : next;
    });
  }, [connected, liveSessionIds, knownSessionIds, panes]);

  // When a new session is created targeting a specific pane, assign the first
  // unassigned live session there once it appears.
  useEffect(() => {
    if (!preferredEmptyPaneId || !paneIds.includes(preferredEmptyPaneId))
      return;
    setLayoutState((previous) => {
      if (previous.assignments[preferredEmptyPaneId] != null) return previous;
      const assigned = new Set(
        Object.values(previous.assignments).filter(Boolean),
      );
      const unassigned = liveSessionIds.find((id) => !assigned.has(id));
      if (!unassigned) return previous;
      return {
        assignments: {
          ...previous.assignments,
          [preferredEmptyPaneId]: unassigned,
        },
      };
    });
  }, [preferredEmptyPaneId, liveSessionIds, paneIds]);

  const assignedInPaneOrder = useMemo(
    () =>
      paneIds
        .map((paneId) => layoutState.assignments[paneId])
        .filter((sessionId): sessionId is SessionId => sessionId != null),
    [layoutState.assignments, paneIds],
  );

  // focusedPaneId is the single source of truth for which pane is active.
  const [focusedPaneId, setFocusedPaneId] = useState<string | null>(() => {
    const fromHash = loadFocusedPaneFromHash();
    if (fromHash && paneIds.includes(fromHash)) return fromHash;
    if (!focusedSessionId) return paneIds[0] ?? null;
    return (
      paneIds.find((id) => layoutState.assignments[id] === focusedSessionId) ??
      paneIds[0] ??
      null
    );
  });

  // Derive the focused session from the focused pane.
  const focusedPaneSessionId = focusedPaneId
    ? (layoutState.assignments[focusedPaneId] ?? null)
    : null;

  // Keep focusedPaneId valid when panes change.
  if (focusedPaneId != null && !paneIds.includes(focusedPaneId)) {
    setFocusedPaneId(paneIds[0] ?? null);
  }

  // Push our derived session up to Workspace.
  useEffect(() => {
    if (focusedPaneSessionId !== focusedSessionId) {
      onFocusSession(focusedPaneSessionId);
    }
  }, [focusedPaneSessionId, focusedSessionId, onFocusSession]);

  // Allow Workspace to focus a specific session's pane (e.g. from menu).
  const focusBySession = useCallback(
    (sessionId: SessionId) => {
      const paneId = paneIds.find(
        (id) => layoutState.assignments[id] === sessionId,
      );
      if (paneId) setFocusedPaneId(paneId);
    },
    [layoutState.assignments, paneIds],
  );

  useEffect(() => {
    onFocusBySession?.(focusBySession);
  }, [focusBySession, onFocusBySession]);

  const moveSessionToPane = useCallback(
    (sessionId: SessionId, targetPaneId: string) => {
      setLayoutState((prev) => {
        if (prev.assignments[targetPaneId] === sessionId) return prev;
        return {
          ...prev,
          assignments: {
            ...prev.assignments,
            [targetPaneId]: sessionId,
          },
        };
      });
      setFocusedPaneId(targetPaneId);
    },
    [],
  );

  useEffect(() => {
    onMoveSessionToPane?.(moveSessionToPane);
  }, [moveSessionToPane, onMoveSessionToPane]);

  const focusPane = useCallback((paneId: string) => {
    setFocusedPaneId(paneId);
  }, []);

  // Report focused pane changes.
  useEffect(() => {
    onFocusedPaneChange?.(focusedPaneId);
  }, [focusedPaneId, onFocusedPaneChange]);

  useEffect(() => {
    onFocusPaneCb?.(focusPane);
  }, [focusPane, onFocusPaneCb]);

  // Remember last active tab per tabs container so switching away doesn't reset.
  const tabMemory = useRef<Record<string, number>>({});

  // Ctrl-[ / Ctrl-] to cycle panes. Tabs containers automatically
  // switch to show the focused pane.
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
      if (e.key !== "[" && e.key !== "]") return;
      e.preventDefault();
      const idx = focusedPaneId ? paneIds.indexOf(focusedPaneId) : -1;
      const delta = e.key === "]" ? 1 : -1;
      const next = (idx + delta + paneIds.length) % paneIds.length;
      focusPane(paneIds[next]);
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [focusPane, focusedPaneId, paneIds]);

  useEffect(() => {
    onAssignmentsChange?.(layoutState);
  }, [layoutState, onAssignmentsChange]);

  useEffect(() => {
    if (!manageVisibility) return;
    workspace.setVisibleSessions(assignedInPaneOrder);
  }, [assignedInPaneOrder, knownSessionIds, manageVisibility, workspace]);

  useEffect(() => {
    if (!preferredEmptyPaneId || !onPreferredEmptyPaneResolved) return;
    if (!paneIds.includes(preferredEmptyPaneId)) {
      onPreferredEmptyPaneResolved();
      return;
    }
    if (layoutState.assignments[preferredEmptyPaneId] != null) {
      onPreferredEmptyPaneResolved();
    }
  }, [
    layoutState.assignments,
    onPreferredEmptyPaneResolved,
    paneIds,
    preferredEmptyPaneId,
  ]);

  const updateRoot = useCallback(
    (next: BSPNode) => {
      setRoot(next);
      const dsl = serializeDSL(next);
      const updated: BSPLayout = { ...layout, root: next, dsl };
      lastLayoutRef.current = updated;
      lastDslRef.current = dsl;
      saveActiveLayout(updated);
      onLayoutChange(updated);
    },
    [layout, onLayoutChange],
  );

  const handleResize = useCallback(
    (split: BSPSplit, indexA: number, indexB: number, fraction: number) => {
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
      updateRoot(replaceNode(rootRef.current));
    },
    [updateRoot],
  );

  useEffect(() => {
    const handler = (event: KeyboardEvent) => {
      if (!focusedSessionId) return;
      const session = liveSessions.find((item) => item.id === focusedSessionId);
      if (!session || session.state !== "exited") return;
      if (event.key === "Enter") {
        event.preventDefault();
        workspace.restartSession(focusedSessionId);
      } else if (event.key === "Escape") {
        event.preventDefault();
        void workspace.closeSession(focusedSessionId);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [focusedSessionId, liveSessions, workspace]);

  const multiPane = leafCount(root) > 1;

  return (
    <div style={{ width: "100%", height: "100%", display: "flex" }}>
      <BSPPane
        node={root}
        assignments={layoutState.assignments}
        connectionId={connectionId}
        multiPane={multiPane}
        focusedPaneId={focusedPaneId}
        onFocusPane={focusPane}
        onCreateInPane={onCreateInPane}
        onResize={handleResize}
        palette={palette}
        fontFamily={fontFamily}
        fontSize={fontSize}
        visible={manageVisibility}
        tabMemory={tabMemory}
      />
    </div>
  );
}

function BSPPane({
  node,
  assignments,
  connectionId,
  multiPane,
  focusedPaneId,
  onFocusPane,
  onCreateInPane,
  onResize,
  palette,
  fontFamily,
  fontSize,
  visible,
  tabMemory,
  path = [],
}: {
  node: BSPNode;
  assignments: Record<string, SessionId | null>;
  connectionId: string;
  multiPane: boolean;
  focusedPaneId: string | null;
  onFocusPane: (paneId: string) => void;
  onCreateInPane?: (paneId: string, command?: string) => void;
  onResize: (
    split: BSPSplit,
    indexA: number,
    indexB: number,
    fraction: number,
  ) => void;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;

  visible: boolean;
  tabMemory: React.RefObject<Record<string, number>>;
  path?: number[];
}) {
  const nodeRef = useRef(node);
  nodeRef.current = node;
  const onResizeRef = useRef(onResize);
  onResizeRef.current = onResize;

  if (node.type === "leaf") {
    const paneId = path.length > 0 ? path.join(".") : "0";
    return (
      <LeafPane
        paneId={paneId}
        leaf={node}
        sessionId={assignments[paneId] ?? null}
        connectionId={connectionId}
        multiPane={multiPane}
        isFocused={paneId === focusedPaneId}
        onFocusPane={() => onFocusPane(paneId)}
        onCreateInPane={onCreateInPane}
        palette={palette}
        fontFamily={fontFamily}
        fontSize={fontSize}
        visible={visible}
      />
    );
  }

  const paneProps = (child: BSPChild, index: number) => ({
    node: child.node,
    assignments,
    connectionId,
    multiPane,
    focusedPaneId,
    onFocusPane,
    onCreateInPane,
    onResize,
    palette,
    fontFamily,
    fontSize,
    visible,
    tabMemory,
    path: [...path, index],
  });

  if (node.direction === "tabs") {
    const theme = themeFor(palette);
    const scale = uiScale(fontSize);

    // Derive active tab from which child contains the focused pane,
    // falling back to the last remembered tab for this container.
    const tabKey = path.join(".") || "root";
    const focusedPrefix = focusedPaneId ?? "";
    let activeTab = -1;
    for (let i = 0; i < node.children.length; i++) {
      const childPrefix = [...path, i].join(".");
      if (
        focusedPrefix === childPrefix ||
        focusedPrefix.startsWith(childPrefix + ".")
      ) {
        activeTab = i;
        break;
      }
    }
    if (activeTab >= 0) {
      tabMemory.current![tabKey] = activeTab;
    } else {
      activeTab = Math.min(
        tabMemory.current![tabKey] ?? 0,
        node.children.length - 1,
      );
    }

    const tabLabel = (child: BSPChild, index: number): string => {
      if (child.label) return child.label;
      if (child.node.type === "leaf" && child.node.tag) return child.node.tag;
      return tp("bsp.tab", { index: index + 1 });
    };

    return (
      <div
        style={{
          display: "flex",
          flexDirection: "column",
          width: "100%",
          height: "100%",
        }}
      >
        <div
          style={{
            display: "flex",
            gap: 1,
            flexShrink: 0,
            backgroundColor: theme.solidPanelBg,
            borderBottom: `1px solid ${theme.subtleBorder}`,
            fontSize: scale.sm,
          }}
        >
          {node.children.map((child, index) => {
            const childPath = [...path, index].join(".");
            return (
              <button
                key={index}
                onClick={() => onFocusPane(childPath)}
                style={{
                  ...ui.btn,
                  flex: 1,
                  minWidth: 0,
                  padding: `${scale.controlY}px ${scale.controlX}px`,
                  fontSize: scale.sm,
                  textAlign: "center" as const,
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap" as const,
                  opacity: index === activeTab ? 1 : 0.5,
                  borderBottom:
                    index === activeTab
                      ? `1px solid ${theme.accent}`
                      : "1px solid transparent",
                }}
              >
                {tabLabel(child, index)}
              </button>
            );
          })}
        </div>
        <div
          style={{
            flex: 1,
            overflow: "hidden",
            position: "relative",
            minHeight: 0,
          }}
        >
          <BSPPane
            key={activeTab}
            {...paneProps(node.children[activeTab], activeTab)}
          />
        </div>
      </div>
    );
  }

  const flexDirection = node.direction === "horizontal" ? "row" : "column";

  return (
    <div
      style={{ display: "flex", flexDirection, width: "100%", height: "100%" }}
    >
      {node.children.map((child, index) => (
        <Fragment key={index}>
          {index > 0 && (
            <ResizeHandle
              direction={node.direction as "horizontal" | "vertical"}
              onDrag={(fraction) =>
                onResizeRef.current(
                  nodeRef.current as BSPSplit,
                  index - 1,
                  index,
                  fraction,
                )
              }
            />
          )}
          <div
            style={{
              flex: child.weight,
              overflow: "hidden",
              position: "relative",
              minWidth: 0,
              minHeight: 0,
            }}
          >
            <BSPPane {...paneProps(child, index)} />
          </div>
        </Fragment>
      ))}
    </div>
  );
}

function LeafPane({
  paneId,
  leaf,
  sessionId,
  connectionId,
  multiPane,
  isFocused,
  onFocusPane,
  onCreateInPane,
  palette,
  fontFamily,
  fontSize,
  visible,
}: {
  paneId: string;
  leaf: BSPLeaf;
  sessionId: SessionId | null;
  connectionId: string;
  multiPane: boolean;
  isFocused: boolean;
  onFocusPane: () => void;
  onCreateInPane?: (paneId: string, command?: string) => void;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;

  visible: boolean;
}) {
  const theme = themeFor(palette);
  const scale = uiScale(fontSize);
  const workspace = useBlitWorkspace();
  const sessions = useBlitSessions();
  const session = sessions.find((item) => item.id === sessionId) ?? null;
  const connection = useBlitConnection(connectionId);
  const termRef = useRef<BlitTerminalHandle>(null);
  const autoCreatedRef = useRef(false);

  useEffect(() => {
    if (sessionId || !leaf.command || autoCreatedRef.current) return;
    if (connection?.status !== "connected") return;
    autoCreatedRef.current = true;
    onCreateInPane?.(paneId, leaf.command);
  }, [sessionId, leaf.command, connection?.status, onCreateInPane, paneId]);

  useEffect(() => {
    if (isFocused && termRef.current) termRef.current.focus();
  }, [isFocused, sessionId, visible]);

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        position: "relative",
        border: multiPane
          ? isFocused
            ? `1px solid ${theme.accent}`
            : `1px solid transparent`
          : "none",
      }}
      onPointerDownCapture={onFocusPane}
      onFocusCapture={onFocusPane}
    >
      {sessionId && session ? (
        <>
          <BlitTerminal
            ref={termRef}
            sessionId={sessionId}
            fontSize={resolveLeafFontSize(leaf, fontSize)}
            fontFamily={fontFamily}
            palette={palette}
            style={{ width: "100%", height: "100%" }}
            showCursor={isFocused}
          />
          {session?.state === "exited" && (
            <div
              style={{
                position: "absolute",
                bottom: 8,
                left: "50%",
                transform: "translateX(-50%)",
                background: theme.solidPanelBg,
                border: `1px solid ${theme.border}`,
                padding: `${scale.controlY}px ${scale.controlX}px`,
                fontSize: scale.sm,
                display: "flex",
                alignItems: "center",
                gap: scale.gap,
              }}
            >
              <mark
                style={{
                  ...ui.badge,
                  backgroundColor: "rgba(255,100,100,0.3)",
                }}
              >
                {t("bsp.exited")}
              </mark>
              {connection?.supportsRestart && (
                <button
                  onClick={() => workspace.restartSession(sessionId)}
                  style={{ ...ui.btn, fontSize: scale.sm }}
                >
                  {t("bsp.restart")} <kbd style={ui.kbd}>Enter</kbd>
                </button>
              )}
              <button
                onClick={() => void workspace.closeSession(sessionId)}
                style={{ ...ui.btn, fontSize: scale.sm, opacity: 0.5 }}
              >
                {t("bsp.close")} <kbd style={ui.kbd}>Esc</kbd>
              </button>
            </div>
          )}
        </>
      ) : connection?.status === "connected" ? (
        <EmptyPane
          paneId={paneId}
          label={leaf.tag || null}
          isFocused={isFocused}
          theme={theme}
          palette={palette}
          fontSize={fontSize}
          onCreateInPane={onCreateInPane}
        />
      ) : (
        <div
          style={{
            width: "100%",
            height: "100%",
            backgroundColor: `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`,
          }}
        />
      )}
    </div>
  );
}

function EmptyPane({
  paneId,
  label,
  isFocused,
  theme,
  palette,
  fontSize,
  onCreateInPane,
}: {
  paneId: string;
  label: string | null;
  isFocused: boolean;
  theme: Theme;
  palette: TerminalPalette;
  fontSize: number;
  onCreateInPane?: (paneId: string, command?: string) => void;
}) {
  const [cmd, setCmd] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);
  const scale = uiScale(fontSize);
  useEffect(() => {
    if (isFocused) inputRef.current?.focus();
  }, [isFocused]);

  return (
    <div
      onClick={() => inputRef.current?.focus()}
      style={{
        width: "100%",
        height: "100%",
        position: "relative",
        backgroundColor: `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`,
      }}
    >
      <div
        style={{
          position: "absolute",
          bottom: 8,
          left: "50%",
          transform: "translateX(-50%)",
          background: theme.solidPanelBg,
          border: `1px solid ${theme.border}`,
          padding: `${scale.controlY}px ${scale.controlX}px`,
          fontSize: scale.sm,
          display: "flex",
          alignItems: "center",
          gap: scale.gap,
        }}
      >
        <mark style={ui.badge}>{t("bsp.empty")}</mark>
        <input
          ref={inputRef}
          type="text"
          value={cmd}
          onChange={(e) => setCmd(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              onCreateInPane?.(paneId, cmd.trim() || undefined);
            }
          }}
          placeholder={t("bsp.commandPlaceholder")}
          style={{
            ...ui.input,
            flex: "none",
            background: "transparent",
            border: "none",
            color: "inherit",
            padding: 0,
            fontSize: scale.sm,
            fontFamily: "inherit",
            width: "min(50vw, 220px)",
          }}
        />
      </div>
    </div>
  );
}
