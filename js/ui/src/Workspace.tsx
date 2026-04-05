import {
  createSignal,
  createEffect,
  createMemo,
  onMount,
  onCleanup,
  Show,
  For,
} from "solid-js";
import {
  BlitTerminal,
  BlitSurfaceView,
  BlitWorkspaceProvider,
  createBlitWorkspace,
  createBlitSessions,
  createBlitWorkspaceState,
  createBlitWorkspaceConnection,
} from "@blit-sh/solid";
import { BlitWorkspace, PALETTES, DEFAULT_FONT } from "@blit-sh/core";
import type {
  BlitTransport,
  BlitSession,
  BlitSurface,
  BlitWasmModule,
  SessionId,
  TerminalPalette,
  ConnectionId,
} from "@blit-sh/core";
import type { ConnectionSpec } from "./App";
import { createMetrics } from "./createMetrics";
import { createFontLoader } from "./createFontLoader";
import { createKeyboardShortcuts } from "./createKeyboardShortcuts";
import {
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  writeStorage,
  useConfigValue,
  preferredPalette,
  preferredFont,
  preferredFontSize,
  blitHost,
  basePath,
} from "./storage";
import type { UIScale, Theme } from "./theme";
import { themeFor, layout, ui, uiScale, z } from "./theme";
import { t } from "./i18n";
import { StatusBar, statusBarBg, statusBarFg } from "./StatusBar";
import { SwitcherOverlay } from "./SwitcherOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { BSPContainer } from "./bsp/BSPContainer";
import type { BSPAssignments, BSPLayout } from "./bsp/layout";
import {
  loadActiveLayout,
  saveActiveLayout,
  saveToHistory,
  loadRecentLayouts,
  PRESETS,
  surfaceAssignment,
  isSurfaceAssignment,
  parseSurfaceAssignment,
} from "./bsp/layout";

export type Overlay = "expose" | "palette" | "font" | "help" | null;

export function Workspace(props: {
  connections: ConnectionSpec[];
  wasm: BlitWasmModule;
  onAuthError: () => void;
}) {
  const workspace = new BlitWorkspace({ wasm: props.wasm });
  for (const conn of props.connections) {
    createBlitWorkspaceConnection(workspace, conn.id, conn.transport);
  }

  return (
    <BlitWorkspaceProvider workspace={workspace}>
      <WorkspaceScreen
        connectionSpecs={props.connections}
        onAuthError={props.onAuthError}
      />
    </BlitWorkspaceProvider>
  );
}

function WorkspaceScreen(props: {
  connectionSpecs: ConnectionSpec[];
  onAuthError: () => void;
}) {
  const workspace = createBlitWorkspace();
  const wsState = createBlitWorkspaceState(workspace);
  const sessions = createBlitSessions(workspace);

  /** Connection ID labels from the CLI config. */
  const connectionLabels = new Map<string, string>(
    props.connectionSpecs.map((c) => [c.id, c.label]),
  );
  const multiConnection = props.connectionSpecs.length > 1;
  const defaultConnectionId = props.connectionSpecs[0]?.id ?? "main";

  const focusedSession = () => {
    const snap = wsState();
    if (!snap.focusedSessionId) return null;
    return snap.sessions.find((s) => s.id === snap.focusedSessionId) ?? null;
  };

  /** The connection that owns the currently focused session (or the first). */
  const activeConnectionId = (): ConnectionId => {
    const fs = focusedSession();
    return fs?.connectionId ?? defaultConnectionId;
  };

  const connection = () => {
    const snap = wsState();
    return snap.connections.find((c) => c.id === activeConnectionId()) ?? null;
  };

  /** All connections from snapshot. */
  const allConnections = () => wsState().connections;

  const [surfaces, setSurfaces] = createSignal<BlitSurface[]>([]);

  // Aggregate surfaces from all connections.
  createEffect(() => {
    const cleanups: (() => void)[] = [];
    const syncAll = () => {
      const all: BlitSurface[] = [];
      for (const spec of props.connectionSpecs) {
        const conn = workspace.getConnection(spec.id);
        if (!conn) continue;
        for (const s of conn.surfaceStore.getSurfaces().values()) {
          all.push(s);
        }
      }
      setSurfaces(all);
    };
    for (const spec of props.connectionSpecs) {
      const conn = workspace.getConnection(spec.id);
      if (!conn) continue;
      cleanups.push(conn.surfaceStore.onChange(syncAll));
    }
    syncAll();
    onCleanup(() => cleanups.forEach((fn) => fn()));
  });

  const [palette, setPalette] =
    createSignal<TerminalPalette>(preferredPalette());
  const [font, setFont] = createSignal(preferredFont());
  const [fontSize, setFontSize] = createSignal(preferredFontSize());
  const [overlay, setOverlay] = createSignal<Overlay>(null);
  const [debugPanel, setDebugPanel] = createSignal(false);
  const [previewPanelOpen, setPreviewPanelOpen] = createSignal(true);
  const [previewPanelWidth, setPreviewPanelWidth] =
    createSignal(SURFACE_PANEL_WIDTH);
  // Parse focus params from URL hash on init.
  // Surface: s=<connectionId>:<surfaceId>
  // Terminal: t=<sessionId>  (sessionId is already "<connectionId>:<counter>")
  const initHash = new URLSearchParams(
    location.hash.slice(1).replace(/&/g, "&"),
  );
  const hashSurface = initHash.get("s");
  const hashTerminal = initHash.get("t");

  // s= and t= are mutually exclusive; s= takes priority.
  const pendingSurfaceFromHash = (() => {
    if (!hashSurface) return null;
    const sep = hashSurface.indexOf(":");
    if (sep < 0) return null;
    const surfaceId = Number(hashSurface.slice(sep + 1));
    return Number.isFinite(surfaceId) ? surfaceId : null;
  })();

  const [focusedSurfaceId, setFocusedSurfaceId] = createSignal<number | null>(
    null,
  );

  // Restore surface focus from hash once the surface actually exists (one-shot).
  if (pendingSurfaceFromHash != null) {
    let surfaceRestored = false;
    createEffect(() => {
      if (surfaceRestored) return;
      const ss = surfaces();
      if (ss.some((s) => s.surfaceId === pendingSurfaceFromHash)) {
        surfaceRestored = true;
        setFocusedSurfaceId(pendingSurfaceFromHash);
      }
    });
  }

  // Restore terminal focus from hash once sessions are available (one-shot).
  // Only if no surface focus was requested.
  if (hashTerminal && pendingSurfaceFromHash == null) {
    let terminalRestored = false;
    createEffect(() => {
      if (terminalRestored) return;
      const ss = sessions();
      if (ss.length === 0) return;
      const match = ss.find((s) => s.id === hashTerminal);
      if (match) {
        terminalRestored = true;
        workspace.focusSession(match.id);
      }
    });
  }
  const [serverFonts, setServerFonts] = createSignal<string[]>([]);
  const { resolvedFont, fontLoading, advanceRatio } = createFontLoader(
    font,
    DEFAULT_FONT,
  );
  const [activeLayout, setActiveLayout] = createSignal<BSPLayout | null>(
    loadActiveLayout(),
  );
  const [layoutAssignments, setLayoutAssignments] =
    createSignal<BSPAssignments | null>(null);

  // Clear focused surface if it was destroyed.
  createEffect(() => {
    const fid = focusedSurfaceId();
    if (fid == null) return;
    const exists = surfaces().some((s) => s.surfaceId === fid);
    if (!exists) setFocusedSurfaceId(null);
  });

  const offScreenSurfaces = createMemo(() => {
    const fid = focusedSurfaceId();
    // Collect surface IDs assigned to BSP panes.
    const la = layoutAssignments();
    const inPane = new Set<number>();
    if (la) {
      for (const v of Object.values(la.assignments)) {
        if (v && isSurfaceAssignment(v)) {
          const id = parseInt(v.slice("surface:".length), 10);
          if (Number.isFinite(id)) inPane.add(id);
        }
      }
    }
    return surfaces().filter(
      (s) => s.surfaceId !== fid && !inPane.has(s.surfaceId),
    );
  });

  const offScreenSessions = createMemo(() => {
    const al = activeLayout();
    const la = layoutAssignments();
    const sess = sessions();
    if (al) {
      const assigned = new Set<SessionId>(
        la
          ? Object.values(la.assignments).filter(
              (id): id is SessionId => id != null,
            )
          : [],
      );
      return sess.filter((s) => s.state !== "closed" && !assigned.has(s.id));
    }
    // When a surface is focused the terminal it displaced is off-screen.
    if (focusedSurfaceId() != null) {
      return sess.filter((s) => s.state !== "closed");
    }
    return sess.filter(
      (s) => s.state !== "closed" && s.id !== wsState().focusedSessionId,
    );
  });

  function toggleDebug() {
    setDebugPanel((v) => !v);
  }
  function togglePreviewPanel() {
    setPreviewPanelOpen((v) => !v);
  }

  let paletteOverlayOrigin: TerminalPalette | null = null;
  let fontOverlayOrigin: { family: string; size: number } | null = null;

  const remotePaletteId = useConfigValue(PALETTE_KEY);
  const remoteFont = useConfigValue(FONT_KEY);
  const remoteFontSize = useConfigValue(FONT_SIZE_KEY);

  createEffect(() => {
    const id = remotePaletteId();
    if (!id) return;
    const p = PALETTES.find((x) => x.id === id);
    if (p) setPalette(p);
  });

  createEffect(() => {
    const f = remoteFont();
    if (f?.trim()) setFont(f.trim());
  });

  createEffect(() => {
    const s = remoteFontSize();
    if (!s) return;
    const n = parseInt(s, 10);
    if (n > 0) setFontSize(n);
  });

  const resolvedFontWithFallback = () => {
    const rf = resolvedFont();
    return rf === DEFAULT_FONT ? rf : `${rf}, ${DEFAULT_FONT}`;
  };

  onMount(() => {
    fetch(`${basePath}fonts`)
      .then((r) => (r.ok ? r.json() : []))
      .then(setServerFonts)
      .catch(() => {});
  });

  let lru: SessionId[] = [];

  createEffect(() => {
    const fid = wsState().focusedSessionId;
    if (!fid) return;
    lru = [fid, ...lru.filter((id) => id !== fid)];
  });

  createEffect(() => {
    if (activeLayout()) return;
    setLayoutAssignments(null);
  });

  // Visibility management
  createEffect(() => {
    const al = activeLayout();
    const ov = overlay();
    if (al && ov !== "expose") return;
    const desired = new Set<SessionId>();
    const fid = wsState().focusedSessionId;
    if (fid) desired.add(fid);
    for (const s of offScreenSessions()) desired.add(s.id);
    if (ov === "expose") {
      for (const session of sessions()) {
        if (session.state !== "closed") desired.add(session.id);
      }
    }
    workspace.setVisibleSessions(desired);
  });

  // Auth error — trigger if any connection has an auth error.
  createEffect(() => {
    const conns = allConnections();
    if (conns.some((c) => c.error === "auth")) props.onAuthError();
  });

  // Debounce connected status — worst status across all connections.
  const rawStatus = () => {
    const conns = allConnections();
    if (conns.length === 0) return "disconnected" as const;
    // If any connection is in error/disconnected, show that.
    for (const s of [
      "error",
      "disconnected",
      "closed",
      "connecting",
      "authenticating",
    ] as const) {
      if (conns.some((c) => c.status === s)) return s;
    }
    return "connected" as const;
  };
  const [stableStatus, setStableStatus] = createSignal(rawStatus());
  createEffect(() => {
    const rs = rawStatus();
    if (rs !== "connected") {
      setStableStatus(rs);
      return;
    }
    const timer = setTimeout(() => setStableStatus("connected"), 500);
    onCleanup(() => clearTimeout(timer));
  });

  // Theme on document
  createEffect(() => {
    document.documentElement.setAttribute(
      "data-theme",
      palette().dark ? "dark" : "light",
    );
  });

  onMount(() => {
    document.documentElement.style.fontFamily = "system-ui, sans-serif";
  });

  // Title
  createEffect(() => {
    const host = blitHost();
    const parts: string[] = [];
    const fs = focusedSession();
    if (fs?.title) parts.push(fs.title);
    if (host && host !== "localhost" && host !== "127.0.0.1") parts.push(host);
    parts.push("blit");
    document.title = parts.join(" \u2014 ");
  });

  let previousFocus: Element | null = null;

  // Auto-focus the terminal or surface canvas when the overlay closes.
  // Skip when a BSP layout is active — BSPContainer manages its own DOM
  // focus per-pane. Running here would always focus the first canvas in DOM
  // order (pane 1) because document.querySelector returns the first match.
  createEffect(() => {
    if (overlay()) return; // overlay is open, skip
    if (activeLayout()) return; // BSP manages its own focus
    const sid = wsState().focusedSessionId;
    const surfId = focusedSurfaceId();
    if (!sid && surfId == null) return; // nothing to focus
    // Defer until Solid commits the DOM update.
    setTimeout(() => {
      const el = document.querySelector<HTMLElement>(
        "section textarea[tabindex], section canvas[tabindex]",
      );
      el?.focus();
    }, 16);
  });

  function closeOverlay() {
    paletteOverlayOrigin = null;
    fontOverlayOrigin = null;
    setOverlay(null);
    const el = previousFocus;
    previousFocus = null;
    if (el instanceof HTMLElement) setTimeout(() => el.focus(), 0);
  }

  function restoreOverlayPreview(target: Overlay) {
    if (target === "palette" && paletteOverlayOrigin) {
      setPalette(paletteOverlayOrigin);
      paletteOverlayOrigin = null;
    } else if (target === "font" && fontOverlayOrigin) {
      setFont(fontOverlayOrigin.family);
      setFontSize(fontOverlayOrigin.size);
      fontOverlayOrigin = null;
    }
  }

  function cancelOverlay() {
    restoreOverlayPreview(overlay());
    closeOverlay();
  }

  function toggleOverlay(target: Overlay) {
    const current = overlay();
    if (current === target) {
      cancelOverlay();
      return;
    }
    restoreOverlayPreview(current);
    if (!current) previousFocus = document.activeElement;
    if (target === "palette") {
      paletteOverlayOrigin = palette();
    } else if (target === "font") {
      fontOverlayOrigin = { family: font(), size: fontSize() };
    }
    setOverlay(target);
  }

  function changePalette(nextPalette: TerminalPalette) {
    setPalette(nextPalette);
    paletteOverlayOrigin = null;
    writeStorage(PALETTE_KEY, nextPalette.id);
    closeOverlay();
  }

  function changeFont(family: string, size: number) {
    const value = family.trim() || DEFAULT_FONT;
    setFont(value);
    setFontSize(size);
    fontOverlayOrigin = null;
    writeStorage(FONT_KEY, value);
    writeStorage(FONT_SIZE_KEY, String(size));
    closeOverlay();
  }

  let focusBySessionFn: ((sessionId: SessionId) => void) | null = null;
  let moveSessionToPaneFn:
    | ((sessionId: SessionId, targetPaneId: string) => void)
    | null = null;
  let moveToPaneFn: ((value: string, targetPaneId: string) => void) | null =
    null;
  let focusPaneFn: ((paneId: string) => void) | null = null;
  const [bspFocusedPaneId, setBspFocusedPaneId] = createSignal<string | null>(
    null,
  );

  function switchSession(sessionId: SessionId) {
    setFocusedSurfaceId(null);
    workspace.focusSession(sessionId);
    focusBySessionFn?.(sessionId);
    previousFocus = null;
    closeOverlay();
  }

  function focusSurface(surfaceId: number) {
    // When a BSP layout is active, place the surface into the focused pane.
    if (activeLayout() && bspFocusedPaneId()) {
      moveToPaneFn?.(surfaceAssignment(surfaceId), bspFocusedPaneId()!);
      setFocusedSurfaceId(null);
    } else {
      setFocusedSurfaceId(surfaceId);
    }
    closeOverlay();
  }

  let termHandle: { rows: number; cols: number; focus: () => void } | null =
    null;

  async function createAndFocus(command?: string) {
    try {
      const fid = wsState().focusedSessionId;
      const session = await workspace.createSession({
        connectionId: activeConnectionId(),
        rows: termHandle?.rows ?? 24,
        cols: termHandle?.cols ?? 80,
        ...(command ? { command } : {}),
        ...(!command && fid ? { cwdFromSessionId: fid } : {}),
      });
      setFocusedSurfaceId(null);
      workspace.focusSession(session.id);
      previousFocus = null;
      closeOverlay();
    } catch {}
  }

  async function createInPane(paneId: string, command?: string) {
    try {
      const fid = wsState().focusedSessionId;
      const session = await workspace.createSession({
        connectionId: activeConnectionId(),
        rows: termHandle?.rows ?? 24,
        cols: termHandle?.cols ?? 80,
        ...(command ? { command } : {}),
        ...(!command && fid ? { cwdFromSessionId: fid } : {}),
      });
      moveSessionToPaneFn?.(session.id, paneId);
      workspace.focusSession(session.id);
    } catch {}
  }

  function selectPane(
    paneId: string,
    sessionId: SessionId | null,
    command?: string,
  ) {
    if (sessionId && !command) {
      workspace.focusSession(sessionId);
      focusBySessionFn?.(sessionId);
    } else {
      void createInPane(paneId, command);
    }
    closeOverlay();
  }

  function handleRestartOrClose() {
    const fs = focusedSession();
    if (!fs) {
      void createAndFocus();
      return;
    }
    if (fs.state !== "exited") return;
    if (connection()?.supportsRestart) {
      workspace.restartSession(fs.id);
    } else {
      void workspace.closeSession(fs.id);
    }
  }

  createKeyboardShortcuts({
    workspace,
    overlay,
    activeLayout,
    bspFocusedPaneId,
    focusedSession,
    sessions,
    focusedSessionId: () => wsState().focusedSessionId,
    supportsRestart: () => connection()?.supportsRestart ?? false,
    focusedSurfaceId,
    closeSurface: (surfaceId: number) => {
      workspace.closeSurface(activeConnectionId(), surfaceId);
    },
    unfocusSurface: () => {
      setFocusedSurfaceId(null);
    },
    toggleOverlay,
    cancelOverlay,
    toggleDebug,
    togglePreviewPanel,
    createAndFocus,
    createInPane,
    handleRestartOrClose,
  });

  // Set font defaults on connection
  createEffect(() => {
    const conn = workspace.getConnection(activeConnectionId());
    if (!conn) return;
    const dpr = window.devicePixelRatio || 1;
    conn.setFontSize(fontSize() * dpr);
    conn.setFontFamily(resolvedFontWithFallback());
  });

  // Sync layout + focus to URL hash
  createEffect(() => {
    if (connection()?.status !== "connected") return;
    const parts: string[] = [];
    const al = activeLayout();
    const paneId = bspFocusedPaneId();
    const la = layoutAssignments();
    if (al) parts.push(`l=${al.dsl}`);
    if (paneId) parts.push(`p=${paneId}`);
    if (la) {
      const a = Object.entries(la.assignments)
        .filter(([, sid]) => sid != null)
        .map(([pane, sid]) => {
          const s = sessions().find((s) => s.id === sid);
          return s ? `${pane}:${s.connectionId}:${s.ptyId}` : null;
        })
        .filter(Boolean)
        .join(",");
      if (a) parts.push(`a=${a}`);
    }
    const fSurface = focusedSurfaceId();
    if (fSurface != null) parts.push(`s=${activeConnectionId()}:${fSurface}`);
    const fTerminal = wsState().focusedSessionId;
    if (fTerminal && fSurface == null) parts.push(`t=${fTerminal}`);
    const existing = location.hash.slice(1);
    // Only strip hash keys we are actively replacing.  During the window
    // between "connected" and S2C_LIST arrival the BSP container hasn't
    // resolved pending-hash assignments yet, so `a=` would be empty.
    // Stripping the old `a=` here would lose assignments if the user
    // reloads during that window (or on reconnect/remount).
    const written = new Set(parts.map((p) => p.slice(0, p.indexOf("="))));
    const kept = existing
      .split("&")
      .filter(
        (s) =>
          s &&
          !(/^[lpast]=/.test(s) && written.has(s.slice(0, s.indexOf("=")))),
      );
    const merged = [...kept, ...parts];
    if (merged.length > 0) {
      history.replaceState(null, "", `#${merged.join("&")}`);
    }
  });

  const { countFrame, timeline, net, metrics } = createMetrics(
    props.connectionSpecs
      .map((s) => workspace.getConnection(s.id)?.transport)
      .filter((t): t is NonNullable<typeof t> => t != null),
  );
  const theme = () => themeFor(palette());
  const chromeScale = () => uiScale(fontSize());
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette()}
      fontFamily={resolvedFontWithFallback()}
      fontSize={fontSize()}
      advanceRatio={advanceRatio()}
    >
      <main
        style={{
          ...layout.workspace,
          "background-color": theme().bg,
          color: theme().fg,
          "font-family": resolvedFontWithFallback(),
        }}
      >
        <section
          style={{
            ...layout.termContainer,
            display: "flex",
            "flex-direction": "row",
          }}
        >
          <div style={{ flex: 1, overflow: "hidden", position: "relative" }}>
            <Show
              when={activeLayout()}
              fallback={
                <Show
                  when={focusedSurfaceId()}
                  fallback={
                    <Show
                      when={wsState().focusedSessionId}
                      fallback={
                        <EmptyState
                          theme={theme()}
                          scale={chromeScale()}
                          mod={mod}
                          onCreate={() => void createAndFocus()}
                          onSwitcher={() => toggleOverlay("expose")}
                          onHelp={() => toggleOverlay("help")}
                        />
                      }
                    >
                      {(fid) => (
                        <>
                          <BlitTerminal
                            sessionId={fid()}
                            onRender={countFrame}
                            style={{ width: "100%", height: "100%" }}
                            fontFamily={resolvedFontWithFallback()}
                            fontSize={fontSize()}
                            palette={palette()}
                          />
                          <Show when={focusedSession()?.state === "exited"}>
                            <div
                              style={{
                                position: "absolute",
                                bottom: "32px",
                                left: "50%",
                                transform: "translateX(-50%)",
                                "background-color": theme().solidPanelBg,
                                border: `1px solid ${theme().border}`,
                                padding: `${chromeScale().controlY}px ${chromeScale().controlX}px`,
                                "font-size": `${chromeScale().sm}px`,
                                "z-index": z.exitedBanner,
                                display: "flex",
                                "align-items": "center",
                                gap: `${chromeScale().gap}px`,
                              }}
                            >
                              <mark
                                style={{
                                  ...ui.badge,
                                  "background-color": "rgba(255,100,100,0.3)",
                                }}
                              >
                                {t("workspace.exited")}
                              </mark>
                              <Show when={connection()?.supportsRestart}>
                                <button
                                  onClick={() => handleRestartOrClose()}
                                  style={{
                                    ...ui.btn,
                                    "font-size": `${chromeScale().md}px`,
                                  }}
                                >
                                  {t("workspace.restart")}{" "}
                                  <kbd style={ui.kbd}>Enter</kbd>
                                </button>
                              </Show>
                              <button
                                onClick={() => {
                                  const fs = focusedSession();
                                  if (fs) void workspace.closeSession(fs.id);
                                }}
                                style={{
                                  ...ui.btn,
                                  "font-size": `${chromeScale().md}px`,
                                  opacity: 0.5,
                                }}
                              >
                                {t("workspace.close")}{" "}
                                <kbd style={ui.kbd}>Esc</kbd>
                              </button>
                            </div>
                          </Show>
                        </>
                      )}
                    </Show>
                  }
                >
                  {(sid) => (
                    <BlitSurfaceView
                      connectionId={activeConnectionId()}
                      surfaceId={sid()}
                      focus
                      resizable
                      style={{
                        width: "100%",
                        height: "100%",
                      }}
                    />
                  )}
                </Show>
              }
            >
              {(al) => (
                <BSPContainer
                  layout={al()}
                  onLayoutChange={setActiveLayout}
                  connectionId={activeConnectionId()}
                  palette={palette()}
                  fontFamily={resolvedFontWithFallback()}
                  fontSize={fontSize()}
                  focusedSessionId={wsState().focusedSessionId}
                  lruSessionIds={lru}
                  liveSurfaceIds={surfaces().map((s) => s.surfaceId)}
                  manageVisibility={overlay() !== "expose"}
                  onAssignmentsChange={setLayoutAssignments}
                  onFocusSession={(id) => workspace.focusSession(id)}
                  onFocusBySession={(fn) => {
                    focusBySessionFn = fn;
                  }}
                  onFocusPane={(fn) => {
                    focusPaneFn = fn;
                  }}
                  onMoveSessionToPane={(fn) => {
                    moveSessionToPaneFn = fn;
                  }}
                  onMoveToPane={(fn) => {
                    moveToPaneFn = fn;
                  }}
                  onFocusedPaneChange={setBspFocusedPaneId}
                  onCreateInPane={createInPane}
                />
              )}
            </Show>
          </div>
          <Show
            when={
              previewPanelOpen() &&
              (offScreenSessions().length > 0 || offScreenSurfaces().length > 0)
            }
          >
            <PreviewPanel
              offScreenSessions={offScreenSessions()}
              surfaces={offScreenSurfaces()}
              focusedSurfaceId={focusedSurfaceId()}
              connectionId={activeConnectionId()}
              theme={theme()}
              scale={chromeScale()}
              palette={palette()}
              fontFamily={resolvedFontWithFallback()}
              fontSize={fontSize()}
              onFocusSession={switchSession}
              onFocusSurface={focusSurface}
              width={previewPanelWidth()}
              onResize={setPreviewPanelWidth}
              onClose={togglePreviewPanel}
            />
          </Show>
        </section>
        <Show when={overlay() === "expose"}>
          <SwitcherOverlay
            sessions={sessions()}
            focusedSessionId={
              focusedSurfaceId() != null ? null : wsState().focusedSessionId
            }
            lru={lru}
            palette={palette()}
            fontFamily={resolvedFontWithFallback()}
            fontSize={fontSize()}
            onSelect={switchSession}
            onClose={closeOverlay}
            onCreate={createAndFocus}
            activeLayout={activeLayout()}
            layoutAssignments={layoutAssignments()}
            onSelectPane={selectPane}
            focusedPaneId={activeLayout() ? bspFocusedPaneId() : null}
            onMoveToPane={(sessionId, targetPaneId) => {
              moveSessionToPaneFn?.(sessionId, targetPaneId);
              workspace.focusSession(sessionId);
              closeOverlay();
            }}
            onApplyLayout={(l) => {
              setActiveLayout(l);
              saveActiveLayout(l);
              saveToHistory(l);
              closeOverlay();
            }}
            onClearLayout={() => {
              setLayoutAssignments(null);
              setActiveLayout(null);
              saveActiveLayout(null);
              closeOverlay();
            }}
            recentLayouts={loadRecentLayouts()}
            presetLayouts={PRESETS}
            onChangeFont={() => toggleOverlay("font")}
            onChangePalette={() => toggleOverlay("palette")}
            surfaces={surfaces()}
            connectionId={activeConnectionId()}
            focusedSurfaceId={focusedSurfaceId()}
            onFocusSurface={focusSurface}
            onMoveSurfaceToPane={(sid, targetPaneId) => {
              moveToPaneFn?.(surfaceAssignment(sid), targetPaneId);
              setFocusedSurfaceId(null);
              closeOverlay();
            }}
          />
        </Show>
        <Show when={overlay() === "palette"}>
          <PaletteOverlay
            current={palette()}
            fontSize={fontSize()}
            onSelect={changePalette}
            onPreview={setPalette}
            onClose={closeOverlay}
          />
        </Show>
        <Show when={overlay() === "font"}>
          <FontOverlay
            currentFamily={font()}
            currentSize={fontSize()}
            serverFonts={serverFonts()}
            palette={palette()}
            fontSize={fontSize()}
            onSelect={changeFont}
            onPreview={(family, size) => {
              setFont(family);
              setFontSize(size);
            }}
            onClose={closeOverlay}
          />
        </Show>
        <Show when={overlay() === "help"}>
          <HelpOverlay
            onClose={closeOverlay}
            palette={palette()}
            fontSize={fontSize()}
          />
        </Show>
        <footer
          style={{
            ...layout.statusBar,
            padding: "0 1em",
            "background-color": statusBarBg(stableStatus(), theme()),
            color: statusBarFg(stableStatus(), theme()),
            "border-top-color": theme().subtleBorder,
            height: `${chromeScale().md + chromeScale().controlY * 2}px`,
            "font-size": `${chromeScale().sm}px`,
          }}
        >
          <StatusBar
            sessions={sessions()}
            surfaceCount={surfaces().length}
            focusedSession={focusedSession()}
            status={stableStatus()}
            retryCount={connection()?.retryCount ?? 0}
            error={connection()?.error ?? null}
            onReconnect={() => {
              for (const spec of props.connectionSpecs) {
                const c = wsState().connections.find((x) => x.id === spec.id);
                if (c && c.status !== "connected") {
                  workspace.reconnectConnection(spec.id);
                }
              }
            }}
            metrics={metrics()}
            palette={palette()}
            fontSize={fontSize()}
            termSize={null}
            fontLoading={fontLoading()}
            debug={debugPanel()}
            toggleDebug={toggleDebug}
            previewPanelOpen={previewPanelOpen()}
            onPreviewPanel={togglePreviewPanel}
            debugStats={workspace.getConnectionDebugStats(
              activeConnectionId(),
              wsState().focusedSessionId,
            )}
            timeline={timeline}
            net={net}
            onSwitcher={() => toggleOverlay("expose")}
            onPalette={() => toggleOverlay("palette")}
            onFont={() => toggleOverlay("font")}
          />
        </footer>
      </main>
    </BlitWorkspaceProvider>
  );
}

function EmptyState(props: {
  theme: Theme;
  scale: UIScale;
  mod: string;
  onCreate: () => void;
  onSwitcher: () => void;
  onHelp: () => void;
}) {
  return (
    <div
      style={{
        display: "flex",
        "flex-direction": "column",
        "align-items": "center",
        "justify-content": "center",
        height: "100%",
        gap: `${props.scale.gap}px`,
        opacity: 0.6,
      }}
    >
      <div style={{ "font-size": `${props.scale.lg}px` }}>
        {t("workspace.welcome")}
      </div>
      <div
        style={{
          "font-size": `${props.scale.sm}px`,
          display: "flex",
          "flex-direction": "column",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
        }}
      >
        <button
          onClick={props.onCreate}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          {t("workspace.newTerminal")} <kbd style={ui.kbd}>Enter</kbd>{" "}
          <kbd style={ui.kbd}>{props.mod}+Shift+Enter</kbd>
        </button>
        <button
          onClick={props.onSwitcher}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          {t("workspace.menu")} <kbd style={ui.kbd}>{props.mod}+K</kbd>
        </button>
        <button
          onClick={props.onHelp}
          style={{ ...ui.btn, "font-size": `${props.scale.md}px` }}
        >
          {t("workspace.help")} <kbd style={ui.kbd}>Ctrl+?</kbd>
        </button>
      </div>
      <button
        onClick={props.onCreate}
        style={{
          "margin-top": `${props.scale.tightGap}px`,
          padding: `${props.scale.controlY}px ${props.scale.controlX * 2}px`,
          "font-size": `${props.scale.md}px`,
          "background-color": props.theme.accent,
          color: "#fff",
          border: "none",
          cursor: "pointer",
        }}
      >
        {t("workspace.newTerminal")}
      </button>
    </div>
  );
}

const SURFACE_PANEL_WIDTH = 280;
const MIN_PANEL_WIDTH = 160;
const MAX_PANEL_WIDTH = 600;

function PreviewPanel(props: {
  offScreenSessions: BlitSession[];
  surfaces: BlitSurface[];
  focusedSurfaceId: number | null;
  connectionId: string;
  theme: Theme;
  scale: UIScale;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  onFocusSession: (id: SessionId) => void;
  onFocusSurface: (surfaceId: number) => void;
  width: number;
  onResize: (width: number) => void;
  onClose: () => void;
}) {
  const [expandedId, setExpandedId] = createSignal<number | null>(null);
  const [resizeHover, setResizeHover] = createSignal(false);
  const [resizeActive, setResizeActive] = createSignal(false);

  function handleResizePointerDown(e: PointerEvent) {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    setResizeActive(true);
    const startX = e.clientX;
    const startWidth = props.width;

    const onMove = (me: PointerEvent) => {
      const delta = startX - me.clientX;
      props.onResize(
        Math.max(
          MIN_PANEL_WIDTH,
          Math.min(MAX_PANEL_WIDTH, startWidth + delta),
        ),
      );
    };

    const onUp = () => {
      setResizeActive(false);
      document.removeEventListener("pointermove", onMove);
      document.removeEventListener("pointerup", onUp);
    };

    document.addEventListener("pointermove", onMove);
    document.addEventListener("pointerup", onUp);
  }

  const resizeBg = () =>
    resizeActive()
      ? "rgba(128,128,128,0.5)"
      : resizeHover()
        ? "rgba(128,128,128,0.3)"
        : "transparent";

  return (
    <div
      style={{
        width: `${props.width}px`,
        "flex-shrink": 0,
        display: "flex",
        "flex-direction": "row",
        overflow: "hidden",
      }}
    >
      <div
        onPointerDown={handleResizePointerDown}
        onPointerEnter={() => setResizeHover(true)}
        onPointerLeave={() => setResizeHover(false)}
        style={{
          width: "3px",
          "flex-shrink": 0,
          cursor: "col-resize",
          background: resizeBg(),
          "border-left": `1px solid ${props.theme.subtleBorder}`,
          transition: "background 0.1s",
          "touch-action": "none",
        }}
      />
      <div
        style={{
          flex: 1,
          "background-color": props.theme.solidPanelBg,
          display: "flex",
          "flex-direction": "column",
          overflow: "hidden",
        }}
      >
        <div
          style={{
            display: "flex",
            "align-items": "center",
            "justify-content": "flex-end",
            padding: `${props.scale.controlY}px ${props.scale.tightGap}px`,
            "border-bottom": `1px solid ${props.theme.subtleBorder}`,
          }}
        >
          <button
            onClick={props.onClose}
            title="Close panel (Ctrl+Shift+B)"
            style={{
              ...ui.btn,
              "font-size": `${props.scale.xs}px`,
              padding: `0 ${props.scale.tightGap}px`,
              opacity: 0.5,
            }}
          >
            {"\u00D7"}
          </button>
        </div>
        <div style={{ flex: "1 1 0", "min-height": 0, "overflow-y": "auto" }}>
          <For each={props.offScreenSessions}>
            {(s) => (
              <SessionThumbnail
                session={s}
                theme={props.theme}
                scale={props.scale}
                palette={props.palette}
                fontFamily={props.fontFamily}
                fontSize={props.fontSize}
                onFocus={() => props.onFocusSession(s.id)}
              />
            )}
          </For>
          <For each={props.surfaces}>
            {(s) => (
              <SurfaceThumbnail
                surface={s}
                connectionId={props.connectionId}
                theme={props.theme}
                scale={props.scale}
                focused={s.surfaceId === props.focusedSurfaceId}
                onFocus={() => props.onFocusSurface(s.surfaceId)}
              />
            )}
          </For>
        </div>
      </div>
    </div>
  );
}

function SessionThumbnail(props: {
  session: BlitSession;
  theme: Theme;
  scale: UIScale;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  onFocus: () => void;
}) {
  const label = () =>
    props.session.title ||
    props.session.tag ||
    props.session.command ||
    "Session";

  return (
    <div
      style={{
        "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        display: "flex",
        "flex-direction": "column",
        "flex-shrink": 0,
        overflow: "hidden",
      }}
    >
      <button
        onClick={props.onFocus}
        style={{
          ...ui.btn,
          display: "flex",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.controlY}px ${props.scale.tightGap}px`,
          "font-size": `${props.scale.sm}px`,
          width: "100%",
          "text-align": "left",
          opacity: 1,
          "flex-shrink": 0,
        }}
      >
        <span
          style={{
            flex: 1,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
          }}
        >
          {label()}
        </span>
        <Show when={props.session.state === "exited"}>
          <mark
            style={{
              ...ui.badge,
              "background-color": "rgba(255,100,100,0.3)",
              "font-size": `${props.scale.xs}px`,
            }}
          >
            exited
          </mark>
        </Show>
      </button>
      <div
        style={{
          height: "120px",
          overflow: "hidden",
          cursor: "pointer",
        }}
        onClick={props.onFocus}
      >
        <BlitTerminal
          sessionId={props.session.id}
          readOnly
          showCursor={false}
          style={{ width: "100%", height: "100%" }}
          fontFamily={props.fontFamily}
          fontSize={props.fontSize}
          palette={props.palette}
        />
      </div>
    </div>
  );
}

function SurfaceThumbnail(props: {
  surface: BlitSurface;
  connectionId: string;
  theme: Theme;
  scale: UIScale;
  focused: boolean;
  onFocus: () => void;
}) {
  return (
    <div
      style={{
        "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        display: "flex",
        "flex-direction": "column",
        "flex-shrink": 0,
        overflow: "hidden",
      }}
    >
      <button
        onClick={props.onFocus}
        style={{
          ...ui.btn,
          display: "flex",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.controlY}px ${props.scale.tightGap}px`,
          "font-size": `${props.scale.sm}px`,
          width: "100%",
          "text-align": "left",
          opacity: 1,
          "flex-shrink": 0,
          "background-color": props.focused
            ? props.theme.selectedBg
            : "transparent",
        }}
      >
        <span
          style={{
            flex: 1,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
          }}
        >
          {props.surface.title ||
            props.surface.appId ||
            `Surface ${props.surface.surfaceId}`}
        </span>
        <span
          style={{
            "font-size": `${props.scale.xs}px`,
            color: props.theme.dimFg,
          }}
        >
          {props.surface.width}x{props.surface.height}
        </span>
      </button>
      <div
        style={{
          height: "120px",
          overflow: "hidden",
          cursor: "pointer",
        }}
        onClick={props.onFocus}
      >
        <BlitSurfaceView
          connectionId={props.connectionId}
          surfaceId={props.surface.surfaceId}
          style={{
            display: "block",
            width: "100%",
            height: "100%",
            "object-fit": "contain",
          }}
        />
      </div>
    </div>
  );
}
