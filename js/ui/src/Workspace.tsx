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
  useRemotes,
  useDefaultRemote,
  configWsStatus,
  addRemote,
  removeRemote,
  setDefaultRemote,
  reorderRemotes,
} from "./storage";
import type { UIScale, Theme } from "./theme";
import { themeFor, layout, ui, uiScale, z } from "./theme";
import { t } from "./i18n";
import { StatusBar } from "./StatusBar";
import { SwitcherOverlay } from "./SwitcherOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { RemotesOverlay } from "./RemotesOverlay";
import { BSPContainer } from "./bsp/BSPContainer";
import { ConnectingOverlay } from "./ConnectingOverlay";
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

export type Overlay = "expose" | "palette" | "font" | "help" | "remotes" | null;

export function Workspace(props: {
  connections: ConnectionSpec[] | (() => ConnectionSpec[]);
  wasm: BlitWasmModule;
  onAuthError: () => void;
}) {
  const workspace = new BlitWorkspace({ wasm: props.wasm });

  // Normalise: accept either a static array or a reactive accessor.
  const getConnections =
    typeof props.connections === "function"
      ? props.connections
      : () => props.connections as ConnectionSpec[];

  // Reactively reconcile workspace connections whenever the list changes.
  createEffect(() => {
    const next = getConnections();
    const nextIds = new Set(next.map((c) => c.id));

    // Remove connections no longer in the list.
    const existing = workspace.getSnapshot().connections;
    for (const conn of existing) {
      if (!nextIds.has(conn.id)) {
        workspace.removeConnection(conn.id);
      }
    }

    // Add new connections (snapshot may have changed after removals).
    const existingIds = new Set(
      workspace.getSnapshot().connections.map((c) => c.id),
    );
    for (const conn of next) {
      if (!existingIds.has(conn.id)) {
        workspace.addConnection({ id: conn.id, transport: conn.transport });
      }
    }
  });

  onCleanup(() => {
    for (const conn of workspace.getSnapshot().connections) {
      workspace.removeConnection(conn.id);
    }
  });

  const connectionSpecs = createMemo(() => getConnections());

  return (
    <BlitWorkspaceProvider workspace={workspace}>
      <WorkspaceScreen
        connectionSpecs={connectionSpecs}
        onAuthError={props.onAuthError}
      />
    </BlitWorkspaceProvider>
  );
}

function WorkspaceScreen(props: {
  connectionSpecs: () => ConnectionSpec[];
  onAuthError: () => void;
}) {
  const workspace = createBlitWorkspace();
  const wsState = createBlitWorkspaceState(workspace);
  const sessions = createBlitSessions(workspace);

  /** Connection ID labels from the CLI config — reactive. */
  const connectionLabels = createMemo(
    () =>
      new Map<string, string>(
        props.connectionSpecs().map((c) => [c.id, c.label]),
      ),
  );
  const multiConnection = createMemo(() => props.connectionSpecs().length > 1);
  const defaultConnectionId = createMemo(
    () => props.connectionSpecs()[0]?.id ?? "main",
  );

  const focusedSession = () => {
    const snap = wsState();
    if (!snap.focusedSessionId) return null;
    return snap.sessions.find((s) => s.id === snap.focusedSessionId) ?? null;
  };

  /** The connection that owns the currently focused session (or the first). */
  const activeConnectionId = (): ConnectionId => {
    const fs = focusedSession();
    return fs?.connectionId ?? defaultConnectionId();
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
      for (const spec of props.connectionSpecs()) {
        const conn = workspace.getConnection(spec.id);
        if (!conn) continue;
        for (const s of conn.surfaceStore.getSurfaces().values()) {
          // Filter out zero-size auxiliary surfaces (e.g. mpv's
          // secondary window that never commits a buffer).
          if (s.width > 0 && s.height > 0) all.push(s);
        }
      }
      setSurfaces(all);
    };
    for (const spec of props.connectionSpecs()) {
      const conn = workspace.getConnection(spec.id);
      if (!conn) continue;
      cleanups.push(conn.surfaceStore.onChange(syncAll));
    }
    // Also refresh surfaces on workspace state changes (connection
    // status transitions) so the list stays in sync after reconnects
    // even if the surfaceStore change event is suppressed during the
    // reconnect window.
    cleanups.push(workspace.subscribe(syncAll));
    syncAll();
    onCleanup(() => cleanups.forEach((fn) => fn()));
  });

  const remotes = useRemotes();
  const defaultRemote = useDefaultRemote();

  /** Map remote name → connection status (derived from workspace snapshot). */
  const remoteStatuses = createMemo(() => {
    const map = new Map<string, import("@blit-sh/core").ConnectionStatus>();
    for (const conn of allConnections()) {
      map.set(conn.id, conn.status);
    }
    return map;
  });

  const [palette, setPalette] =
    createSignal<TerminalPalette>(preferredPalette());
  const [font, setFont] = createSignal(preferredFont());
  const [fontSize, setFontSize] = createSignal(preferredFontSize());
  const [overlay, setOverlay] = createSignal<Overlay>(null);
  const [openInNewTerminalMode, setOpenInNewTerminalMode] = createSignal(false);
  const [newTerminalTargetPaneId, setNewTerminalTargetPaneId] = createSignal<
    string | null
  >(null);
  const [debugPanel, setDebugPanel] = createSignal(false);
  const [previewPanelOpen, setPreviewPanelOpen] = createSignal(true);
  const [previewPanelWidth, setPreviewPanelWidth] =
    createSignal(MIN_PANEL_WIDTH);
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
  // Track the connectionId for the focused surface so we don't re-derive
  // it reactively (which causes thrashing when surface list changes).
  const [focusedSurfaceConnId, setFocusedSurfaceConnId] =
    createSignal<ConnectionId | null>(null);

  /** Set or clear the focused surface, always keeping the connectionId
   *  in sync so the BSP view uses the correct connection. */
  function focusSurfaceById(surfaceId: number | null) {
    setFocusedSurfaceId(surfaceId);
    if (surfaceId != null) {
      const s = surfaces().find((x) => x.surfaceId === surfaceId);
      setFocusedSurfaceConnId(s?.connectionId ?? null);
    } else {
      setFocusedSurfaceConnId(null);
    }
  }

  // Restore surface focus from hash once the surface actually exists (one-shot).
  if (pendingSurfaceFromHash != null) {
    let surfaceRestored = false;
    createEffect(() => {
      if (surfaceRestored) return;
      const ss = surfaces();
      if (ss.some((s) => s.surfaceId === pendingSurfaceFromHash)) {
        surfaceRestored = true;
        focusSurfaceById(pendingSurfaceFromHash);
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

  // Clear focused surface if it was destroyed.  Use a short grace period
  // to avoid flickering during reconnect cycles where the surface list is
  // temporarily empty before being re-populated.
  let clearFocusedTimer: ReturnType<typeof setTimeout> | null = null;
  createEffect(() => {
    const fid = focusedSurfaceId();
    if (fid == null) {
      if (clearFocusedTimer) {
        clearTimeout(clearFocusedTimer);
        clearFocusedTimer = null;
      }
      return;
    }
    const exists = surfaces().some((s) => s.surfaceId === fid);
    if (!exists) {
      if (!clearFocusedTimer) {
        clearFocusedTimer = setTimeout(() => {
          clearFocusedTimer = null;
          // Re-check after the grace period.
          const stillGone = !surfaces().some((s) => s.surfaceId === fid);
          if (stillGone) focusSurfaceById(null);
        }, 2000);
      }
    } else if (clearFocusedTimer) {
      clearTimeout(clearFocusedTimer);
      clearFocusedTimer = null;
    }
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

  // Connecting overlay dismiss state: user can hide it manually.
  // Only shown on initial page load — once dismissed (or connected), stays hidden.
  const [connectingOverlayDismissed, setConnectingOverlayDismissed] =
    createSignal(false);
  createEffect(() => {
    // Auto-dismiss once we reach connected for the first time.
    if (stableStatus() === "connected") setConnectingOverlayDismissed(true);
  });
  const showConnectingOverlay = () =>
    stableStatus() !== "connected" && !connectingOverlayDismissed();

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
    if (fs) {
      const label = connectionLabels().get(fs.connectionId);
      if (label) parts.push(label);
      if (fs.title) parts.push(fs.title);
    }
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
    setOpenInNewTerminalMode(false);
    setNewTerminalTargetPaneId(null);
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

  function openNewTerminalPicker(paneId?: string) {
    if (!previousFocus) previousFocus = document.activeElement;
    setNewTerminalTargetPaneId(paneId ?? null);
    setOpenInNewTerminalMode(true);
    setOverlay("expose");
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
  const activePaneId = createMemo(() =>
    activeLayout() ? bspFocusedPaneId() : null,
  );

  function switchSession(sessionId: SessionId) {
    focusSurfaceById(null);
    workspace.focusSession(sessionId);
    focusBySessionFn?.(sessionId);
    previousFocus = null;
    closeOverlay();
  }

  function focusSurface(surfaceId: number) {
    // When a BSP layout is active, place the surface into the focused pane.
    if (activeLayout() && bspFocusedPaneId()) {
      moveToPaneFn?.(surfaceAssignment(surfaceId), bspFocusedPaneId()!);
      focusSurfaceById(null);
    } else {
      focusSurfaceById(surfaceId);
    }
    closeOverlay();
  }

  let termHandle: { rows: number; cols: number; focus: () => void } | null =
    null;

  async function createAndFocus(command?: string, connectionId?: string) {
    try {
      const fid = wsState().focusedSessionId;
      const connId = connectionId ?? activeConnectionId();
      const session = await workspace.createSession({
        connectionId: connId,
        rows: termHandle?.rows ?? 24,
        cols: termHandle?.cols ?? 80,
        ...(command ? { command } : {}),
        ...(!command && fid && !connectionId ? { cwdFromSessionId: fid } : {}),
      });
      focusSurfaceById(null);
      workspace.focusSession(session.id);
      previousFocus = null;
      closeOverlay();
    } catch {}
  }

  async function createInPane(
    paneId: string,
    command?: string,
    connectionId?: string,
  ) {
    try {
      const fid = wsState().focusedSessionId;
      const connId = connectionId ?? activeConnectionId();
      const session = await workspace.createSession({
        connectionId: connId,
        rows: termHandle?.rows ?? 24,
        cols: termHandle?.cols ?? 80,
        ...(command ? { command } : {}),
        ...(!command && fid && !connectionId ? { cwdFromSessionId: fid } : {}),
      });
      moveSessionToPaneFn?.(session.id, paneId);
      workspace.focusSession(session.id);
    } catch {}
  }

  function selectPane(
    paneId: string,
    sessionId: SessionId | null,
    command?: string,
    connectionId?: string,
  ) {
    if (sessionId && !command) {
      workspace.focusSession(sessionId);
      focusBySessionFn?.(sessionId);
    } else {
      void createInPane(paneId, command, connectionId);
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
    layoutAssignments,
    focusedSession,
    sessions,
    focusedSessionId: () => wsState().focusedSessionId,
    supportsRestart: () => connection()?.supportsRestart ?? false,
    focusedSurfaceId,
    closeSurface: (surfaceId: number) => {
      workspace.closeSurface(activeConnectionId(), surfaceId);
    },
    unfocusSurface: () => {
      focusSurfaceById(null);
    },
    toggleOverlay,
    cancelOverlay,
    toggleDebug,
    togglePreviewPanel,
    createAndFocus,
    createInPane,
    openNewTerminalPicker,
    handleRestartOrClose,
    connectionCount: () => allConnections().length,
    focusBySession: (sessionId) => {
      workspace.focusSession(sessionId);
      focusBySessionFn?.(sessionId);
    },
  });

  // Set font defaults on connection
  createEffect(() => {
    const conn = workspace.getConnection(activeConnectionId());
    if (!conn) return;
    const dpr = window.devicePixelRatio || 1;
    conn.setFontSize(fontSize() * dpr);
    conn.setFontFamily(resolvedFontWithFallback());
  });

  // Sync layout + focus to URL hash.
  // Assignments are also durably saved to localStorage so they survive
  // even when the hash is lost (new tab, bookmark, etc.).
  createEffect(() => {
    if (connection()?.status !== "connected") return;
    const parts: string[] = [];
    const al = activeLayout();
    const paneId = bspFocusedPaneId();
    const la = layoutAssignments();
    if (al)
      parts.push(`l=${al.name !== al.dsl ? `${al.name}:${al.dsl}` : al.dsl}`);
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
    // Strip layout-managed keys (l, p, a) from the old hash only when we
    // have fresh values to replace them.  If layoutAssignments is still null
    // (BSPContainer hasn't resolved its pending hash assignments yet), keep
    // the existing `a=` (and `p=`) so they aren't wiped before resolution
    // completes.  Once la is non-null this effect will re-run and write the
    // resolved values.
    const written = new Set(parts.map((p) => p.slice(0, p.indexOf("="))));
    written.add("l");
    if (paneId) written.add("p");
    if (la) written.add("a");
    const kept = existing
      .split("&")
      .filter(
        (s) =>
          s &&
          !(/^[lpast]=/.test(s) && written.has(s.slice(0, s.indexOf("=")))),
      );
    const merged = [...kept, ...parts];
    const newHash = merged.join("&");
    if (newHash !== existing) {
      history.replaceState(
        null,
        "",
        newHash ? `#${newHash}` : location.pathname + location.search,
      );
    }
  });

  const { countFrame, timeline, net, metrics } = createMetrics(
    props.connectionSpecs().map((s) => s.transport),
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
            <Show when={showConnectingOverlay()}>
              <ConnectingOverlay
                gatewayStatus={configWsStatus()}
                connections={allConnections()}
                connectionLabels={connectionLabels()}
                palette={palette()}
                fontSize={fontSize()}
                onDismiss={() => setConnectingOverlayDismissed(true)}
              />
            </Show>
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
                          onNewTerminal={() => {
                            if (allConnections().length <= 1) {
                              void createAndFocus();
                            } else {
                              openNewTerminalPicker();
                            }
                          }}
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
                      connectionId={
                        focusedSurfaceConnId() ?? activeConnectionId()
                      }
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
                  connectionLabels={connectionLabels()}
                  palette={palette()}
                  fontFamily={resolvedFontWithFallback()}
                  fontSize={fontSize()}
                  focusedSessionId={wsState().focusedSessionId}
                  lruSessionIds={lru}
                  liveSurfaceIds={surfaces().map((s) => s.surfaceId)}
                  manageVisibility={overlay() !== "expose"}
                  extraVisibleSessions={offScreenSessions().map((s) => s.id)}
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
                  onCreateInPane={(paneId, command, connectionId) => {
                    if (
                      !command &&
                      !connectionId &&
                      allConnections().length > 1
                    ) {
                      openNewTerminalPicker(paneId);
                    } else {
                      void createInPane(paneId, command, connectionId);
                    }
                  }}
                  onSwitcher={() => toggleOverlay("expose")}
                  onHelp={() => toggleOverlay("help")}
                  onRender={countFrame}
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
          {(_) => (
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
              onCreate={(command, connectionId) => {
                const paneId = newTerminalTargetPaneId();
                if (paneId) {
                  void createInPane(paneId, command, connectionId);
                } else {
                  void createAndFocus(command, connectionId);
                }
              }}
              initialNewTerminalMode={openInNewTerminalMode()}
              activeLayout={activeLayout()}
              layoutAssignments={layoutAssignments()}
              onSelectPane={selectPane}
              focusedPaneId={activePaneId()}
              onMoveToPane={(sessionId, targetPaneId) => {
                moveSessionToPaneFn?.(sessionId, targetPaneId);
                workspace.focusSession(sessionId);
                closeOverlay();
              }}
              onApplyLayout={(l) => {
                // Clear stale assignments immediately so the hash sync
                // effect (which runs before BSPContainer re-computes)
                // doesn't write old pane IDs into the URL.
                setLayoutAssignments(null);
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
              onChangeRemotes={() => toggleOverlay("remotes")}
              remotes={remotes()}
              remoteStatuses={remoteStatuses()}
              surfaces={surfaces()}
              connectionId={activeConnectionId()}
              connectionLabels={connectionLabels()}
              multiConnection={multiConnection()}
              focusedSurfaceId={focusedSurfaceId()}
              onFocusSurface={focusSurface}
              onMoveSurfaceToPane={(sid, targetPaneId) => {
                moveToPaneFn?.(surfaceAssignment(sid), targetPaneId);
                focusSurfaceById(null);
                closeOverlay();
              }}
            />
          )}
        </Show>
        <Show when={overlay() === "palette"}>
          {(_) => (
            <PaletteOverlay
              current={palette()}
              fontSize={fontSize()}
              onSelect={changePalette}
              onPreview={setPalette}
              onClose={closeOverlay}
            />
          )}
        </Show>
        <Show when={overlay() === "font"}>
          {(_) => (
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
          )}
        </Show>
        <Show when={overlay() === "help"}>
          {(_) => (
            <HelpOverlay
              onClose={closeOverlay}
              palette={palette()}
              fontSize={fontSize()}
            />
          )}
        </Show>
        <Show when={overlay() === "remotes"}>
          {(_) => (
            <RemotesOverlay
              remotes={remotes()}
              defaultRemote={defaultRemote()}
              statuses={remoteStatuses()}
              palette={palette()}
              fontSize={fontSize()}
              onAdd={(name, uri) => addRemote(name, uri)}
              onRemove={(name) => removeRemote(name)}
              onSetDefault={(name) => setDefaultRemote(name)}
              onReorder={(names) => reorderRemotes(names)}
              onClose={closeOverlay}
            />
          )}
        </Show>
        <footer
          style={{
            ...layout.statusBar,
            padding: "0 1em",
            "background-color": theme().bg,
            color: theme().fg,
            "border-top-color": theme().subtleBorder,
            height: `${chromeScale().md + chromeScale().controlY * 2}px`,
            "font-size": `${chromeScale().sm}px`,
          }}
        >
          <StatusBar
            sessions={sessions()}
            surfaceCount={surfaces().length}
            focusedSession={focusedSession()}
            connectionLabels={connectionLabels()}
            connections={allConnections()}
            gatewayStatus={configWsStatus()}
            status={stableStatus()}
            onReconnect={() => {
              for (const spec of props.connectionSpecs()) {
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
  onNewTerminal: () => void;
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
          onClick={props.onNewTerminal}
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
    </div>
  );
}

const SURFACE_PANEL_WIDTH = 280;
const MIN_PANEL_WIDTH = 160;

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
      props.onResize(Math.max(MIN_PANEL_WIDTH, startWidth + delta));
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
          "background-color": props.theme.bg,
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
                connectionId={s.connectionId}
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
          overflow: "hidden",
          cursor: "pointer",
        }}
        onClick={props.onFocus}
      >
        <BlitTerminal
          sessionId={props.session.id}
          readOnly
          showCursor={false}
          style={{ width: "100%", height: "auto" }}
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
          overflow: "hidden",
          cursor: "pointer",
        }}
        onClick={props.onFocus}
      >
        <BlitSurfaceView
          connectionId={props.surface.connectionId}
          surfaceId={props.surface.surfaceId}
          style={{
            display: "block",
            width: "100%",
            height: "auto",
            "object-fit": "contain",
          }}
        />
      </div>
    </div>
  );
}
