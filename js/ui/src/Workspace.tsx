import {
  createSignal,
  createEffect,
  createMemo,
  onMount,
  onCleanup,
  untrack,
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
  BlitTerminalSurface,
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
  AUDIO_BITRATE_KEY,
  AUDIO_MUTED_KEY,
  VIDEO_QUALITY_KEY,
  SURFACE_STREAMING_KEY,
  writeStorage,
  useConfigValue,
  preferredPalette,
  preferredFont,
  preferredFontSize,
  preferredAudioBitrate,
  preferredAudioMuted,
  preferredVideoQuality,
  preferredSurfaceStreaming,
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
import {
  sessionName,
  sessionPrefix,
  themeFor,
  layout,
  ui,
  uiScale,
  z,
} from "./theme";
import { t } from "./i18n";
import { StatusBar } from "./StatusBar";
import { SwitcherOverlay } from "./SwitcherOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { RemotesOverlay } from "./RemotesOverlay";
import { MediaOverlay } from "./MediaOverlay";
import { BSPContainer, EmptyPane } from "./bsp/BSPContainer";

import { MobileToolbar } from "./MobileToolbar";
import type { BSPAssignments, BSPLayout } from "./bsp/layout";
import {
  loadActiveLayout,
  saveActiveLayout,
  saveToHistory,
  removeFromHistory,
  loadRecentLayouts,
  PRESETS,
  surfaceAssignment,
  isSurfaceAssignment,
  parseSurfaceAssignment,
} from "./bsp/layout";

export type Overlay =
  | "expose"
  | "palette"
  | "font"
  | "help"
  | "remotes"
  | "media"
  | null;

function getHmrWorkspace(wasm: BlitWasmModule): BlitWorkspace {
  const prev = import.meta.hot?.data?.workspace as BlitWorkspace | undefined;
  if (prev) return prev;
  const ws = new BlitWorkspace({ wasm });
  if (import.meta.hot) import.meta.hot.data.workspace = ws;
  return ws;
}

export function Workspace(props: {
  connections: ConnectionSpec[] | (() => ConnectionSpec[]);
  wasm: BlitWasmModule;
  onAuthError: () => void;
}) {
  const workspace = getHmrWorkspace(props.wasm);

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
    // On real teardown, remove all connections. During HMR, keep them alive —
    // the reconciliation effect will re-adopt them on the next mount.
    if (!import.meta.hot) {
      for (const conn of workspace.getSnapshot().connections) {
        workspace.removeConnection(conn.id);
      }
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

  // Per-surface signature of the fields that drive the thumbnail UI
  // (title, appId, width, height).  SurfaceStore mutates width/height
  // in place on each frame so ref-level diffing never sees dim changes,
  // and <For each> keys by reference so a child component reading
  // `props.surface.width` won't re-render when the underlying field is
  // mutated.  We fix both by tracking a per-surface sig: when a
  // surface's sig changes we emit a shallow copy (new ref → <For>
  // remounts that one child and reads the new dims), while surfaces
  // whose sig is unchanged keep their ref so their children aren't
  // disturbed.
  const surfaceSigs = new Map<string, string>();

  // Track the set of available connection IDs so the surface aggregation
  // effect re-runs when connections are added or removed.  The joined-string
  // comparison ensures the memo value only changes when the actual set of
  // IDs changes, not on every workspace snapshot update (which is frequent
  // due to terminal output, pings, etc.).
  const availableConnIds = createMemo(() =>
    wsState()
      .connections.map((c) => c.id)
      .sort()
      .join(","),
  );

  // Aggregate surfaces from all connections.
  // When surface streaming is disabled the list is emptied, which cascades
  // through every derived view (focused surface, BSP panes, preview panel,
  // status bar count, switcher) so windows disappear immediately.
  createEffect(() => {
    // Re-run when connection specs change OR when the set of live
    // connections changes (a connection that was absent when we first ran
    // may now be available, and we need its surfaceStore.onChange listener).
    const _connIds = availableConnIds();
    const streaming = surfaceStreaming();
    const cleanups: (() => void)[] = [];
    const syncAll = () => {
      if (!streaming) {
        if (untrack(() => surfaces()).length !== 0) {
          surfaceSigs.clear();
          setSurfaces([]);
        }
        return;
      }
      const all: BlitSurface[] = [];
      const seenKeys = new Set<string>();
      let anyChanged = false;
      for (const spec of props.connectionSpecs()) {
        const conn = workspace.getConnection(spec.id);
        if (!conn) continue;
        for (const s of conn.surfaceStore.getSurfaces().values()) {
          const key = `${s.connectionId}:${s.surfaceId}`;
          seenKeys.add(key);
          const sig = `${s.title}\0${s.appId}\0${s.width}x${s.height}`;
          if (surfaceSigs.get(key) !== sig) {
            surfaceSigs.set(key, sig);
            // Shallow copy: a new ref forces <For> to rebuild this
            // item's child, which is the only way a downstream
            // `props.surface.width` JSX read picks up the fresh value
            // (SolidJS doesn't track property access on plain objects).
            all.push({ ...s });
            anyChanged = true;
          } else {
            all.push(s);
          }
        }
      }
      // Prune sigs for surfaces that no longer exist so stale entries
      // don't forever block a new surface with the same id from
      // getting a fresh ref on first frame.
      if (surfaceSigs.size !== seenKeys.size) {
        for (const key of surfaceSigs.keys()) {
          if (!seenKeys.has(key)) {
            surfaceSigs.delete(key);
            anyChanged = true;
          }
        }
      }
      const prev = untrack(() => surfaces());
      if (!anyChanged && prev.length === all.length) return;
      setSurfaces(all);
    };
    for (const spec of props.connectionSpecs()) {
      const conn = workspace.getConnection(spec.id);
      if (!conn) continue;
      cleanups.push(conn.surfaceStore.onChange(syncAll));
    }
    // Also refresh on workspace state changes (connection status
    // transitions) so the surface list stays in sync after reconnects
    // and initial connection setup.  The equality check in syncAll
    // prevents <For> churn on unrelated snapshot changes (terminal
    // frames, pacing, ping).
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
  const [audioMuted, setAudioMuted] = createSignal(preferredAudioMuted());
  const [audioBitrate, setAudioBitrate] = createSignal(preferredAudioBitrate());
  const [videoQuality, setVideoQuality] = createSignal(preferredVideoQuality());
  const [surfaceStreaming, setSurfaceStreaming] = createSignal(
    preferredSurfaceStreaming(),
  );
  const [previewPanelOpen, setPreviewPanelOpen] = createSignal(true);
  const [previewPanelWidth, setPreviewPanelWidth] =
    createSignal(MIN_PANEL_WIDTH);

  // --- Mobile touch detection & virtual keyboard tracking ---
  const [isMobileTouch, setIsMobileTouch] = createSignal(false);
  const [terminalSurface, setTerminalSurface] =
    createSignal<BlitTerminalSurface | null>(null);

  onMount(() => {
    const isTouch = () =>
      "ontouchstart" in window ||
      navigator.maxTouchPoints > 0 ||
      matchMedia("(pointer: coarse)").matches;
    const check = () => isTouch();
    setIsMobileTouch(check());
    // Recheck when the coarse pointer media query changes (e.g.
    // DevTools device-mode toggle).
    const mq = matchMedia("(pointer: coarse)");
    const handler = () => setIsMobileTouch(check());
    mq.addEventListener?.("change", handler);
    onCleanup(() => {
      mq.removeEventListener?.("change", handler);
    });
  });

  // Track visualViewport to detect keyboard open/close on mobile.
  const [vpHeight, setVpHeight] = createSignal<number | null>(null);
  const [vpOffset, setVpOffset] = createSignal(0);
  onMount(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    const update = () => {
      setVpHeight(vv.height);
      setVpOffset(vv.offsetTop);
    };
    update(); // initialise immediately
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    onCleanup(() => {
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
    });
  });

  // Capture full viewport height at mount and on orientation change.
  const [fullHeight, setFullHeight] = createSignal(0);
  onMount(() => {
    setFullHeight(window.innerHeight);
    const onOrientationChange = () => {
      setTimeout(() => setFullHeight(window.innerHeight), 150);
    };
    screen.orientation?.addEventListener("change", onOrientationChange);
    onCleanup(() =>
      screen.orientation?.removeEventListener("change", onOrientationChange),
    );
  });

  // Keyboard open when visualViewport shrinks >150px from full height.
  const keyboardOpen = createMemo(() => {
    if (!isMobileTouch()) return false;
    const h = vpHeight();
    const full = fullHeight();
    if (h === null || full === 0) return false;
    return full - h > 150;
  });

  // Sticky virtual keyboard: track explicit user intent so the keyboard
  // isn't dismissed when tapping elsewhere on the page.
  const [keyboardWanted, setKeyboardWanted] = createSignal(false);

  // Re-focus the terminal textarea when it blurs while the user wants
  // the keyboard open, unless an overlay is active.
  createEffect(() => {
    if (!isMobileTouch() || !keyboardWanted()) return;
    const handler = (e: FocusEvent) => {
      if (!(e.target instanceof HTMLTextAreaElement)) return;
      if (!(e.target as Element).closest?.("section")) return;
      if (overlay()) return;
      setTimeout(() => {
        if (!keyboardWanted() || overlay()) return;
        const el = document.querySelector<HTMLElement>(
          "section textarea[tabindex]",
        );
        el?.focus();
      }, 50);
    };
    document.addEventListener("focusout", handler, true);
    onCleanup(() => document.removeEventListener("focusout", handler, true));
  });

  /** Toggle the virtual keyboard on mobile. */
  function toggleMobileKeyboard() {
    const el = document.querySelector<HTMLElement>(
      "section textarea[tabindex]",
    );
    if (!el) return;
    if (keyboardWanted()) {
      setKeyboardWanted(false);
      el.blur();
    } else {
      setKeyboardWanted(true);
      el.focus();
    }
  }

  // Parse focus params from URL hash on init.
  // Surface: s=<connectionId>:<surfaceId>
  // Terminal: t=<sessionId>  (sessionId is already "<connectionId>:<counter>")
  const initHash = new URLSearchParams(
    location.hash.slice(1).replace(/&/g, "&"),
  );
  const hashSurface = initHash.get("s");
  const hashTerminal = initHash.get("t");

  // s= and t= are mutually exclusive; s= takes priority.
  const pendingSurfaceFromHash: {
    connectionId: string;
    surfaceId: number;
  } | null = (() => {
    if (!hashSurface) return null;
    const sep = hashSurface.indexOf(":");
    if (sep < 0) return null;
    const connectionId = hashSurface.slice(0, sep);
    const surfaceId = Number(hashSurface.slice(sep + 1));
    if (!connectionId || !Number.isFinite(surfaceId)) return null;
    return { connectionId, surfaceId };
  })();

  const [focusedSurfaceId, setFocusedSurfaceId] = createSignal<number | null>(
    null,
  );
  // Track the connectionId for the focused surface so we don't re-derive
  // it reactively (which causes thrashing when surface list changes).
  const [focusedSurfaceConnId, setFocusedSurfaceConnId] =
    createSignal<ConnectionId | null>(null);

  /** Set or clear the focused surface, always keeping the connectionId
   *  in sync so the BSP view uses the correct connection.
   *  When `connectionId` is provided it is used directly, avoiding a
   *  potentially ambiguous lookup by numeric surfaceId alone. */
  function focusSurfaceById(
    surfaceId: number | null,
    connectionId?: ConnectionId | null,
  ) {
    setFocusedSurfaceId(surfaceId);
    if (surfaceId != null) {
      const connId =
        connectionId ??
        surfaces().find((x) => x.surfaceId === surfaceId)?.connectionId ??
        null;
      setFocusedSurfaceConnId(connId);
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
      if (
        ss.some(
          (s) =>
            s.surfaceId === pendingSurfaceFromHash.surfaceId &&
            s.connectionId === pendingSurfaceFromHash.connectionId,
        )
      ) {
        surfaceRestored = true;
        focusSurfaceById(
          pendingSurfaceFromHash.surfaceId,
          pendingSurfaceFromHash.connectionId as ConnectionId,
        );
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
  const [recentLayouts, setRecentLayouts] = createSignal(loadRecentLayouts());
  const [layoutAssignments, setLayoutAssignments] =
    createSignal<BSPAssignments | null>(null);
  /** True once BSPContainer has finished resolving hash-based assignments. */
  const [assignmentsResolved, setAssignmentsResolved] = createSignal(true);

  // Re-parse layout from URL hash when the user edits it externally.
  // The app writes the hash via history.replaceState() which does NOT
  // trigger hashchange, so this only fires on genuine external edits.
  createEffect(() => {
    const onHashChange = () => {
      const fromHash = loadActiveLayout();
      if (fromHash && fromHash.dsl !== activeLayout()?.dsl) {
        setActiveLayout(fromHash);
      }
    };
    window.addEventListener("hashchange", onHashChange);
    onCleanup(() => window.removeEventListener("hashchange", onHashChange));
  });

  // Clear focused surface if it was destroyed.  Use a short grace period
  // to avoid flickering during reconnect cycles where the surface list is
  // temporarily empty before being re-populated.
  let clearFocusedTimer: ReturnType<typeof setTimeout> | null = null;
  createEffect(() => {
    const fid = focusedSurfaceId();
    const fConnId = focusedSurfaceConnId();
    if (fid == null) {
      if (clearFocusedTimer) {
        clearTimeout(clearFocusedTimer);
        clearFocusedTimer = null;
      }
      return;
    }
    const exists = surfaces().some(
      (s) =>
        s.surfaceId === fid && (fConnId == null || s.connectionId === fConnId),
    );
    if (!exists) {
      if (!clearFocusedTimer) {
        clearFocusedTimer = setTimeout(() => {
          clearFocusedTimer = null;
          // Re-check after the grace period.
          const stillGone = !surfaces().some(
            (s) =>
              s.surfaceId === fid &&
              (fConnId == null || s.connectionId === fConnId),
          );
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
    const fConnId = focusedSurfaceConnId();
    // Collect surface keys assigned to BSP panes.
    const al = activeLayout();
    const la = layoutAssignments();
    if (al) {
      // While layoutAssignments hasn't been reported yet (null during
      // initialization or layout switch), treat all surfaces as assigned
      // to avoid showing them in both BSP panes and the side panel.
      if (!la) return [];
    }
    const inPane = new Set<string>();
    if (la) {
      for (const v of Object.values(la.assignments)) {
        if (v && isSurfaceAssignment(v)) {
          const parsed = parseSurfaceAssignment(v);
          if (parsed) inPane.add(`${parsed.connectionId}:${parsed.surfaceId}`);
        }
      }
    }
    return surfaces().filter(
      (s) =>
        !(
          s.surfaceId === fid &&
          (fConnId == null || s.connectionId === fConnId)
        ) && !inPane.has(`${s.connectionId}:${s.surfaceId}`),
    );
  });

  const offScreenSessions = createMemo(() => {
    const al = activeLayout();
    const la = layoutAssignments();
    const sess = sessions();
    if (al) {
      // While layoutAssignments hasn't been reported yet (null during
      // initialization or layout switch), treat all sessions as assigned
      // to avoid flashing every terminal in the side panel.
      if (!la) return [];
      const assigned = new Set<SessionId>(
        Object.values(la.assignments).filter(
          (id): id is SessionId => id != null && !isSurfaceAssignment(id),
        ),
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
  const remoteAudioBitrate = useConfigValue(AUDIO_BITRATE_KEY);
  const remoteAudioMuted = useConfigValue(AUDIO_MUTED_KEY);
  const remoteVideoQuality = useConfigValue(VIDEO_QUALITY_KEY);
  const remoteSurfaceStreaming = useConfigValue(SURFACE_STREAMING_KEY);

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

  createEffect(() => {
    const s = remoteAudioBitrate();
    if (!s) return;
    const n = parseInt(s, 10);
    if (n >= 0) setAudioBitrate(n);
  });

  createEffect(() => {
    const s = remoteAudioMuted();
    if (s === "0") setAudioMuted(false);
    else if (s === "1") setAudioMuted(true);
  });

  createEffect(() => {
    const s = remoteVideoQuality();
    if (!s) return;
    const n = parseInt(s, 10);
    if (n >= 0 && n <= 4) setVideoQuality(n);
  });

  createEffect(() => {
    const s = remoteSurfaceStreaming();
    if (s === "0") setSurfaceStreaming(false);
    else if (s === "1") setSurfaceStreaming(true);
  });

  // Sync media preferences to all connections so new subscribes use them.
  createEffect(() => {
    const q = videoQuality();
    const b = audioBitrate();
    const streaming = surfaceStreaming();
    for (const snap of allConnections()) {
      const conn = workspace.getConnection(snap.id);
      if (conn) {
        conn.defaultSurfaceQuality = q;
        conn.defaultAudioBitrateKbps = b;
        conn.surfaceStreamingEnabled = streaming;
      }
    }
  });

  // Reactively sync audio subscriptions to all connections.
  // Subscribes when unmuted and surfaces exist, unsubscribes when muted or
  // surfaces disappear. Also applies mute state to the AudioPlayer so newly
  // added connections pick up the current setting.
  //
  // AudioPlayer state changes (e.g. reset on reconnect / S2C_HELLO) are
  // wired into the connection's emit chain (see BlitConnection constructor),
  // so this effect re-runs whenever the subscription is invalidated and can
  // re-subscribe automatically.
  createEffect(() => {
    const muted = audioMuted();
    const bitrate = audioBitrate();
    // Read surfaces() to re-run when surfaces appear/disappear.
    surfaces();
    for (const snap of allConnections()) {
      if (!snap.supportsAudio) continue;
      const conn = workspace.getConnection(snap.id);
      if (!conn) continue;
      conn.audioPlayer.setMuted(muted);
      const surfs = conn.surfaceStore.getSurfaces();
      if (surfs.size === 0) {
        // No surfaces — unsubscribe if subscribed.
        if (conn.audioPlayer.subscribed) {
          conn.sendAudioUnsubscribe();
        }
        continue;
      }
      if (!muted && !conn.audioPlayer.subscribed) {
        conn.sendAudioSubscribe(bitrate);
      } else if (muted && conn.audioPlayer.subscribed) {
        conn.sendAudioUnsubscribe();
      }
    }
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
    setAssignmentsResolved(true);
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

  // Worst status across all connections.
  const connectionStatus = () => {
    const conns = allConnections();
    if (conns.length === 0) return "disconnected" as const;
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

  // Auto-open the remotes overlay while connections are being established
  // on initial page load, and auto-close once everything is connected.
  // Once dismissed (by auto-close or user action), never auto-open again.
  const [remotesAutoOpen, setRemotesAutoOpen] = createSignal<
    "pending" | "open" | "done"
  >("pending");
  createEffect(() => {
    const status = connectionStatus();
    const phase = remotesAutoOpen();
    if (status === "connected") {
      if (phase === "open") {
        // All connected — auto-close if still showing.
        setRemotesAutoOpen("done");
        if (overlay() === "remotes") setOverlay(null);
      } else if (phase === "pending") {
        // Connected before we ever opened — skip entirely.
        setRemotesAutoOpen("done");
      }
      return;
    }
    // Only auto-open when there are configured remotes — a single local
    // connection is near-instant and doesn't need a status dialog.
    if (phase === "pending" && overlay() === null && remotes().length > 0) {
      setRemotesAutoOpen("open");
      setOverlay("remotes");
    }
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
    // In BSP, the workspace's focusedSessionId can be resurrected by
    // resolveFocusedSessionId's per-connection fallback on any connection
    // event (e.g. a terminal title update), even after BSP explicitly
    // cleared it to focus a surface or empty pane.  Gate on BSP's focused
    // pane actually holding a session so a background terminal's title
    // can't leak into the browser title bar.
    const al = activeLayout();
    const bspHasSession =
      al != null &&
      (() => {
        const pid = bspFocusedPaneId();
        if (!pid) return false;
        const assignment = layoutAssignments()?.assignments[pid] ?? null;
        return assignment != null && !isSurfaceAssignment(assignment);
      })();
    const fs = al && !bspHasSession ? null : focusedSession();
    if (fs) {
      if (fs.title) parts.push(fs.title);
      const label = connectionLabels().get(fs.connectionId);
      if (label) parts.push(label);
    } else {
      const surf =
        focusedSurfaceId() != null
          ? (surfaces().find(
              (s) =>
                s.surfaceId === focusedSurfaceId() &&
                (focusedSurfaceConnId() == null ||
                  s.connectionId === focusedSurfaceConnId()),
            ) ?? null)
          : bspFocusedSurface();
      if (surf) {
        const name = surf.title || surf.appId;
        if (name) parts.push(name);
        const label = connectionLabels().get(surf.connectionId);
        if (label) parts.push(label);
      }
    }
    if (host && host !== "localhost" && host !== "127.0.0.1") parts.push(host);
    // Don't append "Blit" — installed PWA windows and most browsers already
    // prefix the tab with the app/manifest name, producing redundant
    // "Blit - … — Blit" titles.  Falling back to an empty document.title
    // when nothing is focused lets the OS/browser show just the app name.
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
    // If the user manually dismisses the auto-opened remotes overlay,
    // mark it done so it never re-opens or auto-closes a later overlay.
    if (overlay() === "remotes" && remotesAutoOpen() === "open") {
      setRemotesAutoOpen("done");
    }
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
    if (target === "remotes" && remotesAutoOpen() === "open") {
      // User explicitly opened remotes — stop auto-close from dismissing it.
      setRemotesAutoOpen("done");
    } else if (target === "palette") {
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

  function changeAudioBitrate(kbps: number) {
    setAudioBitrate(kbps);
    writeStorage(AUDIO_BITRATE_KEY, String(kbps));
    // Re-subscribe all active audio connections with the new bitrate.
    for (const snap of allConnections()) {
      if (!snap.supportsAudio) continue;
      const conn = workspace.getConnection(snap.id);
      if (!conn || !conn.audioPlayer.subscribed) continue;
      conn.sendAudioSubscribe(kbps);
    }
  }

  function toggleAudio() {
    const newMuted = !audioMuted();
    setAudioMuted(newMuted);
    writeStorage(AUDIO_MUTED_KEY, newMuted ? "1" : "0");
    // The reactive effect (syncAudioSubscriptions) will handle
    // subscribing/unsubscribing and applying mute to all connections.
  }

  function resetAudio() {
    for (const snap of allConnections()) {
      if (!snap.supportsAudio) continue;
      const conn = workspace.getConnection(snap.id);
      if (!conn) continue;
      conn.resetAudio();
    }
  }

  function changeVideoQuality(quality: number) {
    setVideoQuality(quality);
    writeStorage(VIDEO_QUALITY_KEY, String(quality));
    // Re-subscribe all active surface subscriptions with the new quality.
    for (const snap of allConnections()) {
      const conn = workspace.getConnection(snap.id);
      if (!conn) continue;
      conn.defaultSurfaceQuality = quality;
      for (const surface of conn.surfaceStore.getSurfaces().values()) {
        conn.sendSurfaceResubscribe(surface.surfaceId, quality);
      }
    }
  }

  function changeSurfaceStreaming(enabled: boolean) {
    setSurfaceStreaming(enabled);
    writeStorage(SURFACE_STREAMING_KEY, enabled ? "1" : "0");
    for (const snap of allConnections()) {
      const conn = workspace.getConnection(snap.id);
      if (!conn) continue;
      conn.setSurfaceStreamingEnabled(enabled);
    }
  }

  let focusBySessionFn: ((sessionId: SessionId) => void) | null = null;
  let moveSessionToPaneFn:
    | ((sessionId: SessionId, targetPaneId: string) => void)
    | null = null;
  let moveToPaneFn: ((value: string, targetPaneId: string) => void) | null =
    null;
  let clearPaneAssignmentFn: ((paneId: string) => void) | null = null;
  let focusPaneFn: ((paneId: string) => void) | null = null;
  const [bspFocusedPaneId, setBspFocusedPaneId] = createSignal<string | null>(
    null,
  );
  const activePaneId = createMemo(() =>
    activeLayout() ? bspFocusedPaneId() : null,
  );

  /** Resolve the surface occupying the BSP-focused pane (if any). */
  const bspFocusedSurface = createMemo(() => {
    const paneId = activePaneId();
    if (!paneId) return null;
    const la = layoutAssignments();
    if (!la) return null;
    const value = la.assignments[paneId] ?? null;
    const parsed = parseSurfaceAssignment(value);
    if (!parsed) return null;
    return (
      surfaces().find(
        (s) =>
          s.surfaceId === parsed.surfaceId &&
          s.connectionId === parsed.connectionId,
      ) ?? null
    );
  });

  function switchSession(sessionId: SessionId) {
    focusSurfaceById(null);
    workspace.focusSession(sessionId);
    focusBySessionFn?.(sessionId);
    previousFocus = null;
    closeOverlay();
  }

  function focusSurface(surfaceId: number, connectionId?: ConnectionId) {
    // When a BSP layout is active, place the surface into the focused pane.
    if (activeLayout() && bspFocusedPaneId()) {
      const connId =
        connectionId ??
        surfaces().find((x) => x.surfaceId === surfaceId)?.connectionId ??
        activeConnectionId();
      moveToPaneFn?.(surfaceAssignment(connId, surfaceId), bspFocusedPaneId()!);
      focusSurfaceById(null);
    } else {
      focusSurfaceById(surfaceId, connectionId);
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
      focusPaneFn?.(paneId);
    } else if (command || connectionId) {
      void createInPane(paneId, command, connectionId);
    } else {
      // Empty pane, no command — just move focus.
      focusPaneFn?.(paneId);
    }
    closeOverlay();
  }

  function handleRestartOrClose() {
    const fs = focusedSession();
    if (!fs) {
      const paneId = bspFocusedPaneId();
      if (paneId) {
        void createInPane(paneId);
      } else {
        void createAndFocus();
      }
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
    focusedSurfaceConnId,
    closeSurface: (connectionId: ConnectionId, surfaceId: number) => {
      workspace.closeSurface(connectionId, surfaceId);
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
    clearFocusedPaneAssignment: () => {
      const paneId = bspFocusedPaneId();
      if (paneId) clearPaneAssignmentFn?.(paneId);
    },
    resetAudio,
  });

  // Set font defaults on connection
  createEffect(() => {
    const conn = workspace.getConnection(activeConnectionId());
    if (!conn) return;
    const dpr = window.devicePixelRatio || 1;
    conn.setFontSize(fontSize() * dpr);
    conn.setFontFamily(resolvedFontWithFallback());
  });

  // Durable map from session ID to its hash-encodable representation
  // ("t:connectionId:ptyId").  Survives connection removal so URL-hash
  // entries for panes assigned to sessions on a removed remote aren't lost.
  const durableSessionHashEntries = new Map<string, string>();

  // Sync layout + focus to URL hash.
  // Assignments are also durably saved to localStorage so they survive
  // even when the hash is lost (new tab, bookmark, etc.).
  createEffect(() => {
    // Record every session we see so the hash can reference sessions whose
    // connection has been removed.  This runs unconditionally (before the
    // connected guard) so entries are populated before they're needed.
    for (const s of sessions()) {
      if (s.ptyId != null) {
        durableSessionHashEntries.set(s.id, `t:${s.connectionId}:${s.ptyId}`);
      }
    }
    if (connection()?.status !== "connected") return;
    const parts: string[] = [];
    const al = activeLayout();
    const paneId = bspFocusedPaneId();
    const la = layoutAssignments();
    const resolved = assignmentsResolved();
    if (al)
      parts.push(`l=${al.name !== al.dsl ? `${al.name}:${al.dsl}` : al.dsl}`);
    if (paneId) parts.push(`p=${paneId}`);
    // Only write pane assignments to the hash when BSPContainer has
    // finished resolving any hash-based entries.  Writing a partial `a=`
    // while resolution is in progress would overwrite the original (complete)
    // `a=` kept from the existing hash, losing entries for connections that
    // haven't become ready yet.
    if (la && resolved) {
      const a = Object.entries(la.assignments)
        .filter(([, sid]) => sid != null)
        .map(([pane, sid]) => {
          const parsed = parseSurfaceAssignment(sid);
          if (parsed) {
            // e.g. "1.0:s:hound:42"
            return `${pane}:s:${parsed.connectionId}:${parsed.surfaceId}`;
          }
          const s = sessions().find((s) => s.id === sid);
          if (s) {
            // e.g. "0:t:hound:28"
            return `${pane}:t:${s.connectionId}:${s.ptyId}`;
          }
          // Session removed (e.g. connection destroyed) — use cached info
          // so the hash entry survives until the remote is re-added.
          const cached = durableSessionHashEntries.get(sid as string);
          return cached ? `${pane}:${cached}` : null;
        })
        .filter(Boolean)
        .join(",");
      if (a) parts.push(`a=${a}`);
    }
    const fSurface = focusedSurfaceId();
    if (fSurface != null) {
      const sConnId = focusedSurfaceConnId() ?? activeConnectionId();
      parts.push(`s=${sConnId}:${fSurface}`);
    }
    const fTerminal = wsState().focusedSessionId;
    if (fTerminal && fSurface == null) parts.push(`t=${fTerminal}`);
    const existing = location.hash.slice(1);
    // Strip layout-managed keys (l, p, a) from the old hash only when we
    // have fresh values to replace them.  While BSPContainer is still
    // resolving hash assignments (assignmentsResolved is false), keep
    // the existing `a=` (and `p=`) so the original shareable hash
    // survives until resolution completes.
    const written = new Set(parts.map((p) => p.slice(0, p.indexOf("="))));
    written.add("l");
    if (paneId) written.add("p");
    if (resolved) written.add("a");
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

  const { countFrame, timeline, net, metrics } = createMetrics(() =>
    props.connectionSpecs().map((s) => s.transport),
  );

  // Periodically bump a counter while the debug panel is open so that
  // debugStats (which reads from non-reactive Maps) gets re-sampled.
  const [debugTick, setDebugTick] = createSignal(0);
  createEffect(() => {
    if (!debugPanel()) return;
    const id = setInterval(() => setDebugTick((n) => n + 1), 1000);
    onCleanup(() => clearInterval(id));
  });

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
          // On mobile, pin to visualViewport so the keyboard doesn't hide content.
          ...(isMobileTouch() && vpHeight()
            ? {
                position: "fixed",
                "inset-inline": "0",
                top: `${vpOffset()}px`,
                height: `${vpHeight()}px`,
              }
            : {}),
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
                        <EmptyPane
                          paneId="__workspace_empty__"
                          label={null}
                          isFocused={true}
                          theme={theme()}
                          palette={palette()}
                          fontSize={fontSize()}
                          connectionId={activeConnectionId()}
                          connectionLabels={connectionLabels()}
                          onCreateInPane={(_paneId, command, connectionId) => {
                            // In non-BSP mode, paneId is irrelevant — we just
                            // create a terminal and focus it.  When the user
                            // didn't type a remote prefix or command and there
                            // are multiple connections, fall back to the
                            // remote picker so they can choose.
                            if (
                              !command &&
                              !connectionId &&
                              allConnections().length > 1
                            ) {
                              openNewTerminalPicker();
                            } else {
                              void createAndFocus(command, connectionId);
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
                            surfaceRef={setTerminalSurface}
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
                  liveSurfaceKeys={surfaces().map(
                    (s) => `${s.connectionId}:${s.surfaceId}`,
                  )}
                  manageVisibility={overlay() !== "expose"}
                  extraVisibleSessions={offScreenSessions().map((s) => s.id)}
                  onAssignmentsChange={setLayoutAssignments}
                  onAssignmentsResolved={setAssignmentsResolved}
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
                  onClearPaneAssignment={(fn) => {
                    clearPaneAssignmentFn = fn;
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
              focusedSurfaceConnId={focusedSurfaceConnId()}
              connectionId={activeConnectionId()}
              connectionLabels={connectionLabels()}
              theme={theme()}
              scale={chromeScale()}
              palette={palette()}
              fontFamily={resolvedFontWithFallback()}
              fontSize={fontSize()}
              isMobileTouch={isMobileTouch()}
              onFocusSession={switchSession}
              onFocusSurface={(connectionId, surfaceId) =>
                focusSurface(surfaceId, connectionId)
              }
              onCloseSession={(id) => void workspace.closeSession(id)}
              onCloseSurface={(connectionId, surfaceId) =>
                workspace.closeSurface(connectionId, surfaceId)
              }
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
                // Clear any focused surface — BSP takes over the main
                // area so the surface overlay won't render, and leaving
                // focusedSurfaceId set would hide the surface from the
                // side panel as well (offScreenSurfaces filters it out).
                focusSurfaceById(null);
                setActiveLayout(l);
                saveActiveLayout(l);
                saveToHistory(l);
                setRecentLayouts(loadRecentLayouts());
                closeOverlay();
              }}
              onRemoveLayout={(dsl) => {
                removeFromHistory(dsl);
                setRecentLayouts(loadRecentLayouts());
              }}
              onClearLayout={() => {
                setLayoutAssignments(null);
                setActiveLayout(null);
                saveActiveLayout(null);
                closeOverlay();
              }}
              recentLayouts={recentLayouts()}
              presetLayouts={PRESETS}
              onChangeFont={() => toggleOverlay("font")}
              onChangePalette={() => toggleOverlay("palette")}
              onChangeRemotes={() => toggleOverlay("remotes")}
              defaultRemote={defaultRemote()}
              remotes={remotes()}
              remoteStatuses={remoteStatuses()}
              surfaces={surfaces()}
              connectionId={activeConnectionId()}
              connectionLabels={connectionLabels()}
              multiConnection={multiConnection()}
              focusedSurfaceId={focusedSurfaceId()}
              focusedSurfaceConnId={focusedSurfaceConnId()}
              onFocusSurface={focusSurface}
              onMoveSurfaceToPane={(sid, connId, targetPaneId) => {
                moveToPaneFn?.(surfaceAssignment(connId, sid), targetPaneId);
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
              gatewayStatus={configWsStatus()}
              palette={palette()}
              fontSize={fontSize()}
              readOnly={false}
              onAdd={(name, uri) => addRemote(name, uri)}
              onRemove={(name) => removeRemote(name)}
              onSetDefault={(name) => setDefaultRemote(name)}
              onReorder={(names) => reorderRemotes(names)}
              onReconnect={(name) => workspace.reconnectConnection(name)}
              onClose={closeOverlay}
            />
          )}
        </Show>
        <Show when={overlay() === "media"}>
          {(_) => (
            <MediaOverlay
              palette={palette()}
              fontSize={fontSize()}
              audioBitrate={audioBitrate()}
              videoQuality={videoQuality()}
              audioMuted={audioMuted()}
              audioAvailable={allConnections().some((c) => c.supportsAudio)}
              surfaceStreaming={surfaceStreaming()}
              onAudioBitrateChange={changeAudioBitrate}
              onVideoQualityChange={changeVideoQuality}
              onSurfaceStreamingChange={changeSurfaceStreaming}
              onToggleAudio={toggleAudio}
              onResetAudio={resetAudio}
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
            "border-top-color": theme().border,
            height: `${chromeScale().md + chromeScale().controlY * 3}px`,
            "font-size": `${chromeScale().md}px`,
          }}
        >
          <StatusBar
            sessions={sessions()}
            surfaceCount={surfaces().length}
            focusedSession={
              focusedSurfaceId() != null || bspFocusedSurface() != null
                ? null
                : focusedSession()
            }
            focusedSurface={(() => {
              const fid = focusedSurfaceId();
              if (fid != null) {
                const fConnId = focusedSurfaceConnId();
                return (
                  surfaces().find(
                    (s) =>
                      s.surfaceId === fid &&
                      (fConnId == null || s.connectionId === fConnId),
                  ) ?? null
                );
              }
              return bspFocusedSurface();
            })()}
            connectionLabels={connectionLabels()}
            connections={allConnections()}
            gatewayStatus={configWsStatus()}
            status={connectionStatus()}
            onRemotes={() => toggleOverlay("remotes")}
            metrics={metrics()}
            palette={palette()}
            fontSize={fontSize()}
            termSize={null}
            fontLoading={fontLoading()}
            debug={debugPanel()}
            toggleDebug={toggleDebug}
            previewPanelOpen={previewPanelOpen()}
            onPreviewPanel={togglePreviewPanel}
            debugStats={
              (debugTick(),
              workspace.getConnectionDebugStats(
                activeConnectionId(),
                wsState().focusedSessionId,
              ))
            }
            timeline={timeline}
            net={net}
            onSwitcher={() => toggleOverlay("expose")}
            onPalette={() => toggleOverlay("palette")}
            onFont={() => toggleOverlay("font")}
            audioMuted={audioMuted()}
            audioAvailable={allConnections().some((c) => c.supportsAudio)}
            hasSurfaces={surfaces().length > 0}
            isMobileTouch={isMobileTouch()}
            keyboardOpen={keyboardWanted()}
            onToggleKeyboard={toggleMobileKeyboard}
            onMedia={() => toggleOverlay("media")}
          />
        </footer>
        <Show when={isMobileTouch() && keyboardWanted()}>
          <MobileToolbar
            workspace={workspace}
            focusedSessionId={() => wsState().focusedSessionId}
            surface={terminalSurface}
            theme={theme()}
            scale={chromeScale()}
          />
        </Show>
      </main>
    </BlitWorkspaceProvider>
  );
}

const MIN_PANEL_WIDTH = 160;

function PreviewPanel(props: {
  offScreenSessions: BlitSession[];
  surfaces: BlitSurface[];
  focusedSurfaceId: number | null;
  focusedSurfaceConnId: ConnectionId | null;
  connectionId: string;
  connectionLabels?: Map<string, string>;
  theme: Theme;
  scale: UIScale;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  isMobileTouch: boolean;
  onFocusSession: (id: SessionId) => void;
  onFocusSurface: (connectionId: ConnectionId, surfaceId: number) => void;
  onCloseSession: (id: SessionId) => void;
  onCloseSurface: (connectionId: ConnectionId, surfaceId: number) => void;
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
                connectionLabel={props.connectionLabels?.get(s.connectionId)}
                theme={props.theme}
                scale={props.scale}
                palette={props.palette}
                fontFamily={props.fontFamily}
                fontSize={props.fontSize}
                isMobileTouch={props.isMobileTouch}
                onFocus={() => props.onFocusSession(s.id)}
                onClose={() => props.onCloseSession(s.id)}
              />
            )}
          </For>
          <For each={props.surfaces}>
            {(s) => (
              <SurfaceThumbnail
                surface={s}
                connectionId={s.connectionId}
                connectionLabel={props.connectionLabels?.get(s.connectionId)}
                theme={props.theme}
                scale={props.scale}
                focused={
                  s.surfaceId === props.focusedSurfaceId &&
                  s.connectionId === props.focusedSurfaceConnId
                }
                isMobileTouch={props.isMobileTouch}
                onFocus={() =>
                  props.onFocusSurface(s.connectionId, s.surfaceId)
                }
                onClose={() =>
                  props.onCloseSurface(s.connectionId, s.surfaceId)
                }
              />
            )}
          </For>
        </div>
      </div>
    </div>
  );
}

/** Minimum horizontal swipe distance (px) to trigger dismiss. */
const SWIPE_THRESHOLD = 60;
/** Minimum ratio of horizontal to vertical movement for a swipe. */
const SWIPE_RATIO = 1.5;

/** Shared wrapper for preview-panel thumbnails.  Handles swipe-to-dismiss,
 *  hover state, dismiss animation, header bar with close button. */
function Thumbnail(props: {
  theme: Theme;
  scale: UIScale;
  isMobileTouch: boolean;
  onFocus: () => void;
  onClose: () => void;
  closeTitle: string;
  /** Extra header-bar background (e.g. for focused highlight). */
  headerBg?: string;
  /** Inline elements rendered inside the header button. */
  header: () => any;
  /** Body content (terminal preview, surface view, etc.). */
  body: () => any;
}) {
  const [hover, setHover] = createSignal(false);
  const [swipeX, setSwipeX] = createSignal(0);
  const [swiping, setSwiping] = createSignal(false);
  const [dismissed, setDismissed] = createSignal(false);
  let touchStartX = 0;
  let touchStartY = 0;
  let locked = false;

  function onTouchStart(e: TouchEvent) {
    const t = e.touches[0];
    touchStartX = t.clientX;
    touchStartY = t.clientY;
    locked = false;
    setSwiping(false);
    setSwipeX(0);
  }

  function onTouchMove(e: TouchEvent) {
    const t = e.touches[0];
    const dx = t.clientX - touchStartX;
    const dy = t.clientY - touchStartY;
    if (!locked) {
      if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return;
      locked = true;
      if (Math.abs(dx) < Math.abs(dy) * SWIPE_RATIO) return;
      setSwiping(true);
    }
    if (!swiping()) return;
    e.preventDefault();
    setSwipeX(dx);
  }

  function onTouchEnd() {
    if (swiping() && Math.abs(swipeX()) >= SWIPE_THRESHOLD) {
      setDismissed(true);
      setSwipeX(swipeX() > 0 ? 400 : -400);
      setTimeout(() => props.onClose(), 200);
    } else {
      setSwipeX(0);
    }
    setSwiping(false);
  }

  return (
    <div
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      onTouchStart={onTouchStart}
      onTouchMove={onTouchMove}
      onTouchEnd={onTouchEnd}
      style={{
        "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        display: dismissed() ? "none" : "flex",
        "flex-direction": "column",
        "flex-shrink": 0,
        overflow: "hidden",
        position: "relative",
        transform: `translateX(${swipeX()}px)`,
        opacity: swiping()
          ? Math.max(0, 1 - Math.abs(swipeX()) / 200)
          : dismissed()
            ? 0
            : 1,
        transition: swiping() ? "none" : "transform 0.2s, opacity 0.2s",
        "touch-action": "pan-y",
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
          "background-color": props.headerBg ?? "transparent",
        }}
      >
        {props.header()}
        <Show when={!props.isMobileTouch && hover()}>
          <button
            onClick={(e) => {
              e.stopPropagation();
              props.onClose();
            }}
            title={props.closeTitle}
            style={{
              ...ui.btn,
              "font-size": `${props.scale.sm}px`,
              padding: `0 ${props.scale.tightGap}px`,
              opacity: 0.6,
              "flex-shrink": 0,
            }}
          >
            {"\u00D7"}
          </button>
        </Show>
      </button>
      <div
        style={{ overflow: "hidden", cursor: "pointer" }}
        onClick={props.onFocus}
      >
        {props.body()}
      </div>
    </div>
  );
}

function SessionThumbnail(props: {
  session: BlitSession;
  connectionLabel?: string;
  theme: Theme;
  scale: UIScale;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  isMobileTouch: boolean;
  onFocus: () => void;
  onClose: () => void;
}) {
  return (
    <Thumbnail
      theme={props.theme}
      scale={props.scale}
      isMobileTouch={props.isMobileTouch}
      onFocus={props.onFocus}
      onClose={props.onClose}
      closeTitle="Close terminal"
      header={() => (
        <>
          <span
            style={{
              flex: 1,
              overflow: "hidden",
              "text-overflow": "ellipsis",
              "white-space": "nowrap",
            }}
          >
            <span style={{ opacity: 0.5 }}>
              {sessionPrefix(props.session, props.connectionLabel)}
            </span>
            {" \u203A "}
            {sessionName(props.session)}
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
        </>
      )}
      body={() => (
        <BlitTerminal
          sessionId={props.session.id}
          readOnly
          showCursor={false}
          style={{ width: "100%", height: "auto" }}
          fontFamily={props.fontFamily}
          fontSize={props.fontSize}
          palette={props.palette}
        />
      )}
    />
  );
}

function SurfaceThumbnail(props: {
  surface: BlitSurface;
  connectionId: string;
  connectionLabel?: string;
  theme: Theme;
  scale: UIScale;
  focused: boolean;
  isMobileTouch: boolean;
  onFocus: () => void;
  onClose: () => void;
}) {
  return (
    <Thumbnail
      theme={props.theme}
      scale={props.scale}
      isMobileTouch={props.isMobileTouch}
      onFocus={props.onFocus}
      onClose={props.onClose}
      closeTitle="Close surface"
      headerBg={props.focused ? props.theme.selectedBg : undefined}
      header={() => (
        <>
          <span
            style={{
              flex: 1,
              overflow: "hidden",
              "text-overflow": "ellipsis",
              "white-space": "nowrap",
            }}
          >
            <Show when={props.connectionLabel}>
              <span style={{ opacity: 0.5 }}>{props.connectionLabel}</span>
              {" \u203A "}
            </Show>
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
        </>
      )}
      body={() => (
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
      )}
    />
  );
}
