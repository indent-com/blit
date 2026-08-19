import {
  createSignal,
  createEffect,
  createMemo,
  onMount,
  onCleanup,
  untrack,
  Show,
  For,
  Index,
  type JSX,
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
import {
  BLIT_SURFACE_TEXT_INPUT_EVENT,
  BlitWorkspace,
  PALETTES,
  LSP_STATUS_OK,
  detectCodecSupport,
  getProbedCodecSupport,
  setAllowedCodecSupport,
  isIOS,
} from "@blit-sh/core";
import type {
  BlitTransport,
  BlitSession,
  BlitSurface,
  BlitTerminalSurface,
  BlitWasmModule,
  SessionId,
  TerminalPalette,
  ConnectionId,
  LinkHover,
  UrlAssessment,
  BlitActivity,
  BlitSurfaceTextInputEvent,
} from "@blit-sh/core";
import type { ConnectionSpec } from "./App";
import { createMetrics } from "./createMetrics";
import { cardAspectRatio, surfaceCardSignature } from "./surfaceAspect";
import { createFontLoader } from "./createFontLoader";
import { loadFontList, saveFontList } from "./fontStore";
import { createKeyboardShortcuts } from "./createKeyboardShortcuts";
import { truncateDocumentEntityTitle } from "./documentTitle";
import {
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  TEXT_GAMMA_KEY,
  AUDIO_BITRATE_KEY,
  AUDIO_MUTED_KEY,
  VIDEO_BANDWIDTH_KEY,
  VIDEO_SPEED_KEY,
  SURFACE_STREAMING_KEY,
  SURFACE_SMOOTHING_KEY,
  SURFACE_MAX_FPS_KEY,
  SURFACE_ZOOM_KEY,
  SURFACE_ZOOM_MODE_KEY,
  SURFACE_TOUCH_MODE_KEY,
  SURFACE_CODECS_KEY,
  WAYLAND_KEYBOARD_REQUESTS_KEY,
  MIN_SURFACE_ZOOM,
  MAX_SURFACE_ZOOM,
  LEFT_DOCK_WIDTH_KEY,
  PREVIEW_PANEL_WIDTH_KEY,
  writeStorage,
  useConfigValue,
  preferredPalette,
  defaultFont,
  preferredFont,
  preferredFontSize,
  urlPinnedKeys,
  preferredTextGamma,
  preferredAudioBitrate,
  preferredAudioMuted,
  preferredVideoBandwidth,
  preferredVideoSpeed,
  preferredSurfaceStreaming,
  preferredSurfaceSmoothing,
  preferredSurfaceMaxFps,
  preferredSurfaceZoom,
  preferredSurfaceZoomMode,
  preferredSurfaceTouchMode,
  preferredSurfaceCodecs,
  preferredWaylandKeyboardRequests,
  preferredLeftDockWidth,
  preferredPreviewPanelWidth,
  MIN_PREVIEW_PANEL_WIDTH,
  preferredLeftDockOpen,
  preferredCollapsedSections,
  LEFT_DOCK_OPEN_KEY,
  LEFT_COLLAPSED_KEY,
  blitHost,
  basePath,
  useRemotes,
  useRoots,
  useDefaultRemote,
  configWsStatus,
  addRemote,
  removeRemote,
  toggleRemote,
  setDefaultRemote,
  reorderRemotes,
  addRoot,
  removeRoot,
  toggleRoot,
  reorderRoots,
  type Root,
  type SurfaceZoomMode,
  type SurfaceTouchMode,
} from "./storage";
import type { UIScale, Theme } from "./theme";
import {
  mergeStyle,
  sessionName,
  sessionPrefix,
  scrollbarStyle,
  themeFor,
  layout,
  ui,
  uiScale,
  z,
} from "./theme";
import { t } from "./i18n";
import { TerminalDropTarget } from "./terminalDrop";
import { StatusBar } from "./StatusBar";
import { DesktopChrome } from "./DesktopChrome";
import { LeftDock, LEFT_PANELS, type LeftPanel } from "./LeftDock";
import { foldedSections, liveOverrides, toggleSection } from "./dockSections";
import {
  ATTENTION_MS,
  armAttention,
  expireAttention,
  type Attention,
} from "./surfaceAttention";
import {
  formatExpandedHash,
  formatPanelsHash,
  parseExpandedHash,
  parsePanelsHash,
} from "./panelHash";
import { fontCatalog } from "./fontCatalog";
import { ExplorerPanel } from "./ide/ExplorerPanel";
import { BranchesPanel } from "./ide/BranchesPanel";
import { LogPanel } from "./ide/LogPanel";
import { SearchPanel } from "./ide/SearchPanel";
import { ResizeHandle } from "./bsp/ResizeHandle";
import { searchInputFocused } from "./ide/searchStore";
import { ProblemsPanel } from "./ide/ProblemsPanel";
import { BlitTile } from "./ide/BlitTile";
import { tileDisplay } from "./ide/tileDisplay";
import {
  startTileDrag,
  startTouchDrag,
  fillTileDrag,
  isTileDrag,
  isPaneDrag,
  paneDragSource,
  tileDragAssignment,
  MAIN_PANE_SOURCE,
} from "./ide/tileDrag";
import {
  tabId,
  stripConn,
  registerTab,
  unregisterTab,
  resolveTab,
} from "./ide/tabRegistry";
import { createOpenTabs } from "./ide/openTabs";
import {
  allServerRoots,
  hasServerRoots,
  ensureServerRoots,
  addServerRoot,
  removeServerRoot,
  toggleServerRoot,
  reorderServerRoots,
} from "./ide/rootsStore";
import { ensureSessionCatalog } from "./sessionCatalogs";
import { useIdeSession, type IdeSessionDescriptor } from "./ide/session";
import {
  currentSourceSessionForPty,
  sourceSessionCanResolveCwd,
} from "./ide/followTerminal";
import { localFileIndex, searchFileIndex } from "./ide/fileIndex";
import { editorRecencySnapshot } from "./ide/editorPositions";
import { SwitcherOverlay } from "./SwitcherOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { LinkOverlay } from "./LinkOverlay";
import { RemotesOverlay } from "./RemotesOverlay";
import { shellCapabilities } from "./shellCapabilities";
import { RootsOverlay } from "./RootsOverlay";
import { MediaOverlay } from "./MediaOverlay";
import { createMediaDevices } from "./mediaDevices";
import { BSPContainer, EmptyPane } from "./bsp/BSPContainer";
import { WebOverlay } from "./WebOverlay";
import type { WebPaneHandle } from "./WebPane";
import { WebPaneHost } from "./WebPaneHost";
import {
  PersistentWebPanes,
  createWebPaneHostRegistry,
} from "./PersistentWebPanes";
import { WebPaneNav } from "./WebPaneNav";
import {
  ensurePreviewWorker,
  loadLocations,
  looksLikeWebLocation,
  previewSupported,
  saveLocations,
  watchPreviewWorker,
  withLocation,
  type WebLocation,
} from "./preview";

import { MobileToolbar } from "./MobileToolbar";
import { PaneTools } from "./PaneTools";
import type { BSPAssignments, BSPLayout } from "./bsp/layout";
import {
  loadActiveLayout,
  loadAssignmentsFromHash,
  loadLayoutFromHash,
  saveActiveLayout,
  saveToHistory,
  removeFromHistory,
  loadRecentLayouts,
  LAYOUT_HISTORY_KEY,
  PRESETS,
  surfaceAssignment,
  isSurfaceAssignment,
  isWebAssignment,
  parseWebAssignment,
  webAssignment,
  isTileAssignment,
  parseTileAssignment,
  parseDiffArg,
  parseSurfaceAssignment,
  editorAssignment,
  manageAssignment,
  layoutFromDSL,
  leafCount,
  loadFocusedTileFromHash,
} from "./bsp/layout";
import { setReveal } from "./ide/reveal";
import { debugPanelOpenFromHash, withDebugPanelState } from "./workspaceUrl";
import {
  cancelHmrRelease,
  claimHmrLease,
  deferHmrRelease,
  type HmrLeaseState,
} from "./hmrLease";

export type Overlay =
  | "expose"
  | "palette"
  | "font"
  | "help"
  | "remotes"
  | "roots"
  | "media"
  | "web"
  | "link"
  | null;

type HmrWorkspaceData = HmrLeaseState & {
  workspace: BlitWorkspace;
  /** Module-local identity. A new object means this module was hot-reloaded. */
  owner: object;
  /** Transport generation owned by the parent ConnectedApp. */
  key: object;
};

const hmrWorkspaceOwner = {};
const BSP_RESIZE_HISTORY_DEBOUNCE_MS = 250;

/**
 * Reset terminal view leases on both current and pre-fix preserved workspaces.
 * HMR keeps the object itself, so an instance constructed by the previous
 * module does not gain methods added to the new class prototype.
 */
function resetHmrViewSizes(workspace: BlitWorkspace): void {
  const current = workspace as BlitWorkspace & {
    resetViewSizes?: () => void;
  };
  if (typeof current.resetViewSizes === "function") {
    current.resetViewSizes();
    return;
  }

  for (const snapshot of workspace.getSnapshot().connections) {
    const connection = workspace.getConnection(snapshot.id);
    if (!connection) continue;
    const legacy = connection as unknown as {
      viewSizes?: Map<SessionId, unknown>;
    };
    const sessionIds = legacy.viewSizes ? [...legacy.viewSizes.keys()] : [];
    legacy.viewSizes?.clear();
    connection.clearSessionSizes(sessionIds);
  }
}

function removeWorkspaceConnections(workspace: BlitWorkspace): void {
  for (const conn of workspace.getSnapshot().connections) {
    workspace.removeConnection(conn.id);
  }
}

function getHmrWorkspace(
  wasm: BlitWasmModule,
  key: object,
  leaseOwner: object,
): HmrWorkspaceData {
  const raw = import.meta.hot?.data?.workspace as
    | HmrWorkspaceData
    | BlitWorkspace
    | undefined;
  // Accept the raw BlitWorkspace stored by versions before HmrWorkspaceData.
  const prev = raw && "workspace" in raw ? raw.workspace : raw;
  const previousOwner = raw && "workspace" in raw ? raw.owner : null;
  const previousKey = raw && "workspace" in raw ? raw.key : null;
  if (prev && previousKey === key) {
    // Solid normally disposes every old terminal surface, but HMR is allowed to
    // replace a component boundary without visiting all of those cleanups. The
    // preserved workspace would then retain the vanished pane's size forever,
    // and the minimum-size policy would leave most of a larger pane blank.
    // Reset once per module generation; the replacement surfaces immediately
    // register their real boxes while transports and terminal state stay live.
    if (previousOwner !== hmrWorkspaceOwner) resetHmrViewSizes(prev);
    const data = raw as HmrWorkspaceData;
    data.owner = hmrWorkspaceOwner;
    claimHmrLease(data, leaseOwner);
    if (import.meta.hot) import.meta.hot.data.workspace = data;
    return data;
  }
  if (prev) {
    if (raw && "workspace" in raw) cancelHmrRelease(raw);
    removeWorkspaceConnections(prev);
  }
  const ws = new BlitWorkspace({ wasm });
  const data = claimHmrLease<HmrWorkspaceData>(
    { workspace: ws, owner: hmrWorkspaceOwner, key },
    leaseOwner,
  );
  if (import.meta.hot) {
    import.meta.hot.data.workspace = data;
  }
  return data;
}

export function Workspace(props: {
  connections: ConnectionSpec[] | (() => ConnectionSpec[]);
  wasm: BlitWasmModule;
  hmrKey?: object;
  onAuthError: () => void;
}) {
  const hmrLeaseOwner = {};
  const hmrKey = props.hmrKey ?? {};
  const hmrData = getHmrWorkspace(props.wasm, hmrKey, hmrLeaseOwner);
  const workspace = hmrData.workspace;

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
    if (import.meta.hot) {
      deferHmrRelease(
        hmrData,
        hmrLeaseOwner,
        () => import.meta.hot?.data?.workspace === hmrData,
        () => removeWorkspaceConnections(workspace),
        () => {
          if (import.meta.hot?.data?.workspace === hmrData) {
            delete import.meta.hot.data.workspace;
          }
        },
      );
    } else {
      removeWorkspaceConnections(workspace);
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
  const [activities, setActivities] = createSignal<readonly BlitActivity[]>(
    workspace.activities.getSnapshot(),
  );
  const unsubscribeActivities = workspace.activities.subscribe(() =>
    setActivities(workspace.activities.getSnapshot()),
  );
  onCleanup(unsubscribeActivities);

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

  // Read-only connections (an `.ro` share): their terminals render without
  // input affordances instead of swallowing keystrokes the server refuses.
  const readOnlyConnections = createMemo(
    () =>
      new Set(
        props
          .connectionSpecs()
          .filter((c) => c.readOnly)
          .map((c) => c.id),
      ),
  );
  const isSessionReadOnly = (sessionId: string): boolean => {
    if (readOnlyConnections().size === 0) return false;
    const s = wsState().sessions.find((x) => x.id === sessionId);
    return !!s && readOnlyConnections().has(s.connectionId);
  };
  /** The same answer about a whole connection, which is what a manage tile
   *  needs: read-only shares drop the client-control family, so its clients
   *  panel must not be offered rather than sit unanswered. */
  const isConnectionReadOnly = (connectionId: string): boolean =>
    readOnlyConnections().has(connectionId as ConnectionId);

  const focusedSession = () => {
    const snap = wsState();
    if (!snap.focusedSessionId) return null;
    return snap.sessions.find((s) => s.id === snap.focusedSessionId) ?? null;
  };
  const focusedSessionId = createMemo(() => wsState().focusedSessionId);

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

  // Viewer camera/microphone/screen sharing. Owned here, not by the media
  // panel: the capability advertisement and the encoder probes have to run
  // whether or not the panel is open, and the status bar reads the same
  // state to decide whether to light its media glyph.
  const mediaDevices = createMediaDevices({
    workspace,
    get connections() {
      return allConnections();
    },
    get connectionLabels() {
      return connectionLabels();
    },
    get readOnlyConnections() {
      return readOnlyConnections();
    },
  });

  const [surfaces, setSurfaces] = createSignal<BlitSurface[]>([]);

  // Per-surface signature of the fields that drive the thumbnail UI
  // (title, appId, and both size pairs — see surfaceCardSignature).
  // SurfaceStore mutates the dimensions
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

  // Connections that completed the handshake.  Same joined-string trick as
  // above so consumers only re-run when readiness actually flips, not on
  // every snapshot.
  const readyConnIdsKey = createMemo(() =>
    wsState()
      .connections.filter((c) => c.ready)
      .map((c) => c.id)
      .sort()
      .join(","),
  );
  const readyConnIds = createMemo(
    () => new Set(readyConnIdsKey().split(",").filter(Boolean)),
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
          const sig = surfaceCardSignature(s);
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
      // A client asking to be activated (xdg_activation_v1 — e.g. an Electron
      // app reacting to a notification click) gets the same treatment as
      // picking its surface in the switcher.
      cleanups.push(
        conn.surfaceStore.onActivated((surfaceId) =>
          activateSurface(surfaceId, spec.id),
        ),
      );
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
  // Content equality: the snapshot fires on every frame/ping, and a fresh Map
  // reference each tick would churn everything downstream that reads statuses.
  const remoteStatuses = createMemo(
    () => {
      const map = new Map<string, import("@blit-sh/core").ConnectionStatus>();
      for (const conn of allConnections()) {
        map.set(conn.id, conn.status);
      }
      return map;
    },
    undefined,
    {
      equals: (a, b) =>
        a != null &&
        a.size === b.size &&
        [...b].every(([name, status]) => a.get(name) === status),
    },
  );

  const [palette, setPalette] =
    createSignal<TerminalPalette>(preferredPalette());
  const [font, setFont] = createSignal(preferredFont());
  const [fontSize, setFontSize] = createSignal(preferredFontSize());
  const [textGamma, setTextGamma] = createSignal(preferredTextGamma());
  const [overlay, setOverlay] = createSignal<Overlay>(null);
  // Whether the active connection serves the systemd watcher. Probed rather
  // than assumed: it is an extension somebody installed, not a server family,
  // and the status bar should not offer a panel with nothing behind it.
  const [openInNewTerminalMode, setOpenInNewTerminalMode] = createSignal(false);
  const [newTerminalTargetPaneId, setNewTerminalTargetPaneId] = createSignal<
    string | null
  >(null);
  const [debugPanel, setDebugPanel] = createSignal(
    debugPanelOpenFromHash(location.hash),
  );
  const [audioMuted, setAudioMuted] = createSignal(preferredAudioMuted());
  const [audioBitrate, setAudioBitrate] = createSignal(preferredAudioBitrate());
  const [videoBandwidth, setVideoBandwidth] = createSignal(
    preferredVideoBandwidth(),
  );
  const [videoSpeed, setVideoSpeed] = createSignal(preferredVideoSpeed());
  const [surfaceStreaming, setSurfaceStreaming] = createSignal(
    preferredSurfaceStreaming(),
  );
  const [surfaceSmoothing, setSurfaceSmoothing] = createSignal(
    preferredSurfaceSmoothing(),
  );
  const [surfaceMaxFps, setSurfaceMaxFps] = createSignal(
    preferredSurfaceMaxFps(),
  );
  const [surfaceZoom, setSurfaceZoom] = createSignal(preferredSurfaceZoom());
  const [surfaceZoomMode, setSurfaceZoomMode] = createSignal(
    preferredSurfaceZoomMode(),
  );
  const [surfaceTouchMode, setSurfaceTouchMode] = createSignal(
    preferredSurfaceTouchMode(),
  );
  const [waylandKeyboardRequests, setWaylandKeyboardRequests] = createSignal(
    preferredWaylandKeyboardRequests(),
  );
  // Applied to the core's cached probe result up front, so the very first
  // C2S_CLIENT_FEATURES already carries the preference instead of advertising
  // everything and correcting itself a moment later.
  const [surfaceCodecs, setSurfaceCodecs] = createSignal(
    preferredSurfaceCodecs(),
  );
  setAllowedCodecSupport(surfaceCodecs());
  const [probedSurfaceCodecs, setProbedSurfaceCodecs] = createSignal(
    getProbedCodecSupport(),
  );
  // The media panel can only offer codecs the decode probe confirmed, and on
  // a terminal-only workspace nothing else ever runs it — a surface view does
  // it on mount. Kicked when the panel opens rather than at startup, since
  // the probe instantiates real decoders. The promise is cached, so a page
  // that already probed answers immediately.
  createEffect(() => {
    if (overlay() !== "media" || probedSurfaceCodecs()) return;
    void detectCodecSupport().then(() =>
      setProbedSurfaceCodecs(getProbedCodecSupport()),
    );
  });
  // Panel chrome in the URL hash (d= open side panels, x= expanded left-dock
  // sections) is authoritative when present; absent keys fall back to
  // localStorage/defaults. Parsed once up front — the focus params (s=/t=)
  // further down read the same object.
  const initHash = new URLSearchParams(location.hash.slice(1));
  const initPanels = parsePanelsHash(initHash.get("d"));
  const [previewPanelOpen, setPreviewPanelOpen] = createSignal(
    initPanels?.preview ?? true,
  );
  const [previewPanelWidth, setPreviewPanelWidth] = createSignal(
    preferredPreviewPanelWidth(),
  );
  // Left dock (docs/ide.md): one dock, opened/closed from the status bar,
  // stacking the IDE sections as a collapsible accordion.
  // Project search: a transient top pane, not persisted — it opens on
  // Ctrl+Shift+F and closes on Escape or its own dismiss button.
  const [searchOpen, setSearchOpen] = createSignal(false);
  // Bumped on every invoke so the panel refocuses its input even when
  // the pane was already open — the shortcut should always land you in
  // the field, not just reveal it.
  const [searchFocus, setSearchFocus] = createSignal(0);
  // null = size to content (capped at half the column); a number pins an
  // explicit fraction after the user drags the handle.
  const [searchHeight, setSearchHeight] = createSignal<number | null>(null);
  /** Dismiss the search pane and hand focus back to whatever was using it.
   *  Closing chrome should return you to the thing underneath — otherwise
   *  focus is left on `document.body` and the next keystroke goes nowhere.
   *  A tile pane owns its own focus, so only a terminal needs the nudge. */
  function closeSearch() {
    setSearchOpen(false);
    queueMicrotask(() => focusedKeyboardInput()?.focus());
  }

  /** Where a drag starts from when the pane was still auto-sized: its
   *  measured share of the column, so the handle does not jump. */
  const autoSearchFraction = () => {
    const el = document.querySelector("[data-blit-search-pane]");
    const parent = el?.parentElement;
    return el && parent && parent.clientHeight > 0
      ? el.clientHeight / parent.clientHeight
      : 0.32;
  };
  const [leftDockOpen, setLeftDockOpen] = createSignal(
    initPanels?.left ?? preferredLeftDockOpen(),
  );
  const [collapsedSections, setCollapsedSections] = createSignal<
    Set<LeftPanel>
  >(
    parseExpandedHash(initHash.get("x")) ??
      new Set(preferredCollapsedSections() as LeftPanel[]),
  );
  // Sections auto-folded because they don't apply here, which the user asked
  // to see anyway. Not persisted: it is an override of a fold this root
  // caused, not a preference about the dock.
  const [foldOverrides, setFoldOverrides] = createSignal<
    ReadonlySet<LeftPanel>
  >(new Set());
  const [sectionWeights, setSectionWeights] = createSignal<
    Record<LeftPanel, number>
  >({ explorer: 1, branches: 1, log: 1, problems: 1 });
  const [leftDockWidth, setLeftDockWidth] = createSignal(
    preferredLeftDockWidth(),
  );

  // Which root the IDE dock is showing: a declared blit.roots entry, or the
  // focused terminal (follow-cd via fromSessionId).
  const gatewayRoots = useRoots();
  // Server-side roots (docs/design/kv.md § Second consumer): each connected
  // kv-capable server owns its `roots` document; the gateway list remains
  // authoritative for servers without the store, and seeds a server's key
  // on first contact.
  createEffect(() => {
    for (const c of wsState().connections) {
      if (c.status !== "connected" || !c.supportsKv) continue;
      const connectionId = c.id;
      ensureServerRoots(workspace, connectionId, c.generation, () =>
        gatewayRoots().filter(
          (r) => connectionForRemote(r.remote) === connectionId,
        ),
      );
    }
  });
  // Each connected server's application catalog, held open so the switcher can
  // filter it from the first keystroke instead of fetching one when it opens.
  // Armed like the roots watch above, and re-armed on the generation for the
  // same reason: a channel does not survive a re-establish.
  createEffect(() => {
    for (const c of wsState().connections) {
      if (c.status !== "connected") continue;
      ensureSessionCatalog(workspace, c.id, c.generation);
    }
  });
  // The picker's list: per-server roots for kv connections, gateway entries
  // only for targets that don't have server-side roots (avoids doubling
  // seeded entries).
  const roots = createMemo<Root[]>(() => [
    ...allServerRoots(),
    ...gatewayRoots().filter(
      (r) => !hasServerRoots(connectionForRemote(r.remote)),
    ),
  ]);
  // A worktree selection is deliberately not a declared root: it is a
  // navigation, not a configured place. It carries its own connection so it
  // survives the focus moving, and a label so the picker can name it without
  // re-deriving a basename.
  type RootSelection =
    | { kind: "focused" }
    | { kind: "declared"; name: string }
    | {
        kind: "worktree";
        connectionId: ConnectionId;
        path: string;
        label: string;
      };
  const [rootSel, setRootSel] = createSignal<RootSelection>({
    kind: "focused",
  });
  // Live cwd of the focused terminal, fed by the cwd poll below: it labels
  // the root-picker's focused-terminal option and shows in the status bar.
  // `sessionId` is what the reading is *about* — a poll that comes back
  // empty leaves the last value in place, so consumers need it to tell a
  // live cwd from one belonging to the terminal they just left.
  const [focusedTerm, setFocusedTerm] = createSignal<{
    sessionId: SessionId;
    conn: string;
    ptyId: number;
    cwd: string;
  } | null>(null);
  // Unlike `focusedTerm`, this survives focusless reconnect windows. It is
  // only a fallback for a sticky terminal anchor after that PTY is confirmed
  // gone, and is bounded by the terminals seen during this workspace mount.
  const lastTerminalCwds = new Map<string, string>();
  const terminalCwdKey = (connectionId: string, ptyId: number): string =>
    `${connectionId}\u0000${ptyId}`;
  // A `cd` OUTSIDE the current session root re-roots the dock there (Files and
  // Log follow the terminal, not just the label). Inside the root, the poll
  // only expands the tree — re-rooting on every subdirectory cd would narrow
  // the view constantly. Set by the poll, consumed by ideDescriptor.
  const [termCwdOverride, setTermCwdOverride] = createSignal<{
    sessionId: SessionId;
    connectionId: ConnectionId;
    cwd: string;
  } | null>(null);

  // What the focused *pane* anchors the IDE root on. A terminal anchors on its
  // live cwd; an editor/diff tile on its file's directory; a commit tile on its
  // repo. So the dock follows whatever pane you focus — not just terminals.
  type FocusAnchor =
    | { kind: "terminal"; session: BlitSession }
    | { kind: "path"; connectionId: ConnectionId; path: string; label: string };

  const dirOf = (abs: string): string => {
    const s = abs.replace(/\/+$/, "");
    const i = s.lastIndexOf("/");
    return i <= 0 ? "/" : s.slice(0, i);
  };

  // Resolve the currently-focused pane to an anchor, or null when the pane has
  // no root to show (a surface, an empty pane) — in which case the last anchor
  // sticks, so the dock never flickers to nothing.
  const focusedPaneAnchor = (): FocusAnchor | null => {
    const assign = inBsp()
      ? (layoutAssignments()?.assignments[bspFocusedPaneId() ?? ""] ?? null)
      : activeTile();
    if (typeof assign === "string" && isTileAssignment(assign)) {
      const t = parseTileAssignment(assign);
      if (t) {
        // A manage tile is a server's panels, not a place in a filesystem: it
        // has no root to anchor on, so the last one sticks.
        if (t.kind === "manage") return null;
        if (t.kind === "commit") {
          const repoPath = t.arg.slice(t.arg.indexOf(":") + 1);
          return {
            kind: "path",
            connectionId: t.connectionId as ConnectionId,
            path: repoPath,
            label: repoPath,
          };
        }
        const file = t.kind === "diff" ? parseDiffArg(t.arg).path : t.arg;
        return {
          kind: "path",
          connectionId: t.connectionId as ConnectionId,
          path: dirOf(file),
          label: file,
        };
      }
    }
    const term = focusedSession();
    // An exited terminal has no live cwd to anchor on — a follow-terminal
    // open against its dead pty can only fail ("source terminal has no
    // working directory"). Treat it as rootless: the last live anchor
    // sticks, or the dock shows no root at all (first open).
    return term && term.state !== "exited"
      ? { kind: "terminal", session: term }
      : null;
  };

  // A stable identity for an anchor, so we only re-emit when the focused pane
  // meaningfully changes — not on every workspace snapshot (terminal frames
  // fire those constantly, and focusedPaneAnchor() allocates a fresh object
  // each call, which would otherwise churn ideDescriptor every frame).
  const anchorKey = (a: FocusAnchor | null): string =>
    !a
      ? ""
      : a.kind === "terminal"
        ? `t:${a.session.id}`
        : `p:${a.connectionId}:${a.path}`;

  // Sticky: keep the last derivable anchor when focus lands on a rootless pane.
  const [lastAnchor, setLastAnchor] = createSignal<FocusAnchor | null>(null);
  createEffect(() => {
    const a = focusedPaneAnchor();
    if (!a) return;
    const k = anchorKey(a);
    setLastAnchor((prev) => (anchorKey(prev) === k ? prev : a));
  });

  // Hoisted declaration: the server-roots memo above runs at component
  // setup, before this point in source order.
  function connectionForRemote(remote: string): ConnectionId {
    return (remote || defaultConnectionId()) as ConnectionId;
  }

  const ideDescriptor = createMemo<IdeSessionDescriptor | null>(() => {
    // No session while the dock is closed — the fs/git syncs would be pure
    // overhead. Editor/diff tiles own their own handles, so they are
    // unaffected. The 30s idle cache keeps a session warm across quick
    // close/reopen and pane switches.
    if (!leftDockOpen()) return null;
    // Nor while every section in it is collapsed. An open dock showing only
    // its three headers reads nothing from the tree, the log or the problem
    // list, so the fs sync and git repo behind them are as much dead weight
    // as when the dock is shut — and collapsing the sections is how a dock
    // gets emptied in practice, since it leaves the pane where it is.
    //
    // Deliberately the user's own collapse set, not the folded set the dock
    // renders: that one counts sections auto-folded for having nothing to
    // show, and those are derived from the session this decides whether to
    // open.
    if (LEFT_PANELS.every((panel) => collapsedSections().has(panel))) {
      return null;
    }
    const sel = rootSel();
    if (sel.kind === "declared") {
      const r = roots().find((x) => x.name === sel.name && !x.disabled);
      if (!r) return null;
      const connectionId = connectionForRemote(r.remote);
      return { key: `d ${connectionId} ${r.path}`, connectionId, path: r.path };
    }
    if (sel.kind === "worktree") {
      // No `preferRepoRoot`: a linked worktree IS the repo root the server
      // resolves for it, and asking to be re-rooted at "the enclosing repo"
      // is exactly how a click on a worktree would snap back to whichever
      // one we came from.
      return {
        key: `w ${sel.connectionId} ${sel.path}`,
        connectionId: sel.connectionId,
        path: sel.path,
      };
    }
    const a = lastAnchor();
    if (!a) return null;
    if (a.kind === "terminal") {
      // A cd outside the session's root re-keys the descriptor at the new
      // cwd (set by the cwd poll), so Files and Log follow the terminal
      // instead of staying on the root resolved at first open.
      const ov = termCwdOverride();
      if (ov && ov.sessionId === a.session.id) {
        return {
          key: `f ${ov.connectionId} ${ov.cwd}`,
          connectionId: ov.connectionId,
          path: ov.cwd,
        };
      }
      // The terminal may exit after becoming the sticky last anchor. Keep the
      // useful root, but stop issuing PTY-relative opens that can only return
      // the server's internal "source terminal has no working directory"
      // diagnostic. The cwd poll gives us the same root as an absolute path;
      // without even one successful poll there is no root to retain.
      const source = currentSourceSessionForPty(
        wsState().sessions,
        a.session.connectionId,
        a.session.ptyId,
      );
      const sourceConnectionReady =
        wsState().connections.find(
          (connection) => connection.id === a.session.connectionId,
        )?.ready ?? false;
      if (!sourceSessionCanResolveCwd(source, sourceConnectionReady)) {
        const last = focusedTerm();
        const lastCwd =
          last &&
          last.conn === a.session.connectionId &&
          last.ptyId === a.session.ptyId
            ? last.cwd
            : lastTerminalCwds.get(
                terminalCwdKey(a.session.connectionId, a.session.ptyId),
              );
        if (!lastCwd) return null;
        return {
          key: `f ${a.session.connectionId} ${lastCwd}`,
          connectionId: a.session.connectionId,
          path: lastCwd,
        };
      }
      return {
        key: `f ${a.session.connectionId} pty${a.session.ptyId}`,
        connectionId: a.session.connectionId,
        path: "",
        fromSessionId: a.session.id,
        // Keyed by pty, so the session survives reconnects that replace every
        // SessionId — the pty is what its opens keep following.
        fromPtyId: a.session.ptyId,
      };
    }
    // Tile-anchored: the fs sync starts at the file's directory (or the
    // commit's repo), but preferRepoRoot re-roots the tree at the enclosing
    // repo once git discovers it — so opening a file shows the whole project.
    return {
      key: `p ${a.connectionId} ${a.path}`,
      connectionId: a.connectionId,
      path: a.path,
      preferRepoRoot: true,
    };
  });
  const activeSession = useIdeSession(workspace, ideDescriptor);

  // Sections with nothing to show for this root: a commit log over a directory
  // that is not a repository (or a remote with no git at all), problems from a
  // remote that cannot run a language server. They fold away rather than
  // sitting open on a message — the space belongs to the panels that do apply —
  // and unfold by themselves once they have something to say.
  const inapplicableSections = createMemo<ReadonlySet<LeftPanel>>(() => {
    const set = new Set<LeftPanel>();
    const s = activeSession();
    // No session at all — nothing picked yet, or a share still connecting —
    // is as empty as a root without a repository, and folds the same way.
    // Now that the log's fold comes from here rather than from a seeded
    // preference, this case has to be named or the log sits open on nothing.
    if (!s || s.noRepo()) set.add("log");
    // Branches folds on exactly the same condition as the log: both are
    // views of a repository, and neither has anything to say without one.
    if (!s || s.noRepo()) set.add("branches");
    if (s?.noLsp()) set.add("problems");
    return set;
  });
  // An override lapses once its section applies again.
  createEffect(() => {
    const inapplicable = inapplicableSections();
    setFoldOverrides((cur) => {
      const next = liveOverrides(cur, inapplicable);
      return next.size === cur.size ? cur : next;
    });
  });
  const collapsedForDock = createMemo(() =>
    foldedSections(
      collapsedSections(),
      inapplicableSections(),
      foldOverrides(),
    ),
  );

  // --- Mobile touch detection & virtual keyboard tracking ---
  const [isMobileTouch, setIsMobileTouch] = createSignal(false);
  const [terminalSurface, setTerminalSurface] =
    createSignal<BlitTerminalSurface | null>(null);

  // --- Terminal hyperlinks ---
  // `hoveredLink` drives the status-bar preview; `pendingLink` is the target
  // awaiting a decision in the confirmation overlay.
  const [hoveredLink, setHoveredLink] = createSignal<LinkHover | null>(null);
  const [pendingLink, setPendingLink] = createSignal<{
    assessment: UrlAssessment;
    text: string;
  } | null>(null);

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
  const [vpBaseHeight, setVpBaseHeight] = createSignal(0);
  onMount(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    let baseWidth = 0;
    const update = () => {
      const height = vv.height;
      const width = vv.width;
      const fullHeight = Math.max(height, window.innerHeight);
      setVpHeight(height);
      setVpOffset(vv.offsetTop);
      setVpBaseHeight((prev) => {
        // A large width change means rotation or device-mode resize; reset the
        // baseline instead of carrying a portrait height into landscape.
        if (baseWidth === 0 || Math.abs(width - baseWidth) > 48) {
          baseWidth = width;
          return fullHeight;
        }

        // Grow with browser chrome collapse.  Also allow small decreases so
        // address-bar changes do not look like a keyboard; never learn a
        // keyboard-shrunken viewport (>150px) as the new baseline.
        if (fullHeight > prev || prev - height <= 150) {
          baseWidth = width;
          return fullHeight;
        }
        return prev;
      });
    };
    update(); // initialise immediately
    vv.addEventListener("resize", update);
    vv.addEventListener("scroll", update);
    window.addEventListener("resize", update);
    const onOrientationChange = () => setTimeout(update, 150);
    screen.orientation?.addEventListener("change", onOrientationChange);
    onCleanup(() => {
      vv.removeEventListener("resize", update);
      vv.removeEventListener("scroll", update);
      window.removeEventListener("resize", update);
      screen.orientation?.removeEventListener("change", onOrientationChange);
    });
  });

  // How much of the layout viewport something is parked over: a software
  // keyboard, but also iPadOS's ~55px shortcut bar when a hardware keyboard is
  // attached, and the floating keyboard.  Only a full keyboard clears 150px,
  // and gating the viewport pin on that number left <main> at its full 100dvh
  // for the smaller two — with the footer, and the keyboard toggle in it,
  // sitting underneath and untappable.  Anything beyond the deadband is also
  // keyboard-open state: the shortcut bar is still an input panel the toggle
  // must be able to dismiss.  The deadband keeps momentum-scroll jitter from
  // thrashing the layout.
  const occlusion = createMemo(() => {
    if (!isMobileTouch()) return 0;
    const h = vpHeight();
    const full = vpBaseHeight();
    if (h === null || full === 0) return 0;
    return Math.max(0, full - h);
  });
  const viewportOccluded = createMemo(() => occlusion() > 32);

  // Sticky virtual keyboard: track explicit user intent so the keyboard
  // isn't dismissed when tapping elsewhere on the page.
  const [keyboardWanted, setKeyboardWanted] = createSignal(false);
  // A remote Wayland enable may raise the keyboard automatically. An
  // explicit status-bar toggle outranks its later disable until the user
  // dismisses or toggles the keyboard again.
  let keyboardManualOverride = false;
  let automaticKeyboardInput: HTMLTextAreaElement | null = null;
  const terminalInputSelector =
    'textarea[aria-label="Terminal input"][tabindex]:not([readonly])';
  // A surface pane's IME textarea (BlitSurfaceCanvas creates it next to the
  // canvas).  It routes keydown/keyup and composition into the surface, so
  // it is what the software keyboard has to rest on — the canvas itself is
  // not editable and an IME will not stay up for it.
  const surfaceInputSelector = 'textarea[aria-label="Surface input"]';
  const keyboardInputSelector = `${terminalInputSelector}, ${surfaceInputSelector}`;

  // The software keyboard rises only from the status-bar toggle, never from a
  // tap: while it isn't wanted, every terminal and surface textarea carries
  // inputmode="none", which keeps focus semantics (hardware keys, scrollback
  // navigation, paste) but tells the browser not to bring up an IME.  The
  // observer exists because the textareas are created whenever a pane
  // mounts, and the attribute has to be in place before the tap that focuses
  // them — stamping on focus is too late for the IME decision.
  const stampSelector =
    'textarea[aria-label="Terminal input"], textarea[aria-label="Surface input"]';
  createEffect(() => {
    // `suppress` is false when leaving touch mode too (a DevTools device-mode
    // flip), so that pass strips stale stamps before bailing.
    const suppress = isMobileTouch() && !keyboardWanted();
    const stampOne = (el: Element) => {
      if (suppress) el.setAttribute("inputmode", "none");
      else {
        const desired = (el as HTMLElement).dataset.blitInputmode;
        if (desired) el.setAttribute("inputmode", desired);
        else el.removeAttribute("inputmode");
      }
    };
    const stamp = (root: ParentNode) => {
      for (const el of root.querySelectorAll(stampSelector)) stampOne(el);
    };
    stamp(document);
    if (!isMobileTouch()) return;
    const mo = new MutationObserver((records) => {
      for (const r of records) {
        for (const n of r.addedNodes) {
          if (!(n instanceof HTMLElement)) continue;
          if (n.matches(stampSelector)) stampOne(n);
          else stamp(n);
        }
      }
    });
    mo.observe(document.body, { childList: true, subtree: true });
    onCleanup(() => mo.disconnect());
  });

  // The focused pane's terminal or surface input, else the first one on
  // screen that can take focus.  Every fallback matters: a pane holding an
  // editor or a web view has no keyboard input at all, and until something
  // is tapped no pane carries the focused marker.  Resolving to null there
  // left the keyboard toggle dead for good — it returns before flipping
  // `keyboardWanted`, so every later tap took the same branch and did
  // nothing.  Reaching into another pane is safe: that pane's own focusin
  // moves BSP focus to match, so the caret never lands out of sight.
  function focusedKeyboardInput(): HTMLElement | null {
    // A soloed-away pane and a background tab are `display:none`, which
    // leaves the input with no client rects.  focus() there is a silent
    // no-op, so returning one lit the icon over a keyboard that never came
    // up.  offsetParent can't be the test: the IME textareas are
    // position:fixed (pinned to the screen top, always clear of the
    // keyboard), and offsetParent is null on fixed elements even when
    // rendered.  Parked thumbnails are `inert` — same silent no-op, but
    // with boxes still laid out, so they need their own check.
    const focusable = (el: HTMLElement | null | undefined) =>
      el && el.getClientRects().length > 0 && !el.closest("[inert]")
        ? el
        : null;
    const active = document.activeElement;
    if (
      active instanceof HTMLElement &&
      active.matches(keyboardInputSelector)
    ) {
      const focused = focusable(active);
      if (focused) return focused;
    }
    const focusedPane = document.querySelector<HTMLElement>(
      '[data-blit-bsp-focused="true"]',
    );
    return (
      focusable(
        focusedPane?.querySelector<HTMLElement>(terminalInputSelector),
      ) ??
      focusable(
        focusedPane?.querySelector<HTMLElement>(surfaceInputSelector),
      ) ??
      [
        ...document.querySelectorAll<HTMLElement>(
          `section ${terminalInputSelector}`,
        ),
        ...document.querySelectorAll<HTMLElement>(
          `section ${surfaceInputSelector}`,
        ),
      ].find((el) => focusable(el)) ??
      null
    );
  }

  function enableKeyboardInput(el: HTMLElement): void {
    const desired = el.dataset.blitInputmode;
    if (desired) el.setAttribute("inputmode", desired);
    else el.removeAttribute("inputmode");
  }

  // A committed Wayland text-input enable is the remote field asking for an
  // input panel. Only honor it for the surface this viewer already focused;
  // another viewer shares the same Wayland seat and must not pop keyboards on
  // every connected phone. Browser policy still makes showing best-effort.
  onMount(() => {
    const handler = (raw: Event) => {
      const event = raw as BlitSurfaceTextInputEvent;
      const input = event.target;
      if (!(input instanceof HTMLTextAreaElement)) return;
      if (!input.matches(surfaceInputSelector) || !isMobileTouch()) return;
      if (!waylandKeyboardRequests()) return;

      const state = event.detail;
      if (!state.enabled) {
        if (automaticKeyboardInput !== input || keyboardManualOverride) return;
        queueMicrotask(() => {
          // An old surface's disable can be immediately followed by the new
          // focused field's enable. Let that handoff replace the owner before
          // deciding whether the keyboard should go away.
          if (automaticKeyboardInput !== input || keyboardManualOverride)
            return;
          automaticKeyboardInput = null;
          setKeyboardWanted(false);
          if (document.activeElement === input) input.blur();
        });
        return;
      }
      if (!state.requested) return;
      const locallyFocused =
        document.activeElement === input ||
        !!input.closest('[data-blit-bsp-focused="true"]');
      if (!locallyFocused) return;

      if (!keyboardWanted()) {
        keyboardManualOverride = false;
        automaticKeyboardInput = input;
        setKeyboardWanted(true);
      } else if (!keyboardManualOverride) {
        automaticKeyboardInput = input;
      }
      enableKeyboardInput(input);
      input.focus({ preventScroll: true });
      try {
        (
          navigator as { virtualKeyboard?: { show?: () => void } }
        ).virtualKeyboard?.show?.();
      } catch {
        // Safari/Chromium may reject programmatic show without a sufficiently
        // recent user activation; focusing the editable target is the
        // portable best effort.
      }
    };
    document.addEventListener(BLIT_SURFACE_TEXT_INPUT_EVENT, handler);
    onCleanup(() =>
      document.removeEventListener(BLIT_SURFACE_TEXT_INPUT_EVENT, handler),
    );
  });

  function focusSettledElsewhere(): boolean {
    const active = document.activeElement;
    if (!(active instanceof HTMLElement)) return false;
    if (active.matches(terminalInputSelector)) return true;
    if (!active.closest("section")) return false;
    // CodeMirror focuses a contenteditable div, not a textarea.  Without it
    // here the sticky re-focus reads an editor as "nothing took focus" and
    // now that the terminal lookup falls back across panes, drags the caret
    // out of the editor the user just tapped into.
    return active.matches(
      'input, textarea, select, canvas[tabindex], [contenteditable="true"]',
    );
  }

  // The keyboard going away is the user putting it away — iPadOS has a
  // dedicated dismiss key, which produces a blur we cannot tell apart from
  // "tapped a button", so intent has to be read off the viewport instead.
  // Latching on the first occlusion keeps the gap between the tap and the
  // keyboard animating in from counting as a dismissal.  This is also what
  // stops the icon lying: it tracks intent, and intent now expires when the
  // keyboard does.
  let keyboardSeen = false;
  createEffect(() => {
    if (!keyboardWanted()) {
      keyboardSeen = false;
      // inputmode="none" means taps no longer raise the IME, but the OS still
      // can (a keyboard-show gesture, stylus handwriting input).  If an input
      // panel is genuinely up over a focused terminal, latch intent from
      // reality so the icon and toolbar match what's on screen.  This includes
      // iPadOS's shortcut bar: although it is not a full software keyboard, it
      // must take the same hide path.  Focus gating keeps the drain after an
      // explicit hide (the toggle blurred, occlusion not yet gone) from
      // re-latching.
      if (
        viewportOccluded() &&
        document.activeElement instanceof HTMLElement &&
        document.activeElement.matches(keyboardInputSelector)
      ) {
        keyboardManualOverride = true;
        automaticKeyboardInput = null;
        setKeyboardWanted(true);
      }
      return;
    }
    if (viewportOccluded()) keyboardSeen = true;
    else if (keyboardSeen) {
      keyboardManualOverride = false;
      automaticKeyboardInput = null;
      setKeyboardWanted(false);
    }
  });

  // While the keyboard is wanted, focus landing on a surface canvas would
  // dismiss the IME — a canvas is not editable.  BlitSurfaceCanvas hands its
  // own canvas focus to the textarea beside it (an IME will not start a
  // composition otherwise, on any platform), so this capture-phase pass is
  // the net beneath it: it catches a canvas in a pane whatever put it there,
  // and runs first, which makes the two agree rather than compete.  Keys
  // still reach the surface because the textarea routes keydown/keyup and
  // composition through the same handlers as the canvas.
  createEffect(() => {
    if (!isMobileTouch() || !keyboardWanted()) return;
    const handler = (e: FocusEvent) => {
      const t = e.target;
      if (!(t instanceof HTMLCanvasElement) || !t.closest("section")) return;
      t.parentElement
        ?.querySelector<HTMLElement>(surfaceInputSelector)
        ?.focus();
    };
    document.addEventListener("focusin", handler, true);
    onCleanup(() => document.removeEventListener("focusin", handler, true));
  });

  // Re-focus the keyboard-holding textarea when it blurs while the user
  // wants the keyboard open, unless an overlay is active.
  createEffect(() => {
    if (!isMobileTouch() || !keyboardWanted()) return;
    const handler = (e: FocusEvent) => {
      if (!(e.target instanceof HTMLTextAreaElement)) return;
      if (!e.target.matches(keyboardInputSelector)) return;
      if (!(e.target as Element).closest?.("section")) return;
      if (overlay()) return;
      // Long enough to outlast the dismiss animation, so the effect above has
      // cleared `keyboardWanted` and this bails rather than shoving the
      // keyboard back up.  A tap that merely stole focus never lowers the
      // keyboard, so nothing is visibly slower for the case this exists for.
      setTimeout(() => {
        if (!keyboardWanted() || overlay()) return;
        if (focusSettledElsewhere()) return;
        focusedKeyboardInput()?.focus();
      }, 300);
    };
    document.addEventListener("focusout", handler, true);
    onCleanup(() => document.removeEventListener("focusout", handler, true));
  });

  /** Toggle the virtual keyboard on mobile. */
  // Completes the iPadOS focus hop (see toggleMobileKeyboard): the real
  // target gets focus back once something is genuinely parked over the
  // viewport — the only signal that WebKit accepted the host's assist.
  let pendingHopLand: (() => void) | null = null;
  createEffect(() => {
    // Read viewportOccluded() unconditionally: short-circuiting it behind
    // pendingHopLand would subscribe to nothing on the first run, and the
    // effect would never fire.
    const covered = viewportOccluded();
    const land = pendingHopLand;
    if (land && covered) {
      pendingHopLand = null;
      land();
    }
  });

  // The focus-hop host for iPadOS (see toggleMobileKeyboard): a plain 1px
  // textarea at the document level.  It must stay outside `section` (so the
  // sticky-refocus net reads it as "nothing took focus") and outside the
  // inputmode-stamping selectors (so it keeps a real inputmode and the IME
  // will assist it).
  let keyboardHost: HTMLTextAreaElement | null = null;
  function keyboardHostEl(): HTMLTextAreaElement {
    if (!keyboardHost || !keyboardHost.isConnected) {
      keyboardHost = document.createElement("textarea");
      keyboardHost.setAttribute("aria-label", "Keyboard host");
      Object.assign(keyboardHost.style, {
        position: "fixed",
        top: "0",
        left: "0",
        width: "1px",
        height: "1px",
        opacity: "0",
        padding: "0",
        border: "none",
        outline: "none",
        resize: "none",
        overflow: "hidden",
      });
      document.body.appendChild(keyboardHost);
    }
    return keyboardHost;
  }

  // What held focus just before the current tap.  Whether a tapped button
  // takes focus differs by engine (iPadOS: no; Chromium: yes, during the
  // tap's click), so the already-focused decision in the toggle reads this
  // snapshot — the state *before* the tap's own focus churn — instead of
  // the live activeElement at handler time.
  let preTapFocus: Element | null = null;
  const snapshotPreTapFocus = () => {
    preTapFocus = document.activeElement;
  };
  document.addEventListener("pointerdown", snapshotPreTapFocus, true);
  onCleanup(() =>
    document.removeEventListener("pointerdown", snapshotPreTapFocus, true),
  );

  function toggleMobileKeyboard() {
    // A tap means "put it away" when any keyboard input panel is genuinely
    // up, including iPadOS's shortcut bar.  While intent is lit but no panel
    // rose — the IME refused the focus transition, or the tap landed while the
    // last keyboard was still draining — the tap asks for the keyboard again,
    // and taking the hide branch is exactly backwards.
    if (keyboardWanted() && viewportOccluded()) {
      keyboardManualOverride = false;
      automaticKeyboardInput = null;
      setKeyboardWanted(false);
      // Blur whatever actually holds the keyboard.  Matching only the terminal
      // selector missed a focused editor, and the fallback then blurred a
      // terminal that wasn't the one typing — the icon dimmed and the toolbar
      // unmounted with the keyboard still up.
      const active = document.activeElement;
      if (active instanceof HTMLElement && active.closest("section")) {
        active.blur();
      } else {
        focusedKeyboardInput()?.blur();
      }
    } else {
      const el = focusedKeyboardInput();
      if (!el) return;
      keyboardManualOverride = true;
      automaticKeyboardInput = null;
      setKeyboardWanted(true);
      // The stamping effect above has cleared inputmode="none" by now (Solid
      // runs it synchronously on the write), but the IME decision happens on
      // this very element in this very gesture — clear it directly rather
      // than trust effect ordering.
      enableKeyboardInput(el);
      if (el === preTapFocus) {
        // A keyboard already up for this very element was only missing the
        // intent — adopt it without any focus churn, which would just
        // flicker the keyboard.
        if (viewportOccluded()) return;
        if (isIOS()) {
          // iPadOS only answers a focus CHANGE: focus() on the element that
          // already holds focus is a no-op, and blur+focus within one tap
          // nets to zero — no keyboard.  (The tell: switching panes raised
          // the keyboard, because that lands focus on a *different*
          // element.)  Hop focus through a neutral host the IME freshly
          // assists, then hand it to the real target — editable→editable
          // moves keep the keyboard.  The host lives outside any pane, so
          // it never holds focus when a show tap happens and every hop is
          // a real change.
          const host = keyboardHostEl();
          el.blur();
          host.focus();
          // The handback runs when the keyboard is actually rising — the
          // occlusion reading is the only proof WebKit accepted the assist —
          // with a timeout as the fallback for a keyboard that never shows,
          // so focus isn't parked on the host forever.
          pendingHopLand = () => {
            if (keyboardWanted() && document.activeElement === host) el.focus();
          };
          setTimeout(() => {
            const land = pendingHopLand;
            pendingHopLand = null;
            land?.();
          }, 600);
          return;
        }
        // Android leaves the textarea focused with no keyboard up — the
        // pane-focus effect focuses it at load with no user gesture (Chrome
        // moves focus but raises no IME), and the Back gesture dismisses the
        // IME without a blur.  focus() on the already-focused element is a
        // spec'd no-op no keyboard answers, so force a real transition.  This
        // must not be gated on the occlusion reading: a keyboard still
        // draining after an OS dismiss sits over 150px for a moment, and
        // skipping the blur there made this focus() a no-op — the tap lit
        // the icon over a keyboard that never rose, and the keyboard then
        // took extra taps to appear.
        el.blur();
      }
      el.focus();
      // Chromium's IME can stay down for a programmatic focus() even inside
      // a tap; where this API exists (Chrome on Android) it raises the
      // keyboard directly, and it fails silently everywhere else.  Safari
      // has no virtualKeyboard object.
      (
        navigator as { virtualKeyboard?: { show?: () => void } }
      ).virtualKeyboard?.show?.();
    }
  }

  // Focus params from the URL hash (initHash is parsed at the top of the
  // component, next to the panel-chrome restore).
  // Surface: s=<connectionId>:<surfaceId>
  // Terminal: t=<sessionId>  (sessionId is already "<connectionId>:<counter>")
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

  // Surfaces that asked to come forward (xdg_activation_v1) and were answered
  // with a highlight rather than the view — see ./surfaceAttention.ts for why
  // an activation must not move anything.
  const [attention, setAttention] = createSignal<Attention>(new Map());
  /** True while `assignment` is lit; what the dock card and the pane read. */
  const hasAttention = (assignment: string) => attention().has(assignment);
  // One sweep in flight at a time, aimed at the soonest window to close and
  // re-aimed at whatever is left. A timer per request would be a timer per
  // *repeat*, and a chatty client sends several a second; a fixed interval
  // would leave a later arrival lit past its window, holding off its own next
  // pulse for as long as it was late.
  let attentionSweep: ReturnType<typeof setTimeout> | null = null;
  function scheduleAttentionSweep() {
    if (attentionSweep != null) return;
    const lit = untrack(attention);
    if (lit.size === 0) return;
    const soonest = Math.min(...lit.values());
    attentionSweep = setTimeout(
      () => {
        attentionSweep = null;
        const next = expireAttention(untrack(attention), Date.now());
        setAttention(next);
        scheduleAttentionSweep();
      },
      // A hair past the deadline: expireAttention drops a window only once it
      // is strictly over, so landing exactly on it would sweep nothing and
      // re-arm for 0ms, in a loop.
      Math.max(16, soonest - Date.now() + 16),
    );
  }
  function flashAttention(assignment: string) {
    setAttention((prev) => armAttention(prev, assignment, Date.now()));
    scheduleAttentionSweep();
  }
  onCleanup(() => {
    if (attentionSweep != null) clearTimeout(attentionSweep);
  });

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
  // Only into the single main view: under a multi-pane layout the surface is
  // placed by `a=` instead, and filling the non-BSP slot with it would leave a
  // focused surface nothing renders — which every shortcut gated on
  // hasFocusedWaylandSurface would then obey for the rest of the session.
  if (pendingSurfaceFromHash != null) {
    let surfaceRestored = false;
    createEffect(() => {
      if (surfaceRestored) return;
      if (inBsp()) {
        surfaceRestored = true;
        return;
      }
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
  let serverFontsLoaded = false;
  let serverFontsRequest: Promise<void> | null = null;

  function loadServerFonts(): void {
    // The listing is a blit-server HTTP route; embedded on a static origin
    // it is a guaranteed 404, and the local font stack is the answer.
    if (!shellCapabilities().serverRoutes) return;
    if (serverFontsLoaded || serverFontsRequest) return;

    // A remembered listing fills the picker without a round trip. It is only
    // refetched once a day, which is how long the route says it is good for —
    // a font installed on the server shows up in the picker by then.
    const remembered = loadFontList(basePath);
    if (remembered) {
      setServerFonts(remembered.fonts);
      if (!remembered.stale) {
        serverFontsLoaded = true;
        return;
      }
    }

    serverFontsRequest = fetch(`${basePath}fonts`)
      .then(async (r): Promise<string[]> => {
        if (!r.ok) throw new Error(`font list ${r.status}`);
        const json: unknown = await r.json();
        if (!Array.isArray(json)) {
          throw new Error("font list response is not an array");
        }
        return json.filter(
          (font): font is string =>
            typeof font === "string" && font.trim().length > 0,
        );
      })
      .then((fonts) => {
        setServerFonts(fonts);
        saveFontList(basePath, fonts);
        serverFontsLoaded = true;
      })
      .catch(() => {
        // Retry when the font picker is opened.  Font listing is served by the
        // HTTP /fonts route and must not depend on config-WS/server-side config
        // persistence being available.
      })
      .finally(() => {
        serverFontsRequest = null;
      });
  }

  const { resolvedFont, fontLoading, advanceRatio } = createFontLoader(
    font,
    defaultFont(),
  );
  const [activeLayout, setActiveLayoutSignal] = createSignal<BSPLayout | null>(
    loadActiveLayout(),
  );
  // BSP resize pointermoves update the layout continuously for live feedback.
  // Defer the shareable URL until the drag settles so history.replaceState is
  // not called at pointer-event frequency. The flush signal makes the URL
  // effect rebuild from current state instead of committing a stale hash.
  const [historyReplaceFlush, setHistoryReplaceFlush] = createSignal(0);
  let bspResizeHistoryPending = false;
  let bspResizeHistoryTimer: ReturnType<typeof setTimeout> | undefined;
  function setActiveLayout(layout: BSPLayout | null) {
    const flushPendingHistory = bspResizeHistoryPending;
    bspResizeHistoryPending = false;
    clearTimeout(bspResizeHistoryTimer);
    bspResizeHistoryTimer = undefined;
    setActiveLayoutSignal(layout);
    if (flushPendingHistory) setHistoryReplaceFlush((n) => n + 1);
  }
  function setBspLayout(
    layout: BSPLayout | null,
    options?: { debounceHistory?: boolean },
  ) {
    if (options?.debounceHistory) {
      bspResizeHistoryPending = true;
      clearTimeout(bspResizeHistoryTimer);
      bspResizeHistoryTimer = setTimeout(() => {
        bspResizeHistoryTimer = undefined;
        bspResizeHistoryPending = false;
        setHistoryReplaceFlush((n) => n + 1);
      }, BSP_RESIZE_HISTORY_DEBOUNCE_MS);
    } else {
      setActiveLayout(layout);
      return;
    }
    setActiveLayoutSignal(layout);
  }
  onCleanup(() => clearTimeout(bspResizeHistoryTimer));
  const [recentLayouts, setRecentLayouts] = createSignal(loadRecentLayouts());
  const [layoutAssignments, setLayoutAssignments] =
    createSignal<BSPAssignments | null>(null);
  /** True once BSPContainer has finished resolving hash-based assignments.
   *  Seeded false when the hash actually carries some, so the writer never
   *  treats "nothing resolved yet" as "nothing to keep" in the window
   *  before BSPContainer reports in. */
  const [assignmentsResolved, setAssignmentsResolved] = createSignal(
    loadAssignmentsFromHash() == null,
  );

  // Non-BSP "focused tile": an IDE tile (editor/diff/commit) shown in place of
  // the terminal when the user isn't in a multi-pane BSP layout. Opening a tile
  // must NOT swap the user into BSP — it just replaces the main view, and the
  // terminal returns when the tile is dismissed.
  // The hash's tile= param is a short "conn:tabId" ref (docs/design/kv.md);
  // it resolves asynchronously against the server's tabs/ registry once the
  // connection reports the kv capability. Until then the ref parks here and
  // the hash writer preserves the existing tile= param.
  const [activeTile, setActiveTile] = createSignal<string | null>(null);
  const [pendingActiveTileRef, setPendingActiveTileRef] = createSignal<{
    connectionId: ConnectionId;
    id: string;
  } | null>(
    (() => {
      const ref = loadFocusedTileFromHash();
      if (!ref) return null;
      const lastColon = ref.lastIndexOf(":");
      if (lastColon <= 0) return null;
      return {
        connectionId: ref.slice(0, lastColon) as ConnectionId,
        id: ref.slice(lastColon + 1),
      };
    })(),
  );
  let activeTileFetchInFlight = false;
  let activeTileFetchRetries = 0;
  // Retry rides a signal: the in-flight early-return narrows this effect's
  // dependencies to the ref alone, so a plain-variable reset in the catch
  // would never re-trigger it (Solid re-tracks per run).
  const [activeTileRetry, setActiveTileRetry] = createSignal(0);
  createEffect(() => {
    activeTileRetry();
    const ref = pendingActiveTileRef();
    if (!ref || activeTileFetchInFlight) return;
    const conn = wsState().connections.find((c) => c.id === ref.connectionId);
    if (!conn) return; // not added yet — keep waiting, the hash keeps the ref
    if (!conn.supportsKv) {
      if (conn.ready) setPendingActiveTileRef(null); // ready and no kv: give up
      return;
    }
    // The ref stays set until the fetch settles DEFINITIVELY, so the hash
    // writer keeps the tile= param alive for the whole flight. Transient
    // failures (a boot-time re-establish rejects in-flight requests) re-arm
    // and retry on the next snapshot change, bounded like every other
    // re-establish retry in the tree.
    activeTileFetchInFlight = true;
    resolveTab(workspace, ref.connectionId, ref.id)
      .then((assignment) => {
        // Apply only if the user hasn't opened anything meanwhile.
        if (assignment && !activeTile()) setActiveTile(assignment);
        setPendingActiveTileRef(null);
      })
      .catch(() => {
        activeTileFetchInFlight = false;
        if (++activeTileFetchRetries > 20) setPendingActiveTileRef(null);
        else setActiveTileRetry((n) => n + 1);
      });
  });
  // Every tile this client has displayed, most-recent first. This is the
  // FALLBACK ordering/source for the dock: the server registry below is the
  // real one, but a host without FEATURE_KV contributes nothing to it, and
  // this list keeps the dock working there exactly as it did before.
  // Session-only; explicit closes prune it.
  const [localTabs, setLocalTabs] = createSignal<string[]>([]);
  // Recording pushes one entry per file navigated past, so the list is
  // LRU-capped — an unbounded dock also meant unbounded live fs syncs,
  // which is how BLIT_FS_MAX_SYNCS got exhausted in normal browsing.
  const BACKGROUND_TILES_MAX = 50;
  // Only the most recent cards render as live tiles (each live editor holds
  // a content sync of its parent dir); the rest are title-only.
  const LIVE_DOCK_PREVIEWS = 6;
  function recordLocalTab(assignment: string) {
    setLocalTabs((prev) =>
      [assignment, ...prev.filter((a) => a !== assignment)].slice(
        0,
        BACKGROUND_TILES_MAX,
      ),
    );
  }
  /** Close a tab everywhere: drop the server registry record and the local
   *  fallback entry. The counterpart to `registerTab`, and now the ONLY thing
   *  that unregisters — see the effect below. */
  function closeTab(assignment: string) {
    setLocalTabs((prev) => prev.filter((a) => a !== assignment));
    unregisterTab(workspace, assignment);
  }
  // The host-wide open-tab list, mirrored from every connected server's `tabs/`
  // prefix (docs/design/kv.md, ./ide/openTabs.ts).
  const openTabs = createOpenTabs(workspace, () => wsState().connections);
  /**
   * The dock: every open tab, on every connected host, that this client is not
   * currently displaying. DERIVED, not stored — which is the whole point:
   * defocusing a tile can no longer lose it (it merely stops being displayed,
   * and reappears here), and a tab opened in another frontend shows up here
   * without this one having done anything.
   */
  const backgroundTiles = createMemo<string[]>(() => {
    const displayed = new Set<string>();
    for (const v of Object.values(layoutAssignments()?.assignments ?? {})) {
      if (typeof v === "string") displayed.add(v);
    }
    const at = activeTile();
    if (at) displayed.add(at);
    const out: string[] = [];
    const seen = new Set<string>();
    const take = (a: string) => {
      if (displayed.has(a) || seen.has(a)) return;
      if (!isTileAssignment(a) && !isWebAssignment(a)) return;
      seen.add(a);
      out.push(a);
    };
    // Registry first (mtime order — registration is a put on every open, so
    // newest-touched sorts first); the local list then appends anything the
    // registry doesn't know about, which on a kv-less host is all of it.
    for (const tab of openTabs()) take(tab.assignment);
    for (const a of localTabs()) take(a);
    return out.slice(0, BACKGROUND_TILES_MAX);
  });
  /**
   * Everything open, in the order Alt+Shift+[ / ] walks it: terminals, then
   * surfaces, then tabs — the dock's own top-to-bottom order, so the chord
   * agrees with what the eye already scanned. Terminals and surfaces are
   * listed in their arrival order, which is what those two signals already
   * hold.
   *
   * The tab block cannot simply follow `openTabs`, which is ordered by
   * recency: displaying a tab re-registers it (the effect below), so walking
   * the ring would float each tab to the front as it was reached and leave the
   * chord ping-ponging between the last two it touched. So a tab keeps the
   * slot it had on the previous pass and only newcomers append — the sequence
   * they were opened in, which holds still because opening is the only thing
   * that changes it. Solid hands the previous value to the memo, so the order
   * is carried without a signal of its own.
   */
  const cycleRing = createMemo<string[]>((prev) => {
    const out: string[] = [];
    for (const s of sessions()) if (s.state !== "closed") out.push(s.id);
    // Subsurfaces are composited into their parent — only a top-level window
    // is somewhere focus can land.
    for (const s of surfaces()) {
      if (s.parentId === 0) {
        out.push(surfaceAssignment(s.connectionId, s.surfaceId));
      }
    }
    const tabs = new Set<string>();
    for (const tab of openTabs()) tabs.add(tab.assignment);
    for (const a of localTabs()) tabs.add(a);
    // `delete` returns whether it was there, so this both keeps the old order
    // and leaves only the newcomers behind — and a tab that has closed drops
    // out, rather than holding its slot forever.
    for (const a of prev) if (tabs.delete(a)) out.push(a);
    out.push(...tabs);
    return out;
  }, []);
  // One prev/next pass over the displayed set (pane assignments plus the
  // non-BSP active tile) serves two jobs:
  //
  //  - registration: a tile ENTERING the set is written to the server's tabs/
  //    registry (docs/design/kv.md) so hash refs resolve anywhere, and
  //    recorded in the local fallback list;
  //  - in-place replacement: the Edit⇄Staged⇄Unstaged switcher REPLACES a tab
  //    rather than opening a second one beside it, so the outgoing view is
  //    closed — otherwise it would linger in the dock as a stale card.
  //
  // Departures are otherwise NOT unregistered. Deletion is an explicit close
  // now, because the registry is shared: driving it from one client's
  // displayed set let that client delete the record another client's URL
  // hash points at, and the tile silently vanished there on reload.
  //
  // Gated on hash resolution (and the pending tile= ref) so boot churn never
  // writes.
  // Two tiles view "the same file" when their connection + path match —
  // the in-pane Edit⇄Staged⇄Unstaged switcher. Commits never match (their
  // identity is an oid, not a file).
  const tileFileKey = (a: string): string | null => {
    const t = parseTileAssignment(a);
    if (!t) return null;
    // A preview keys the same as its editor: they are one file in two
    // views, which is what makes the switcher replace the tile in place
    // instead of opening a second one beside it.
    if (t.kind === "editor" || t.kind === "preview")
      return `${t.connectionId}:${t.arg}`;
    if (t.kind === "diff")
      return `${t.connectionId}:${parseDiffArg(t.arg).path}`;
    return null;
  };
  const sameTileFile = (a: string, b: string): boolean => {
    const ka = tileFileKey(a);
    return ka !== null && ka === tileFileKey(b);
  };
  let prevPaneAssignments: Record<string, string | null | undefined> = {};
  let prevActiveTile: string | null = null;
  let prevOpenTiles = new Set<string>();
  createEffect(() => {
    const la = layoutAssignments();
    const resolved = assignmentsResolved() && !pendingActiveTileRef();
    const next: Record<string, string | null | undefined> =
      la?.assignments ?? {};
    if (!resolved) return;
    const shown = new Set<string>();
    for (const v of Object.values(next)) {
      if (
        typeof v === "string" &&
        (isTileAssignment(v) || isWebAssignment(v))
      ) {
        shown.add(v);
      }
    }
    const at = activeTile();
    if (at && (isTileAssignment(at) || isWebAssignment(at))) shown.add(at);
    // In-place view switches are the one departure that closes a tab: the
    // switcher swapped which view of ONE file the pane holds, so the outgoing
    // view is not a second open tab, it is the same tab in a different shape.
    // Every other departure — displaced by a terminal, pane cleared, layout
    // torn down, the fullscreen slot dismissed — leaves the tab registered and
    // the dock picks it up.
    if (la) {
      for (const [paneId, prev] of Object.entries(prevPaneAssignments)) {
        if (typeof prev !== "string" || !isTileAssignment(prev)) continue;
        const now = next[paneId];
        if (
          typeof now === "string" &&
          now !== prev &&
          isTileAssignment(now) &&
          !shown.has(prev) &&
          sameTileFile(prev, now)
        ) {
          closeTab(prev);
        }
      }
    }
    // The non-BSP flavor of the same rule. Web panes have no file identity,
    // so they never match and are never closed implicitly.
    if (
      prevActiveTile &&
      at &&
      at !== prevActiveTile &&
      isTileAssignment(at) &&
      !shown.has(prevActiveTile) &&
      sameTileFile(prevActiveTile, at)
    ) {
      closeTab(prevActiveTile);
    }
    // Web panes are registered like every other tab. They used to be
    // skipped, on the belief that their URL rode in the hash — but the hash
    // writer emits `w:<conn>:<tabId>`, a *reference* to the KV record
    // (docs/design/kv.md). Skipping registration left that reference
    // dangling, so a web pane resolved to nothing and vanished on reload.
    for (const a of shown) {
      if (prevOpenTiles.has(a)) continue;
      recordLocalTab(a);
      registerTab(workspace, a);
    }
    prevPaneAssignments = { ...next };
    // Remember a web pane here too, or the rules above can never see one
    // leave the fullscreen slot.
    prevActiveTile =
      at && (isTileAssignment(at) || isWebAssignment(at)) ? at : null;
    prevOpenTiles = shown;
  });
  // "In BSP" means a genuine multi-pane layout. A single-leaf layout ("a") is
  // visually just one pane and is treated as non-BSP for tile purposes, so a
  // stale single-pane layout in the hash never forces the BSP tile path.
  const inBsp = createMemo(() => {
    const al = activeLayout();
    return al != null && leafCount(al.root) > 1;
  });

  // Re-parse layout from URL hash when the user edits it externally.
  // The app writes the hash via history.replaceState() which does NOT
  // trigger hashchange, so this only fires on genuine external edits.
  // Hash-only, both directions: a hash without `l=` clears the layout —
  // consulting stored state here re-applied a long-dismissed layout on
  // back/forward swipes and old-URL autocompletes.
  createEffect(() => {
    const onHashChange = () => {
      setDebugPanel(debugPanelOpenFromHash(location.hash));
      const fromHash = loadLayoutFromHash();
      if (fromHash && fromHash.dsl !== activeLayout()?.dsl) {
        // A layout arriving from outside hides the fullscreen slot, same as
        // applying one from the picker — hand the tile to the dock.
        setActiveTile(null);
        setActiveLayout(fromHash);
      } else if (!fromHash && activeLayout()) {
        exitBspLayout();
      }
    };
    window.addEventListener("hashchange", onHashChange);
    onCleanup(() => window.removeEventListener("hashchange", onHashChange));
  });

  // Clear focused surface if it was destroyed.  A grace period avoids
  // flickering during reconnect cycles where the surface list is temporarily
  // empty before being re-populated — but it only applies while the owning
  // connection is absent or mid-handshake.  Once the connection is ready its
  // surface list is authoritative, so a missing surface means it really is
  // gone and we clear immediately.  Mirrors reconcileAssignments'
  // `readyConnectionIds` gate, which is why BSP panes empty on the ack while
  // the main view used to sit on a dead surface for the full grace period.
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
    // Unknown connection id: we can't tell a destroy from a reconnect blip,
    // so keep the grace period.
    const connReady = fConnId != null && readyConnIds().has(fConnId);
    if (!exists && connReady) {
      if (clearFocusedTimer) {
        clearTimeout(clearFocusedTimer);
        clearFocusedTimer = null;
      }
      focusSurfaceById(null);
    } else if (!exists) {
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
    // A tile covers the main view (it is drawn ahead of the focused surface),
    // so the surface underneath is off-screen and belongs in the panel — the
    // same rule the sessions memo below applies to a displaced terminal.
    // Without this, tapping a tile's dock card hid the surface it covered from
    // everywhere at once: the tile is on top of it, and this filter dropped it
    // from the panel because focusedSurfaceId still named it. It came back
    // only by closing the tile. The slot is deliberately still *set* — that is
    // what brings the surface back when the tile closes — so what changes here
    // is only whether it is also offered as a card.
    const covered = activeTile() != null;
    const fid = covered ? null : focusedSurfaceId();
    const fConnId = covered ? null : focusedSurfaceConnId();
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

  /**
   * The session the user parked out of the main view, which then shows
   * nothing.
   *
   * UI-level state, because the core cannot express it: `focusSession(null)`
   * does not stick — `resolveFocusedSessionId` falls back to the connection's
   * focus and finally to the first live session, so *some* session is always
   * focused (which is what keeps focus alive across reconnects). Parking is a
   * statement about this view, not about which session holds focus.
   *
   * Holding the id rather than a flag is what keeps it honest: parking only
   * applies while that exact session is still the focused one, so anything
   * that moves focus — a new terminal, a dock card, the session closing —
   * un-parks by construction, with no clear-it-here call to forget.
   *
   * Declared here, above `offScreenSessions`: that memo reads it and Solid
   * runs a memo body eagerly at setup, so a later `const` is still in its
   * temporal dead zone when the first render reaches it.
   */
  const [parkedSessionId, setParkedSessionId] = createSignal<SessionId | null>(
    null,
  );
  const mainTerminalParked = () => {
    const fid = wsState().focusedSessionId;
    return fid != null && fid === parkedSessionId();
  };
  // Focus moving elsewhere ends the park outright, rather than leaving the id
  // set and merely inactive. Holding it would let the park resurrect: the core
  // always resolves *some* focus, so closing the session that displaced a
  // parked one hands focus back to it — and it would silently re-park, with
  // its dock card the only way out.
  createEffect(() => {
    const fid = wsState().focusedSessionId;
    if (untrack(parkedSessionId) != null && fid !== untrack(parkedSessionId)) {
      setParkedSessionId(null);
    }
  });
  /** The session the non-BSP main view displays: none while parked. */
  const mainViewSessionId = () =>
    mainTerminalParked() ? null : wsState().focusedSessionId;

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
    // When a surface or a tile is focused the terminal it displaced is
    // off-screen — as is the parked one, which is the whole point of
    // parking it.  focusedSessionId still points at that terminal, so
    // without this branch it would be filtered out below while nothing
    // renders it.
    if (
      focusedSurfaceId() != null ||
      activeTile() != null ||
      mainTerminalParked()
    ) {
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
  function persistCollapsed(next: Set<LeftPanel>) {
    writeStorage(LEFT_COLLAPSED_KEY, [...next].join(","));
  }
  function toggleLeftDock() {
    const next = !leftDockOpen();
    setLeftDockOpen(next);
    writeStorage(LEFT_DOCK_OPEN_KEY, next ? "1" : "0");
  }
  function toggleSectionCollapse(panel: LeftPanel) {
    const cur = collapsedSections();
    const next = toggleSection(
      panel,
      cur,
      inapplicableSections(),
      foldOverrides(),
    );
    setFoldOverrides(next.overridden);
    // A toggle moves exactly one panel, so an unchanged size means this click
    // went to the override instead of the preference.
    if (next.userCollapsed.size !== cur.size) {
      setCollapsedSections(next.userCollapsed);
      persistCollapsed(next.userCollapsed);
    }
  }
  // Keyboard entry point: open the dock and reveal a section.
  function focusSection(panel: LeftPanel) {
    if (!leftDockOpen()) toggleLeftDock();
    setCollapsedSections((cur) => {
      if (!cur.has(panel)) return cur;
      const next = new Set(cur);
      next.delete(panel);
      persistCollapsed(next);
      return next;
    });
  }
  function resizeSectionWeight(
    a: LeftPanel,
    b: LeftPanel,
    deltaWeight: number,
  ) {
    setSectionWeights((w) => ({
      ...w,
      [a]: Math.max(0.1, w[a] + deltaWeight),
      [b]: Math.max(0.1, w[b] - deltaWeight),
    }));
  }

  // Turn an absolute path into one relative to the active session's root, or
  // null when it isn't under that root.
  function relToActiveRoot(abs: string | null): string | null {
    const root = activeSession()?.root();
    if (!root || !abs) return null;
    if (abs === root) return "";
    if (abs.startsWith(`${root}/`)) return abs.slice(root.length + 1);
    return null;
  }

  // The file shown in the focused tile pane (editor or diff), as a root-rel
  // path — so the Explorer can highlight and reveal it. Commit tiles and
  // non-file panes yield null.
  const focusedTileFile = (): string | null => {
    const assign = inBsp()
      ? (layoutAssignments()?.assignments[bspFocusedPaneId() ?? ""] ?? null)
      : activeTile();
    if (!assign || typeof assign !== "string" || !isTileAssignment(assign))
      return null;
    const t = parseTileAssignment(assign);
    if (!t) return null;
    if (t.kind === "editor" || t.kind === "preview")
      return relToActiveRoot(t.arg);
    if (t.kind === "diff") return relToActiveRoot(parseDiffArg(t.arg).path);
    return null; // commit
  };

  // The terminal cwd as a root-rel directory, for the Explorer's follow-cd
  // highlight (null when the cwd is outside the active root).
  const cwdRelToRoot = (): string | null => {
    const f = focusedTerm();
    return f ? relToActiveRoot(f.cwd) : null;
  };

  // Reactive prop bag shared by every left-dock panel: they are pure views
  // over the one active IdeSession (getters keep them live).
  const leftPanelProps = {
    get session() {
      return activeSession();
    },
    get theme() {
      return theme();
    },
    get palette() {
      return palette();
    },
    get scale() {
      return chromeScale();
    },
    get fontFamily() {
      return resolvedFontWithFallback();
    },
    get fontSize() {
      return fontSize();
    },
    get activeFile() {
      return focusedTileFile();
    },
    get cwd() {
      return cwdRelToRoot();
    },
    onOpenTile: openTile,
  };

  // Re-root the dock at a worktree. The connection comes from the session
  // the list was read through, so navigating cannot silently land on another
  // server's path of the same name.
  function openWorktree(path: string) {
    const connectionId = activeSession()?.connectionId;
    if (!connectionId) return;
    const label = path.split("/").filter(Boolean).pop() ?? path;
    setRootSel({ kind: "worktree", connectionId, path, label });
  }

  function panelBody(panel: LeftPanel): JSX.Element {
    if (panel === "branches")
      return (
        <BranchesPanel
          {...leftPanelProps}
          onOpenWorktree={openWorktree}
          onOpenTerminalIn={(path) => void openTerminalIn(path)}
        />
      );
    if (panel === "log") return <LogPanel {...leftPanelProps} />;
    if (panel === "problems") return <ProblemsPanel {...leftPanelProps} />;
    return <ExplorerPanel {...leftPanelProps} />;
  }

  // The root the dock is showing: the focused terminal, or a declared
  // blit.roots entry. Sits at the top of the dock.
  function rootPickerHeader(): JSX.Element {
    const declared = () => roots().filter((r) => !r.disabled);
    // Label the "focused" option with the root actually being explored:
    // the session's resolved root (the repo workdir once git discovers
    // it), never the anchoring file or the terminal's live cwd. A `cd`
    // into a subdirectory expands the tree in place rather than
    // re-rooting it (see the cwd poll), so a cwd label would drift away
    // from the tree it sits above. Collapsed to a declared root's name
    // when the two name the same place.
    const focusedLabel = () => {
      const a = lastAnchor();
      if (!a) return "Focused pane";
      const s = rootSel().kind === "focused" ? activeSession() : null;
      const root = s?.root();
      const f = a.kind === "terminal" ? focusedTerm() : null;
      const path = root ?? (a.kind === "path" ? a.path : f?.cwd);
      const connectionId =
        (root ? s?.connectionId : null) ??
        (a.kind === "path" ? a.connectionId : f?.conn);
      if (!path || !connectionId) return "Focused pane";
      const match = declared().find(
        (r) =>
          r.path === path && connectionForRemote(r.remote) === connectionId,
      );
      return match ? match.name : `${connectionId}:${path}`;
    };
    const worktreeSel = () => {
      const s = rootSel();
      return s.kind === "worktree" ? s : null;
    };
    const value = () => {
      const s = rootSel();
      if (s.kind === "declared") return s.name;
      if (s.kind === "worktree") return "__worktree__";
      return "__focused__";
    };
    return (
      <div
        style={{
          display: "flex",
          "align-items": "center",
          gap: `${chromeScale().tightGap}px`,
          padding: `${chromeScale().controlY}px ${chromeScale().panelPadding}px`,
          "border-bottom": `1px solid ${theme().subtleBorder}`,
        }}
      >
        <select
          // NOT `value={value()}`: Solid compiles that to a render effect
          // tracking only `value()`, which runs *before* the `<Show>` below
          // has added the `__worktree__` option. The browser drops an
          // assignment naming an option that does not exist yet, and the
          // select silently falls back to the first one — so navigating to a
          // worktree changed the root but left the picker reading "Focused
          // pane". Re-assigning from an effect that reads the option set
          // explicitly runs after the children exist.
          ref={(el) => {
            createEffect(() => {
              worktreeSel();
              declared();
              el.value = value();
            });
          }}
          onChange={(e) => {
            const v = e.currentTarget.value;
            if (v === "__focused__") setRootSel({ kind: "focused" });
            // Re-picking the worktree we are already on is a no-op; without
            // this it would fall through and mint a declared root named
            // "__worktree__" that resolves to nothing.
            else if (v !== "__worktree__")
              setRootSel({ kind: "declared", name: v });
          }}
          title="Workspace root"
          style={{
            flex: 1,
            "min-width": 0,
            background: theme().panelBg,
            color: theme().fg,
            border: `1px solid ${theme().subtleBorder}`,
            "border-radius": "3px",
            padding: `1px ${chromeScale().tightGap}px`,
            "font-size": `${chromeScale().sm}px`,
            "font-family": resolvedFontWithFallback(),
          }}
        >
          <option value="__focused__">◐ {focusedLabel()}</option>
          {/* The worktree navigated to from the Branches panel. Only present
              while one is selected: it is a place you went, not a place you
              configured, so it does not accumulate in the list. */}
          <Show when={worktreeSel()}>
            {(sel) => <option value="__worktree__">⌥ {sel().label}</option>}
          </Show>
          <For each={declared()}>
            {(r) => <option value={r.name}>{r.name}</option>}
          </For>
        </select>
        <button
          onClick={() => toggleOverlay("roots")}
          title="Manage workspace roots"
          style={mergeStyle(ui.btn, {
            "flex-shrink": 0,
            "font-size": `${chromeScale().sm}px`,
            padding: `0 ${chromeScale().tightGap}px`,
            opacity: 0.7,
          })}
        >
          {"⚙"}
        </button>
      </div>
    );
  }
  // Open an IDE tile (editor/diff/commit).
  //
  //  - In a multi-pane BSP layout: REPLACE the focused pane (never split,
  //    never destroy the layout), queueing if BSPContainer isn't wired yet.
  //  - Otherwise (no layout, or a degenerate single-pane one): show the tile
  //    in place of the terminal via activeTile — do NOT swap into BSP. Drop any
  //    stale single-pane layout so the non-BSP view actually renders.
  // Which BSP pane an IDE tile should open into: the focused pane if it already
  // holds a tile (so switching views / navigating replaces in place), otherwise
  // an existing editor/diff/commit pane (so clicking a file while a *terminal*
  // is focused swaps the file pane, not the terminal), otherwise the focused
  // pane. moveToPane focuses the target, so the highlight follows.
  // Where a file/diff/commit opens: ALWAYS the focused pane. No preference
  // for existing editor panes, no empty-pane special case — what you open
  // lands where you are, tiling-WM style. A terminal occupying the pane is
  // simply replaced (it stays alive off-screen in the preview panel).
  function preferredTilePane(): string {
    return bspFocusedPaneId() ?? "0";
  }

  // ── Navigation history: browser-like back/forward per tile pane. openTile
  //    records the tile it replaces; navHistory walks the per-pane stacks.
  type NavStacks = { back: string[]; forward: string[] };
  const navHistory = new Map<string, NavStacks>();
  // The non-BSP activeTile slot, keyed in navHistory alongside real pane ids
  // (and used as a web-pane host id, same namespace). It cannot collide with a
  // pane: ids come from enumeratePanes (js/core/src/bsp/layout.ts) as
  // dot-joined child indices, so every real one matches /^\d+(\.\d+)*$/.
  const NAV_NONBSP = "non-bsp";
  const navFor = (key: string): NavStacks => {
    let h = navHistory.get(key);
    if (!h) {
      h = { back: [], forward: [] };
      navHistory.set(key, h);
    }
    return h;
  };
  const navKeyFor = (paneId: string | null): string =>
    inBsp() && paneId ? paneId : NAV_NONBSP;
  const currentTileIn = (key: string): string | null => {
    if (key === NAV_NONBSP) return activeTile();
    const v = layoutAssignments()?.assignments[key];
    return typeof v === "string" ? v : null;
  };
  // Push the pane's current tile onto its back stack before it's replaced by a
  // *different* tile (a fresh navigation clears the forward stack).
  const recordNav = (key: string, next: string) => {
    const cur = currentTileIn(key);
    if (!cur || cur === next || !isTileAssignment(cur)) return;
    const h = navFor(key);
    h.back.push(cur);
    h.forward.length = 0;
  };
  // Place a tile into a pane without recording history (a history move itself).
  // Nothing evicts it from the dock: the dock is derived as "open minus
  // displayed", so showing a tile drops it from there by construction.
  const placeTile = (assignment: string, paneId: string | null) => {
    if (navKeyFor(paneId) === NAV_NONBSP) {
      if (activeLayout()) {
        exitBspLayout();
        saveActiveLayout(null); // persist, or a remount resurrects it
      }
      setActiveTile(assignment);
    } else if (paneId) {
      if (moveToPaneFn) moveToPaneFn(assignment, paneId);
      else pendingTilePlacement = { assignment, paneId };
    }
  };
  function navigateHistory(dir: "back" | "forward") {
    const paneId = inBsp() ? bspFocusedPaneId() : null;
    const key = navKeyFor(paneId);
    const h = navFor(key);
    const from = dir === "back" ? h.back : h.forward;
    const to = dir === "back" ? h.forward : h.back;
    const target = from.pop();
    if (!target) return;
    const cur = currentTileIn(key);
    if (cur && isTileAssignment(cur) && cur !== target) to.push(cur);
    placeTile(target, paneId);
  }

  // ── Web panes ──

  const [webLocations, setWebLocations] = createSignal<WebLocation[]>([]);
  const [webUnavailable, setWebUnavailable] = createSignal<string | null>(null);
  // Which server a web pane resolves against; defaults to the active one and
  // is switchable in the picker, since a URL means different things per remote.
  const [webDest, setWebDest] = createSignal<string | null>(null);
  const webDestId = () => webDest() ?? activeConnectionId();
  // WebPane instances live in one persistent overlay and publish handles by
  // assignment, so moving a frame between a pane and the dock keeps the same
  // browsing context and navigation history.
  const webPaneHosts = createWebPaneHostRegistry();
  const [webHandles, setWebHandles] = createSignal<
    Record<string, WebPaneHandle>
  >({});
  const persistentWebAssignments = createMemo(() => {
    const assignments = new Set<string>();
    const active = activeTile();
    if (active && isWebAssignment(active)) assignments.add(active);
    for (const value of Object.values(layoutAssignments()?.assignments ?? {})) {
      if (typeof value === "string" && isWebAssignment(value)) {
        assignments.add(value);
      }
    }
    // Keep the dock's live-resource budget intact: older web cards remain
    // title-only and reload if restored, just like older editor cards.
    for (const value of backgroundTiles().slice(0, LIVE_DOCK_PREVIEWS)) {
      if (isWebAssignment(value)) assignments.add(value);
    }
    return Array.from(assignments);
  });

  /** Remembered locations live in the *server's* KV store, so each remote
   *  keeps its own set (docs/design/kv.md). `workspace.kv*` is per-connection,
   *  the same route the tab registry takes. */
  const webKv = (connectionId: string) => ({
    kvFetch: (key: string) => workspace.kvFetch(connectionId, key),
    kvPut: (key: string, value: Uint8Array) =>
      workspace.kvPut(connectionId, key, value),
  });

  async function refreshWebLocations() {
    const id = webDestId();
    if (!id) return;
    try {
      setWebLocations(await loadLocations(webKv(id)));
    } catch {
      // An older server without the kv family simply remembers nothing.
    }
  }

  function persistWebLocations(next: WebLocation[]) {
    setWebLocations(next);
    const id = webDestId();
    if (id) void saveLocations(webKv(id), next).catch(() => {});
  }

  /** Open a location as a pane, and remember it. */
  function openWebPane(url: string, connectionId?: string, paneId?: string) {
    const assignment = webAssignment(connectionId ?? activeConnectionId(), url);
    if (paneId) dropTileIntoPane(assignment, paneId);
    else openTile(assignment);
    persistWebLocations(withLocation(webLocations(), url, Date.now()));
  }

  /** How many panes currently hold content of one kind — counted the same way
   *  for BSP panes and the single non-BSP slot, so the status bar's tally does
   *  not depend on which mode you are in. */
  const paneKindCount = (
    matches: (value: string | null) => boolean,
  ): number => {
    if (inBsp()) {
      const assignments = layoutAssignments()?.assignments ?? {};
      return Object.values(assignments).filter((v) => matches(v)).length;
    }
    return matches(activeTile()) ? 1 : 0;
  };

  /** The focused pane's web handle, or null — what the status bar drives. */
  const focusedWebPane = (): {
    handle: WebPaneHandle;
    url: string;
    retarget: (url: string) => void;
  } | null => {
    const assign = inBsp()
      ? (layoutAssignments()?.assignments[bspFocusedPaneId() ?? ""] ?? null)
      : activeTile();
    const parsed = parseWebAssignment(assign);
    if (!parsed) return null;
    if (!assign) return null;
    // Only read when in BSP mode (retarget passes undefined otherwise), so the
    // non-BSP slot needs no name here.
    const paneId = bspFocusedPaneId() ?? "";
    const handle = webHandles()[assign];
    if (!handle) return null;
    return {
      handle,
      url: parsed.url,
      // A new origin is a new relayed target, so the pane is re-assigned
      // rather than navigated — and remembered, like any other open.
      retarget: (url: string) =>
        openWebPane(url, parsed.connectionId, inBsp() ? paneId : undefined),
    };
  };

  onMount(() => {
    if (!previewSupported()) {
      setWebUnavailable(
        "previews need a secure context (https, or http on localhost)",
      );
      return;
    }
    // Register early: a pane that renders before the worker is active fetches
    // straight past it and lands on the gateway's 503.
    void ensurePreviewWorker().then(setWebUnavailable);
    watchPreviewWorker();
  });

  createEffect(() => {
    // Locations follow the active server, and are re-read when the picker
    // opens: the first read can land before the connection is ready, and a
    // stale empty list reads as "nothing remembered".
    void webDestId();
    if (overlay() === "web" || overlay() === null) void refreshWebLocations();
  });

  function openTile(assignment: string) {
    if (inBsp()) {
      const paneId = preferredTilePane();
      recordNav(paneId, assignment);
      if (moveToPaneFn) moveToPaneFn(assignment, paneId);
      else pendingTilePlacement = { assignment, paneId };
      return;
    }
    recordNav(NAV_NONBSP, assignment);
    if (activeLayout()) {
      exitBspLayout();
      saveActiveLayout(null); // persist, or a remount resurrects it
    }
    setActiveTile(assignment);
  }

  // Drop a dragged pane assignment into a specific BSP pane (records nav
  // history there). Any assignment the panel can hold: an IDE/web tile, or a
  // parked terminal/surface. recordNav is a no-op for the latter — it only
  // pushes when the assignment being *replaced* is a tile, which is what makes
  // Back return to a tile a dropped terminal displaced.
  function dropTileIntoPane(
    assignment: string,
    paneId: string,
    sourcePaneId?: string,
  ) {
    recordNav(paneId, assignment);
    if (moveToPaneFn) moveToPaneFn(assignment, paneId, sourcePaneId);
    else pendingTilePlacement = { assignment, paneId };
  }

  /**
   * Show an assignment of any kind, wherever "here" currently is — the focused
   * BSP pane, or the single main view. This is the one entry point that does
   * not care what it is holding: it dispatches on the assignment kind, because
   * each has its own slot (activeTile for a tile, the focused surface for a
   * surface, the focused session for a terminal), and each of the three
   * functions below already knows how to place itself in a pane as well.
   * All three dismiss the other two slots, so the modes can't overlap.
   *
   * Used by both drags that land on the main view and Alt+Shift+[ / ].
   */
  function focusAssignment(assignment: string) {
    const surface = parseSurfaceAssignment(assignment);
    if (surface) {
      focusSurface(surface.surfaceId, surface.connectionId);
      return;
    }
    if (isTileAssignment(assignment) || isWebAssignment(assignment)) {
      openTile(assignment);
      return;
    }
    // Everything else in the assignment namespace is a bare session id.
    switchSession(assignment as SessionId);
  }
  /**
   * What the slot those chords act on is showing right now: the focused BSP
   * pane's occupant, or — with no layout — whichever of the three single-view
   * slots is in use. Null when it holds nothing (a parked view), which makes
   * the next cycle step enter the ring at its near end instead of skipping one.
   */
  function focusedAssignment(): string | null {
    const paneId = bspFocusedPaneId();
    if (activeLayout() && paneId) {
      return layoutAssignments()?.assignments[paneId] ?? null;
    }
    const tile = activeTile();
    if (tile) return tile;
    const surfaceId = focusedSurfaceId();
    if (surfaceId != null) {
      const connId =
        focusedSurfaceConnId() ??
        surfaces().find((s) => s.surfaceId === surfaceId)?.connectionId;
      if (connId) return surfaceAssignment(connId, surfaceId);
    }
    // Not wsState().focusedSessionId: the core always keeps *some* session
    // focused, so only the main view's own slot can say "nothing here".
    return mainViewSessionId();
  }

  /**
   * The first Ctrl-K belongs to Blit's switcher. Repeating it dismisses the
   * switcher and sends the chord to the pane that was underneath it.
   */
  function forwardCtrlKToFocusedPane() {
    const assignment = focusedAssignment();
    if (!assignment) return;
    const surface = parseSurfaceAssignment(assignment);
    if (surface) {
      const conn = workspace.getConnection(surface.connectionId);
      if (!conn) return;
      // Linux evdev: KEY_LEFTCTRL=29, KEY_K=37. BlitSurfaceCanvas uses the
      // same codes for physical keyboard input.
      conn.sendSurfaceInput(surface.surfaceId, 29, true);
      conn.sendSurfaceInput(surface.surfaceId, 37, true);
      conn.sendSurfaceInput(surface.surfaceId, 37, false);
      conn.sendSurfaceInput(surface.surfaceId, 29, false);
      return;
    }
    // Editors and web panes own no PTY input channel. Do not send the chord to
    // a stale focused terminal behind one of those tiles.
    if (isTileAssignment(assignment) || isWebAssignment(assignment)) return;
    workspace.sendInput(assignment as SessionId, new Uint8Array([0x0b]));
  }
  /** Highlight shown while a drag hovers the non-BSP main view (BSP panes draw
   *  their own, per pane). */
  const [mainViewDragOver, setMainViewDragOver] = createSignal(false);
  /** Reveals the main view's ✕ on pointer devices (see PaneTools). */
  const [mainViewHover, setMainViewHover] = createSignal(false);

  /** True while a pane's own content is being dragged (its grip). The dock
   *  reveals itself as a drop-to-park target for exactly this window — it is
   *  hidden when nothing is parked, which is precisely when a drag most needs
   *  it. Depth-counted: dragenter/dragleave fire per element crossed. */
  const [paneDragActive, setPaneDragActive] = createSignal(false);

  /**
   * Whether the preview panel is on screen — and so whether its thumbnails
   * exist to be watched.
   *
   * Both the panel's own `<Show>` and the parked sessions' stream
   * subscriptions read this, because they have to agree: a parked pty whose
   * thumbnail is not rendered would otherwise keep pushing `S2C_UPDATE`
   * frames at a panel nobody can see. A grip drag reveals the panel even when
   * it is empty or toggled off — it is the drop-to-park target, and "nothing
   * parked yet" is exactly when a drag needs somewhere to park.
   */
  const previewPanelVisible = () =>
    paneDragActive() ||
    (previewPanelOpen() &&
      (offScreenSessions().length > 0 ||
        offScreenSurfaces().length > 0 ||
        backgroundTiles().length > 0));
  let paneDragDepth = 0;
  const paneDragEnter = (e: DragEvent) => {
    if (!isPaneDrag(e)) return;
    paneDragDepth++;
    setPaneDragActive(true);
  };
  const paneDragLeave = (e: DragEvent) => {
    if (!isPaneDrag(e)) return;
    if (--paneDragDepth <= 0) {
      paneDragDepth = 0;
      setPaneDragActive(false);
    }
  };
  // `dragend` fires on the source — always ours here — and `drop` anywhere;
  // either way the window is over, whatever the enter/leave count says.
  const paneDragDone = () => {
    paneDragDepth = 0;
    setPaneDragActive(false);
  };
  window.addEventListener("dragenter", paneDragEnter);
  window.addEventListener("dragleave", paneDragLeave);
  window.addEventListener("drop", paneDragDone);
  window.addEventListener("dragend", paneDragDone);
  onCleanup(() => {
    window.removeEventListener("dragenter", paneDragEnter);
    window.removeEventListener("dragleave", paneDragLeave);
    window.removeEventListener("drop", paneDragDone);
    window.removeEventListener("dragend", paneDragDone);
  });

  /** What the main view's grip drags: its tile, its surface, or its
   *  terminal. Parking the focused session is focusing nothing — the view
   *  falls back to EmptyPane and the session joins the dock, which derives
   *  "parked" from "not displayed". Every pane kind carries the grip: it is
   *  also what relocates the toolbar, so a grip-less pane would have a close
   *  button locked in place over whatever it covers. */
  const mainViewDragAssignment = (): string | null => {
    const tile = activeTile();
    if (tile) return tile;
    const sid = focusedSurfaceId();
    const connId = focusedSurfaceConnId();
    if (sid != null && connId != null) return surfaceAssignment(connId, sid);
    return mainViewSessionId() ?? null;
  };

  /** Keep the core's focused session in the dock when the standalone view's
   * foreground assignment is removed. Surface and tile focus sit in UI-only
   * slots above that session, so merely clearing either slot would otherwise
   * expose the terminal as an unwanted second backgrounding step. */
  function parkMainViewSession() {
    const fid = wsState().focusedSessionId;
    if (fid != null) setParkedSessionId(fid);
  }

  /** A grip drag landed on the dock: park the content by taking it off
   *  screen — the dock lists exactly what is open but not displayed. */
  function parkDraggedAssignment(assignment: string, source: string) {
    if (source === MAIN_PANE_SOURCE) {
      if (assignment === activeTile()) {
        parkMainViewSession();
        setActiveTile(null);
        return;
      }
      const surface = parseSurfaceAssignment(assignment);
      if (surface && surface.surfaceId === focusedSurfaceId()) {
        parkMainViewSession();
        focusSurfaceById(null);
        return;
      }
      if (assignment === wsState().focusedSessionId) {
        setParkedSessionId(assignment as SessionId);
      }
      return;
    }
    // A BSP pane: empty it, if it still holds what the drag carried — a
    // layout change mid-drag must not evict a bystander.
    if (layoutAssignments()?.assignments[source] === assignment) {
      clearPaneAssignmentFn?.(source);
    }
  }

  // Send the currently-focused IDE or web tile to the dock (Ctrl+Shift+Q).
  // Handles both the non-BSP focused tile and a tile occupying the focused BSP
  // pane. Returns true if a tile was backgrounded (so the keyboard handler
  // knows it consumed the key). Stopping displaying it IS backgrounding it —
  // the tab stays registered, and the derived dock picks it up.
  function backgroundFocusedTile(): boolean {
    if (activeTile()) {
      parkMainViewSession();
      setActiveTile(null);
      return true;
    }
    const paneId = bspFocusedPaneId();
    if (activeLayout() && paneId) {
      const assign = layoutAssignments()?.assignments[paneId] ?? null;
      if (assign && (isTileAssignment(assign) || isWebAssignment(assign))) {
        clearPaneAssignmentFn?.(paneId);
        return true;
      }
    }
    return false;
  }

  /**
   * Close the focused tile outright — the Ctrl+Alt+Shift+Q counterpart to
   * {@link backgroundFocusedTile}'s Ctrl+Shift+Q. Same targets (a non-BSP
   * active tile, or an IDE/web tile in the focused BSP pane), but the tab is
   * closed rather than merely stopped being displayed, matching what the same
   * chord does to a terminal or a surface. Closing is host-wide now: the
   * registry record goes, so the tab leaves every frontend's dock.
   */
  function closeFocusedTile(): boolean {
    const tile = activeTile();
    if (tile) {
      setActiveTile(null);
      closeTab(tile);
      return true;
    }
    const paneId = bspFocusedPaneId();
    if (activeLayout() && paneId) {
      const assign = layoutAssignments()?.assignments[paneId] ?? null;
      if (assign && (isTileAssignment(assign) || isWebAssignment(assign))) {
        clearPaneAssignmentFn?.(paneId);
        closeTab(assign);
        return true;
      }
    }
    return false;
  }

  /**
   * The non-BSP counterpart to BSPContainer's per-pane ✕: close whatever the
   * single main view is showing. Same cascade as Ctrl+Alt+Shift+Q, minus the
   * BSP-pane surface branch that can't apply here.
   */
  function closeFocusedPane() {
    if (closeFocusedTile()) return;
    const sid = focusedSurfaceId();
    const sConnId = focusedSurfaceConnId();
    if (sid != null && sConnId != null) {
      workspace.closeSurface(sConnId, sid);
      return;
    }
    const fid = wsState().focusedSessionId;
    if (fid) void workspace.closeSession(fid);
  }

  /** True when the main view is holding something the ✕ can close. A parked
   *  view holds nothing, so it gets no toolbar. */
  const mainViewClosable = () =>
    !!activeTile() || focusedSurfaceId() != null || !!mainViewSessionId();

  // Restore a backgrounded tile: showing it removes it from the dock, which is
  // derived as "open minus displayed".
  function restoreTile(assignment: string) {
    openTile(assignment);
  }
  // The ✕ on a background-editor card. Closes the tab host-wide (it is an
  // explicit close, the same as Ctrl+Alt+Shift+Q on a displayed one), so its
  // live dock tile unmounts here — fs-sync/LSP torn down — and it leaves the
  // other frontends' docks too.
  function closeBackgroundTile(assignment: string) {
    closeTab(assignment);
  }

  // The signal updates per pointermove (live layout); the storage write —
  // localStorage plus the config websocket — lands once, on a trailing
  // debounce, instead of per move.
  const persistTimers = new Map<string, ReturnType<typeof setTimeout>>();
  function writeStorageDebounced(key: string, value: string) {
    const prev = persistTimers.get(key);
    if (prev !== undefined) clearTimeout(prev);
    persistTimers.set(
      key,
      setTimeout(() => {
        persistTimers.delete(key);
        writeStorage(key, value);
      }, 250),
    );
  }
  onCleanup(() => {
    for (const t of persistTimers.values()) clearTimeout(t);
  });
  function persistLeftDockWidth(w: number) {
    setLeftDockWidth(w);
    writeStorageDebounced(LEFT_DOCK_WIDTH_KEY, String(w));
  }
  function persistPreviewPanelWidth(w: number) {
    setPreviewPanelWidth(w);
    writeStorageDebounced(PREVIEW_PANEL_WIDTH_KEY, String(w));
  }

  let paletteOverlayOrigin: TerminalPalette | null = null;
  let fontOverlayOrigin: {
    family: string;
    size: number;
    gamma: number;
  } | null = null;

  const remotePaletteId = useConfigValue(PALETTE_KEY);
  const remoteFont = useConfigValue(FONT_KEY);
  const remoteFontSize = useConfigValue(FONT_SIZE_KEY);
  const remoteTextGamma = useConfigValue(TEXT_GAMMA_KEY);
  // No media settings here on purpose — bitrate, mute, encoder effort,
  // streaming, frame rate and zoom are device-local (see storage.ts), so they
  // are read from localStorage once at startup and never taken from another
  // device.

  // `preferred*()` already read the URL when these signals were created, so a
  // pinned key must not be followed here: the synced value lands a beat later
  // (the cached read fires this effect on mount, the socket again on connect)
  // and would silently overwrite what the link asked for. Skipping the whole
  // effect, rather than the first run, is what makes `?fontSize=` survive
  // another device changing the size mid-session.
  const pinnedByUrl = urlPinnedKeys();
  /** Track a preference synced from the account, unless this URL owns it. */
  const followConfig = (
    key: string,
    remote: () => string | null,
    apply: (raw: string) => void,
  ) => {
    if (pinnedByUrl.has(key)) return;
    createEffect(() => {
      const raw = remote();
      if (raw) apply(raw);
    });
  };

  followConfig(PALETTE_KEY, remotePaletteId, (id) => {
    const p = PALETTES.find((x) => x.id === id);
    if (p) setPalette(p);
  });

  followConfig(FONT_KEY, remoteFont, (f) => {
    if (f.trim()) setFont(f.trim());
  });

  followConfig(FONT_SIZE_KEY, remoteFontSize, (s) => {
    const n = parseInt(s, 10);
    if (n > 0) setFontSize(n);
  });

  followConfig(TEXT_GAMMA_KEY, remoteTextGamma, (s) => {
    const n = Number(s);
    if (Number.isFinite(n) && n >= 0.5 && n <= 2.5) setTextGamma(n);
  });

  // Sync media preferences to all connections so new subscribes use them.
  createEffect(() => {
    const bandwidth = videoBandwidth();
    const speed = videoSpeed();
    const b = audioBitrate();
    const streaming = surfaceStreaming();
    const smoothing = surfaceSmoothing();
    const maxFps = surfaceMaxFps();
    for (const snap of allConnections()) {
      const conn = workspace.getConnection(snap.id);
      if (conn) {
        conn.defaultSurfaceBandwidth = bandwidth;
        conn.defaultSurfaceSpeed = speed;
        conn.defaultAudioBitrateKbps = b;
        conn.surfaceStreamingEnabled = streaming;
        conn.surfaceStore.setPresentationSmoothingEnabled(smoothing);
        conn.setSurfaceMaxFpsCap(maxFps);
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
    const base = defaultFont();
    return rf === base ? rf : `${rf}, ${base}`;
  };

  // Overlays portal to <body> (Overlay.tsx) to escape <main>'s keyboard-pin
  // transform; give them the font they used to inherit from <main>.
  createEffect(() => {
    document.body.style.fontFamily = resolvedFontWithFallback();
  });

  onMount(loadServerFonts);

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
    // Parked terminals are watched only while their thumbnails are rendered.
    if (previewPanelVisible()) {
      for (const s of offScreenSessions()) desired.add(s.id);
    }
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
    if (
      phase === "pending" &&
      overlay() === null &&
      remotes().length > 0 &&
      shellCapabilities().remotes
    ) {
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

  // Uniform themed scrollbars: a single global rule covering every scrollable
  // element, so nothing falls back to the chunky native bar (containers that
  // forget to spread scrollbarStyle, CodeMirror, xterm, …). Recoloured with the
  // palette.
  createEffect(() => {
    const t = theme();
    const id = "blit-scrollbars";
    let el = document.getElementById(id) as HTMLStyleElement | null;
    if (!el) {
      el = document.createElement("style");
      el.id = id;
      document.head.appendChild(el);
    }
    el.textContent = `
      * { scrollbar-width: thin; scrollbar-color: ${t.border} transparent; }
      *::-webkit-scrollbar { width: 10px; height: 10px; }
      *::-webkit-scrollbar-track { background: transparent; }
      *::-webkit-scrollbar-thumb {
        background: ${t.border};
        border-radius: 6px;
        border: 2px solid transparent;
        background-clip: padding-box;
      }
      *::-webkit-scrollbar-thumb:hover {
        background: ${t.dimFg};
        background-clip: padding-box;
      }
      *::-webkit-scrollbar-corner { background: transparent; }
    `;
  });
  onCleanup(() => document.getElementById("blit-scrollbars")?.remove());

  // The highlight an xdg_activation_v1 request buys instead of the view: red,
  // fading out over the window. One colour and one direction — a two-colour
  // bounce read as a state change rather than a nudge, and it had to be
  // explained. Nothing here moves, so there is no reduced-motion variant to
  // offer either. Global and themed like the scrollbars above, because the same
  // fade has to land on two very different things — a dock card's header bar
  // and a pane-sized ring — and keyframes cannot be inline styles.
  createEffect(() => {
    const t = theme();
    const id = "blit-attention";
    let el = document.getElementById(id) as HTMLStyleElement | null;
    if (!el) {
      el = document.createElement("style");
      el.id = id;
      document.head.appendChild(el);
    }
    el.textContent = `
      @keyframes blit-attention-fill {
        0%   { background-color: ${t.errorText}; }
        100% { background-color: transparent; }
      }
      @keyframes blit-attention-ring {
        0%   { border-color: ${t.errorText}; }
        100% { border-color: transparent; }
      }
      [data-blit-attention="fill"] {
        animation: blit-attention-fill ${ATTENTION_MS}ms ease-out 1;
      }
      [data-blit-attention="ring"] {
        animation: blit-attention-ring ${ATTENTION_MS}ms ease-out 1;
      }
    `;
  });
  onCleanup(() => document.getElementById("blit-attention")?.remove());

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
    //
    // Outside BSP the same leak happens when a surface is focused:
    // focusedSessionId still points at the terminal that was showing
    // before the surface took over, so terminal title updates would
    // bleed into document.title.  Suppress the session branch when a
    // surface is focused.
    const al = activeLayout();
    const bspHasSession =
      al != null &&
      (() => {
        const pid = bspFocusedPaneId();
        if (!pid) return false;
        const assignment = layoutAssignments()?.assignments[pid] ?? null;
        return assignment != null && !isSurfaceAssignment(assignment);
      })();
    const sessionFocused = al
      ? bspHasSession
      : focusedSurfaceId() == null && !mainTerminalParked();
    const fs = sessionFocused ? focusedSession() : null;
    if (fs) {
      if (fs.title) parts.push(truncateDocumentEntityTitle(fs.title));
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
        if (name) parts.push(truncateDocumentEntityTitle(name));
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
    const sid = mainViewSessionId();
    const surfId = focusedSurfaceId();
    if (!sid && surfId == null) return; // nothing to focus
    // Defer until Solid commits the DOM update.
    setTimeout(() => {
      // A switcher action may have moved focus somewhere deliberate by now
      // (the search panel's input) — don't yank it back to the terminal.
      const active = document.activeElement;
      if (
        active instanceof HTMLElement &&
        active.closest("[data-blit-search-pane]")
      )
        return;
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
    // Dismissing the link dialog by any route — button, backdrop, Escape —
    // means "do not open". Clearing here keeps that true for all of them.
    setPendingLink(null);
    setOverlay(null);
    const el = previousFocus;
    previousFocus = null;
    if (el instanceof HTMLElement) setTimeout(() => el.focus(), 0);
  }

  /**
   * Bind hyperlink hover and activation to a terminal surface as it mounts.
   *
   * Applied to *every* surface, not just the focused one: hovering follows the
   * pointer, so a link in an unfocused split must still preview and open. The
   * WeakSet guards against re-binding a surface that a re-render hands back,
   * and unbinding is left to surface disposal — the listeners live on the
   * surface itself, so they die with it.
   */
  const linkBoundSurfaces = new WeakSet<BlitTerminalSurface>();
  function bindTerminalLinks(surface: BlitTerminalSurface | null) {
    if (!surface || linkBoundSurfaces.has(surface)) return;
    linkBoundSurfaces.add(surface);

    surface.onLinkHover(setHoveredLink);
    // Replaces core's blocking window.confirm with the in-app dialog. The
    // verdict still decides: `allow` opens, anything else asks, and the
    // overlay offers no way to proceed on `deny`.
    surface.setLinkActivateHandler((assessment) => {
      if (assessment.verdict === "allow") {
        window.open(assessment.raw, "_blank", "noopener,noreferrer");
        return;
      }
      setPendingLink({ assessment, text: hoveredLink()?.text ?? "" });
      previousFocus = document.activeElement as HTMLElement | null;
      setOverlay("link");
    });
  }

  function restoreOverlayPreview(target: Overlay) {
    if (target === "palette" && paletteOverlayOrigin) {
      setPalette(paletteOverlayOrigin);
      paletteOverlayOrigin = null;
    } else if (target === "font" && fontOverlayOrigin) {
      setFont(fontOverlayOrigin.family);
      setFontSize(fontOverlayOrigin.size);
      setTextGamma(fontOverlayOrigin.gamma);
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
      fontOverlayOrigin = {
        family: font(),
        size: fontSize(),
        gamma: textGamma(),
      };
      loadServerFonts();
    }
    setOverlay(target);
  }

  function changePalette(nextPalette: TerminalPalette) {
    setPalette(nextPalette);
    paletteOverlayOrigin = null;
    writeStorage(PALETTE_KEY, nextPalette.id);
    closeOverlay();
  }

  function changeFont(family: string, size: number, gamma: number) {
    const value = family.trim() || defaultFont();
    setFont(value);
    setFontSize(size);
    setTextGamma(gamma);
    fontOverlayOrigin = null;
    writeStorage(FONT_KEY, value);
    writeStorage(FONT_SIZE_KEY, String(size));
    writeStorage(TEXT_GAMMA_KEY, String(gamma));
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

  function changeVideoBandwidth(bandwidth: number) {
    setVideoBandwidth(bandwidth);
    writeStorage(VIDEO_BANDWIDTH_KEY, String(bandwidth));
    applyVideoEncoding();
  }

  function changeVideoSpeed(speed: number) {
    setVideoSpeed(speed);
    writeStorage(VIDEO_SPEED_KEY, String(speed));
    applyVideoEncoding();
  }

  /** Push the current bandwidth/speed pair to every live subscription. */
  function applyVideoEncoding() {
    const bandwidth = videoBandwidth();
    const speed = videoSpeed();
    for (const snap of allConnections()) {
      const conn = workspace.getConnection(snap.id);
      if (!conn) continue;
      conn.defaultSurfaceBandwidth = bandwidth;
      conn.defaultSurfaceSpeed = speed;
      for (const surface of conn.surfaceStore.getSurfaces().values()) {
        conn.sendSurfaceResubscribe(surface.surfaceId, bandwidth, speed);
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

  function changeSurfaceSmoothing(enabled: boolean) {
    setSurfaceSmoothing(enabled);
    writeStorage(SURFACE_SMOOTHING_KEY, enabled ? "1" : "0");
    for (const snap of allConnections()) {
      workspace
        .getConnection(snap.id)
        ?.surfaceStore.setPresentationSmoothingEnabled(enabled);
    }
  }

  function changeSurfaceMaxFps(maxFps: number) {
    setSurfaceMaxFps(maxFps);
    writeStorage(SURFACE_MAX_FPS_KEY, String(maxFps));
  }

  /** Narrow which codecs this device accepts for surface video, then make
   *  every live stream honour it: the mask rides C2S_CLIENT_FEATURES, and the
   *  server only reconsiders its encoder on a resubscribe. */
  function changeSurfaceCodecs(mask: number) {
    setSurfaceCodecs(mask);
    writeStorage(SURFACE_CODECS_KEY, String(mask));
    setAllowedCodecSupport(mask);
    for (const snap of allConnections()) {
      workspace.getConnection(snap.id)?.refreshCodecSupport();
    }
  }

  /** Every resizable surface view re-derives the scale it asks the compositor
   *  for, so there is nothing to push to the connections here. */
  function changeSurfaceZoom(percent: number) {
    const clamped = Math.min(
      MAX_SURFACE_ZOOM,
      Math.max(MIN_SURFACE_ZOOM, Math.round(percent)),
    );
    setSurfaceZoom(clamped);
    writeStorage(SURFACE_ZOOM_KEY, String(clamped));
  }

  function changeSurfaceZoomMode(mode: SurfaceZoomMode) {
    setSurfaceZoomMode(mode);
    writeStorage(SURFACE_ZOOM_MODE_KEY, mode);
  }

  function changeSurfaceTouchMode(mode: SurfaceTouchMode) {
    setSurfaceTouchMode(mode);
    writeStorage(SURFACE_TOUCH_MODE_KEY, mode);
  }

  function changeWaylandKeyboardRequests(enabled: boolean) {
    setWaylandKeyboardRequests(enabled);
    writeStorage(WAYLAND_KEYBOARD_REQUESTS_KEY, enabled ? "1" : "0");
    if (enabled || keyboardManualOverride) return;
    const input = automaticKeyboardInput;
    automaticKeyboardInput = null;
    if (!input) return;
    setKeyboardWanted(false);
    if (document.activeElement === input) input.blur();
  }

  let focusBySessionFn: ((sessionId: SessionId) => void) | null = null;
  let moveSessionToPaneFn:
    | ((sessionId: SessionId, targetPaneId: string) => void)
    | null = null;
  let moveToPaneFn:
    | ((value: string, targetPaneId: string, fromPaneId?: string) => void)
    | null = null;
  // A tile to drop into a freshly-created layout, flushed when BSPContainer
  // wires moveToPane on mount (no-layout file open).
  let pendingTilePlacement: { assignment: string; paneId: string } | null =
    null;
  let clearPaneAssignmentFn: ((paneId: string) => void) | null = null;
  let focusPaneFn: ((paneId: string) => void) | null = null;
  // Drop every BSPContainer control-fn reference. These close over a specific
  // BSPContainer instance; when the container unmounts that instance is
  // disposed, so the stale fns must be cleared or a later call would write into
  // a dead instance (the tile lands nowhere and never renders). The next
  // container re-wires them via its onMoveToPane/etc. effects on mount.
  function clearBspControlFns() {
    focusBySessionFn = null;
    moveSessionToPaneFn = null;
    moveToPaneFn = null;
    clearPaneAssignmentFn = null;
    focusPaneFn = null;
  }
  // Invariant: the control fns are valid exactly while a BSPContainer is
  // mounted, which is exactly while inBsp() is true (it mounts under
  // `inBsp() && activeLayout()`). Whenever we're not in BSP, clear them so a
  // dangling reference to a disposed container can never be called — covers
  // every teardown path (open-tile, clear-layout, multi→single collapse).
  createEffect(() => {
    if (!inBsp()) clearBspControlFns();
  });
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

  /** Leave BSP without dropping the focused surface back into the preview
   *  panel. Terminal focus already lives in BlitWorkspace, but surface focus
   *  is derived from the BSP assignment while the container is mounted. Move
   *  it into the non-BSP focus slot before clearing assignments so the new
   *  foreground view mounts, offers its full size, and survives the old BSP
   *  view's resize withdrawal. */
  function exitBspLayout() {
    if (inBsp()) {
      const surface = bspFocusedSurface();
      // Only when the focused pane actually holds a surface. Clearing the slot
      // in the `else` would discard whatever surface the user had focused
      // *before* entering BSP — so leaving a layout of terminals demoted that
      // surface to the preview panel, the opposite of the point of this.
      if (surface) {
        setActiveTile(null);
        focusSurfaceById(surface.surfaceId, surface.connectionId);
      }
    }
    setLayoutAssignments(null);
    setActiveLayout(null);
  }

  /** The mirror of exitBspLayout. Under a layout the panes own what is on
   *  screen, so the single-view surface slot describes nothing — and entering
   *  BSP never placed the focused surface in a pane, it simply stopped
   *  rendering it. Leaving the slot set kept a phantom `s=` in the URL and
   *  handed every consumer of "is a surface focused" the wrong answer. */
  createEffect(() => {
    if (inBsp()) focusSurfaceById(null);
  });

  function switchSession(sessionId: SessionId) {
    focusSessionFromUi(sessionId);
    previousFocus = null;
    closeOverlay();
  }

  function focusSessionFromUi(sessionId: SessionId) {
    // Re-showing the parked session itself: focus does not change, so only an
    // explicit clear can un-park it.
    if (sessionId === parkedSessionId()) setParkedSessionId(null);
    focusSurfaceById(null);
    // Stops DISPLAYING the non-BSP tile; the tab stays open and drops into
    // the dock (and stays listed in every other frontend).
    setActiveTile(null);
    if (activeLayout()) {
      focusBySessionFn?.(sessionId);
    }
    workspace.focusSession(sessionId);
  }

  function focusSurface(surfaceId: number, connectionId?: ConnectionId) {
    setActiveTile(null); // stops displaying the non-BSP tile; tab stays open
    // When a BSP layout is active, place the surface into the focused pane.
    if (activeLayout() && bspFocusedPaneId()) {
      const connId =
        connectionId ??
        surfaces().find((x) => x.surfaceId === surfaceId)?.connectionId ??
        activeConnectionId();
      const assignment = surfaceAssignment(connId, surfaceId);
      // Already displayed in some pane? Focus that pane instead of moving it.
      // moveToPane would *swap*: assignmentsAfterDrop recovers a surface's
      // source pane from the current assignments (surfaces are unique views),
      // so the focused pane's occupant would take the vacated one. That is
      // right for a drag, and wrong for every caller here — none of them is a
      // drag. xdg_activation is the loud case: dropping a link from Slack onto
      // Brave makes Brave raise itself while Slack's pane still holds focus,
      // and the two panes traded places. Terminals never had the bug because
      // focusBySession checks this first; surfaces now match.
      const shown = Object.entries(layoutAssignments()?.assignments ?? {}).find(
        ([, value]) => value === assignment,
      )?.[0];
      if (shown) focusPaneFn?.(shown);
      else moveToPaneFn?.(assignment, bspFocusedPaneId()!);
      focusSurfaceById(null);
    } else {
      focusSurfaceById(surfaceId, connectionId);
    }
    // Null first: closeOverlay restores previousFocus on a timeout, which
    // would steal focus back from the surface — see selectPane.
    previousFocus = null;
    closeOverlay();
  }

  /**
   * A Wayland client asked for its own toplevel (xdg_activation_v1 — an
   * Electron app reacting to a notification click). It is answered with a
   * highlight where the surface already is, and nothing else: the view is the
   * user's, and an app that wants it can only ask to be looked at.
   *
   * Raising instead is what made the dock unusable next to a talkative client.
   * Tokens are cheap and their delivery unacknowledged, so a client repeats the
   * request several times a second, and each repeat landed after whatever the
   * user had just picked — their choice appearing for an instant and being
   * dragged back off, with repeated clicking working only when one fell in a
   * gap. Under a layout it was worse: each repeat re-focused a pane out from
   * under them. See ./surfaceAttention.ts.
   */
  function activateSurface(surfaceId: number, connectionId: ConnectionId) {
    // Already on top: the user is looking straight at it, so lighting it up
    // would be noise rather than news.
    //
    // "On top" is a different slot in each mode: focusedSurfaceId is the
    // non-BSP main view, which is left null under a layout, so testing only
    // that would leave this dead in BSP. There the equivalent question is
    // whether the surface already occupies the focused pane.
    if (inBsp()) {
      const focused = bspFocusedSurface();
      if (
        focused?.surfaceId === surfaceId &&
        focused?.connectionId === connectionId
      ) {
        return;
      }
    } else if (
      focusedSurfaceId() === surfaceId &&
      focusedSurfaceConnId() === connectionId
    ) {
      return;
    }
    flashAttention(surfaceAssignment(connectionId, surfaceId));
  }

  let termHandle: { rows: number; cols: number; focus: () => void } | null =
    null;

  async function createAndFocus(command?: string, connectionId?: string) {
    // `[remote>][command]` doubles as a location bar: an entry with a scheme
    // or a port is a web pane, not a program (see looksLikeWebLocation).
    if (command && looksLikeWebLocation(command)) {
      openWebPane(command, connectionId);
      closeOverlay();
      return;
    }
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
      setActiveTile(null); // stops displaying the non-BSP tile; tab stays open
      workspace.focusSession(session.id);
      previousFocus = null;
      closeOverlay();
    } catch {}
  }

  /** Open a terminal in an absolute directory on the session's own
   *  connection — the Branches panel's secondary action on a worktree. Takes
   *  the focused pane like any other new terminal, so it lands where you are
   *  looking rather than somewhere you have to go find. */
  async function openTerminalIn(path: string) {
    const connectionId = activeSession()?.connectionId;
    if (!connectionId) return;
    try {
      const session = await workspace.createSession({
        connectionId,
        rows: termHandle?.rows ?? 24,
        cols: termHandle?.cols ?? 80,
        cwd: path,
      });
      focusSurfaceById(null);
      setActiveTile(null);
      moveSessionToPaneFn?.(session.id, preferredTilePane());
      workspace.focusSession(session.id);
    } catch {}
  }

  async function createInPane(
    paneId: string,
    command?: string,
    connectionId?: string,
  ) {
    if (command && looksLikeWebLocation(command)) {
      openWebPane(command, connectionId, paneId);
      return;
    }
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
      focusSurfaceById(null);
      focusPaneFn?.(paneId);
      workspace.focusSession(sessionId);
    } else if (command || connectionId) {
      void createInPane(paneId, command, connectionId);
    } else {
      // Empty pane, no command — just move focus.
      focusPaneFn?.(paneId);
    }
    // Null first: closeOverlay restores previousFocus on a timeout, which
    // would steal focus back from the chosen pane — on touch devices that
    // drops the virtual keyboard and clears keyboardWanted.
    previousFocus = null;
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
    inBsp,
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
      parkMainViewSession();
      focusSurfaceById(null);
    },
    backgroundFocusedSession: parkMainViewSession,
    toggleOverlay,
    forwardCtrlK: forwardCtrlKToFocusedPane,
    cancelOverlay,
    toggleDebug,
    togglePreviewPanel,
    toggleLeftPanel: focusSection,
    toggleSearch: () => {
      // Three-way, not a plain toggle. Closed: open and focus. Open but
      // unfocused: just focus — you were looking at the results and asked
      // to get back to the query, not to lose them. Only when the input
      // already has focus does the chord dismiss. The query and results
      // survive a close (see ide/searchStore), so reopening resumes.
      if (!searchOpen()) {
        setSearchOpen(true);
        setSearchFocus((n) => n + 1);
      } else if (!searchInputFocused()) {
        setSearchFocus((n) => n + 1);
      } else {
        closeSearch();
      }
    },
    createAndFocus,
    createInPane,
    openNewTerminalPicker,
    handleRestartOrClose,
    connectionCount: () => allConnections().length,
    cycleRing,
    focusedAssignment,
    focusAssignment,
    clearFocusedPaneAssignment: () => {
      const paneId = bspFocusedPaneId();
      if (paneId) clearPaneAssignmentFn?.(paneId);
    },
    backgroundFocusedTile,
    closeFocusedTile,
    navigateBack: () => navigateHistory("back"),
    navigateForward: () => navigateHistory("forward"),
  });

  // Follow the focused terminal's cwd: poll it and expand the Explorer tree so
  // a `cd` reveals the directory. Server reads the pty cwd (no OSC-7 needed).
  // The same poll feeds the root-picker label (conn:cwd), so it runs whenever a
  // terminal is focused — not only when an IDE root is active.
  let lastFollowedCwd = "";
  /**
   * The worktree top of the repository enclosing `dir`, or null when there
   * is none. One bare `GIT_OPEN` (no watch, no status — nothing to compute
   * server-side) closed as soon as it has answered; asked once per `cd`,
   * since the poll below only reaches it when the cwd has changed.
   */
  const repoTopOf = async (
    connectionId: ConnectionId,
    dir: string,
  ): Promise<string | null> => {
    try {
      const handle = await workspace.openRepo(connectionId, dir, {});
      const top = handle.workdir;
      handle.close();
      return top || null;
    } catch {
      // Not a repository, or git is unavailable on that server: either way
      // there is no boundary here to re-root on.
      return null;
    }
  };
  const pollFocusedCwd = () => {
    const fid = wsState().focusedSessionId;
    if (!fid) {
      setFocusedTerm(null);
      return;
    }
    const focused = wsState().sessions.find((x) => x.id === fid);
    if (!focused) {
      setFocusedTerm(null);
      return;
    }
    const connId = focused.connectionId;
    workspace
      .sessionCwd(connId, fid)
      .then((cwd) => {
        if (!cwd) {
          // No answer for this pty. Drop a reading that belongs to the
          // terminal we just switched away from rather than leaving it
          // on screen attributed to this one.
          setFocusedTerm((prev) =>
            prev && prev.sessionId !== fid ? null : prev,
          );
          return;
        }
        lastTerminalCwds.set(terminalCwdKey(connId, focused.ptyId), cwd);
        setFocusedTerm({
          sessionId: fid,
          conn: connId,
          ptyId: focused.ptyId,
          cwd,
        });
        // A stale override for another terminal never outlives its focus.
        const ov = termCwdOverride();
        if (ov && ov.sessionId !== fid) setTermCwdOverride(null);
        const s = activeSession();
        const root = s?.root();
        if (!s || !root || cwd === lastFollowedCwd) return;
        lastFollowedCwd = cwd;
        if (cwd === root || cwd.startsWith(`${root}/`)) {
          // Inside the current root: reveal, don't re-root.
          s.expandTo(cwd === root ? "" : cwd.slice(root.length + 1));
          // Unless the cd crossed into a *different repository*. `cd linux`
          // from a plain `/src` is not a subdirectory to expand — it is a
          // project to show, and Files and Log belong to that repo rather
          // than to the directory above it. Only a repo boundary re-roots,
          // so cd-ing deeper inside one repo still just expands.
          if (cwd !== root && rootSel().kind === "focused") {
            void repoTopOf(connId, cwd).then((top) => {
              if (!top || top === root || top === s.repoWorkdir()) return;
              // The repo must enclose the cwd and sit inside the current
              // root: a repo *above* the root is the outer project the user
              // narrowed away from on purpose.
              if (top !== cwd && !cwd.startsWith(`${top}/`)) return;
              if (!top.startsWith(`${root}/`)) return;
              // Root at the repo's top, not at the cwd, so `cd linux/mm`
              // still shows the whole kernel.
              setTermCwdOverride({
                sessionId: fid,
                connectionId: connId,
                cwd: top,
              });
            });
          }
        } else if (rootSel().kind === "focused") {
          // Outside it (and the dock follows the terminal, not a pinned
          // root): re-root Files + Log at the new cwd.
          setTermCwdOverride({ sessionId: fid, connectionId: connId, cwd });
        }
      })
      .catch(() => {});
  };
  onMount(() => {
    // Paused while the document is hidden — a background tab has nothing to
    // reveal; becoming visible polls immediately and resumes the interval.
    let timer: ReturnType<typeof setInterval> | null = null;
    const stop = () => {
      if (timer != null) {
        clearInterval(timer);
        timer = null;
      }
    };
    const start = () => {
      pollFocusedCwd();
      if (timer == null) timer = setInterval(pollFocusedCwd, 1500);
    };
    const onVisibility = () => {
      if (document.visibilityState === "hidden") stop();
      else start();
    };
    if (document.visibilityState !== "hidden") start();
    document.addEventListener("visibilitychange", onVisibility);
    onCleanup(() => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    });
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
  createEffect(() => {
    historyReplaceFlush();
    // Debug visibility is local UI state, so keep it shareable even while the
    // transport is disconnected and the connection-gated state below cannot
    // yet be refreshed.
    const debugOpen = debugPanel();
    const currentHash = location.hash.slice(1);
    const debugHash = withDebugPanelState(currentHash, debugOpen);
    if (debugHash !== currentHash) {
      history.replaceState(
        null,
        "",
        debugHash ? `#${debugHash}` : location.pathname + location.search,
      );
    }

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
      const byId = new Map(sessions().map((s) => [s.id, s]));
      const a = Object.entries(la.assignments)
        .filter(([, sid]) => sid != null)
        .map(([pane, sid]) => {
          if (sid != null && isTileAssignment(sid)) {
            // IDE tile — a short server-side tab id (docs/design/kv.md):
            // "0:t:hound:0k3vq8za". The tile is registered under tabs/<id>
            // by the open-tile diff effect; the hash carries only the ref.
            const t = stripConn(sid);
            return t ? `${pane}:t:${t.connectionId}:${tabId(t.bare)}` : null;
          }
          const web = sid != null ? parseWebAssignment(sid) : null;
          if (web && sid != null) {
            // By id, like every other tab: the URL lives in the server's KV
            // registry, not in the hash.
            const t = stripConn(sid);
            return t ? `${pane}:w:${t.connectionId}:${tabId(t.bare)}` : null;
          }
          const parsed = parseSurfaceAssignment(sid);
          if (parsed) {
            // e.g. "1.0:s:hound:42"
            return `${pane}:s:${parsed.connectionId}:${parsed.surfaceId}`;
          }
          const s = byId.get(sid as SessionId);
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
    // Non-BSP focused tile (editor/diff/commit) — persist as a short tab ref.
    const fTile = activeTile();
    if (fTile && !inBsp()) {
      const t = stripConn(fTile);
      if (t) parts.push(`tile=${t.connectionId}:${tabId(t.bare)}`);
    }
    // Panel chrome: which side panels are open (d=) and which left-dock
    // sections are expanded (x=). Always written so a present key is
    // authoritative on restore — "both panels closed" (d=) and "all sections
    // collapsed" (x=) are states the hash must be able to carry.
    parts.push(`d=${formatPanelsHash(leftDockOpen(), previewPanelOpen())}`);
    parts.push(`x=${formatExpandedHash(collapsedSections())}`);
    const existing = location.hash.slice(1);
    // Strip layout-managed keys (l, p, a) from the old hash only when we
    // have fresh values to replace them.  While BSPContainer is still
    // resolving hash assignments (assignmentsResolved is false), keep
    // the existing `a=` (and `p=`) so the original shareable hash
    // survives until resolution completes.
    const written = new Set(parts.map((p) => p.slice(0, p.indexOf("="))));
    written.add("l");
    if (paneId) written.add("p");
    // `s=` is recomputed every write, like `l=`: claiming it unconditionally is
    // what lets "no surface focused" actually erase the key. Deriving ownership
    // from `parts` alone made it write-once — a surface focused a single time
    // stayed in the URL forever, re-arming focusedSurfaceId on every load and
    // (via loadActiveLayout) blocking the stored layout from seeding at all.
    written.add("s");
    // Guarded on `la` as well as `resolved`, matching the push above: the
    // strip set says "this run owns these keys", so claiming `a` without
    // having written one deletes the existing assignments instead of
    // replacing them. Pane contents live only in the hash, and tiles
    // (unlike terminals) cannot be re-derived from workspace state — so
    // that deletion is what loses every non-terminal pane.
    //
    // The window is an HMR remount: `assignmentsResolved` seeds true and
    // BSPContainer only flips it false an effect later, so the first run
    // sees resolved-with-nothing-to-write. A cold load is saved by the
    // not-connected bail above; an HMR remount hands back an already
    // connected workspace and sails past it.
    if (la && resolved) written.add("a");
    const kept = existing.split("&").filter(
      (s) =>
        s &&
        // `tile=` is recomputed every write — drop the stale one, EXCEPT
        // while a hash-restored tile ref is still resolving (the fetch
        // hasn't settled); erasing it then would lose the restore target.
        !(s.startsWith("tile=") && !pendingActiveTileRef()) &&
        !(/^[lpastdx]=/.test(s) && written.has(s.slice(0, s.indexOf("=")))),
    );
    const merged = [...kept, ...parts];
    const newHash = withDebugPanelState(merged.join("&"), debugOpen);
    if (newHash !== existing && !bspResizeHistoryPending) {
      history.replaceState(
        null,
        "",
        newHash ? `#${newHash}` : location.pathname + location.search,
      );
    }
  });

  const { countFrame, timeline, net, metrics } = createMetrics(
    () => props.connectionSpecs().map((s) => s.transport),
    debugPanel,
  );

  // Surface timing samples exist solely for the debug pane. Avoid creating
  // and correlating one record per video frame while it is closed.
  createEffect(() => workspace.setSurfaceDiagnosticsEnabled(debugPanel()));

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
  // Intent alone isn't enough for the key line: it must vanish the moment the
  // software keyboard is reduced, not a settling period later when intent
  // expires — and never sit over a keyboard that failed to rise (hardware
  // keyboard attached, focus lost to an overlay).  The occlusion gate tracks
  // the keyboard itself; the iPadOS shortcut bar (>32px) still counts.
  const showMobileToolbar = createMemo(
    () => isMobileTouch() && keyboardWanted() && viewportOccluded(),
  );
  const statusBarHeight = () => chromeScale().md + chromeScale().controlY * 3;

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette()}
      fontFamily={resolvedFontWithFallback()}
      fontSize={fontSize()}
      advanceRatio={advanceRatio()}
      textGamma={textGamma()}
    >
      <main
        style={{
          ...layout.workspace,
          "background-color": theme().bg,
          color: theme().fg,
          "font-family": resolvedFontWithFallback(),
          // While anything is parked over the viewport, pin to it so content
          // is not hidden.  Otherwise let the 100dvh root size the app
          // natively to avoid double-counting keyboard/browser-chrome space.
          ...(isMobileTouch() && viewportOccluded() && vpHeight()
            ? {
                position: "fixed",
                "inset-inline": "0",
                top: "0",
                height: `${vpHeight()}px`,
                transform: `translateY(${vpOffset()}px)`,
              }
            : {}),
        }}
      >
        <PersistentWebPanes
          assignments={persistentWebAssignments()}
          registry={webPaneHosts}
          onHandle={(assignment, handle) =>
            setWebHandles((previous) => ({
              ...previous,
              [assignment]: handle,
            }))
          }
        />
        <section
          style={{
            ...layout.termContainer,
            display: "flex",
            "flex-direction": "row",
          }}
        >
          <Show when={leftDockOpen()}>
            <LeftDock
              collapsed={collapsedForDock()}
              weights={sectionWeights()}
              header={rootPickerHeader()}
              theme={theme()}
              scale={chromeScale()}
              isMobileTouch={isMobileTouch()}
              width={leftDockWidth()}
              onResizeWidth={persistLeftDockWidth}
              onResizeWeight={resizeSectionWeight}
              onToggleCollapse={toggleSectionCollapse}
              renderBody={panelBody}
            />
          </Show>
          {/* The middle column, with the docks flanking it: project
              search is a top pane *here*, so the left dock and the preview
              panel keep their full height beside it rather than being
              pushed down by it. */}
          <div
            style={{
              flex: 1,
              "min-width": 0,
              display: "flex",
              "flex-direction": "column",
              overflow: "hidden",
            }}
          >
            <Show when={searchOpen()}>
              <section
                data-blit-search-pane
                style={{
                  // Auto by default — the pane is as tall as its results,
                  // capped at half the column so it can never swallow what
                  // you are searching. Dragging the handle pins an explicit
                  // fraction and drops the cap.
                  ...(searchHeight() == null
                    ? { flex: "0 1 auto", "max-height": "50%" }
                    : { flex: `0 0 ${(searchHeight()! * 100).toFixed(1)}%` }),
                  // No floor: an empty query should be exactly the input
                  // row, not a box with blank lines under it. Dragging the
                  // handle pins a height and can still shrink it away.
                  display: "flex",
                  "flex-direction": "column",
                  overflow: "hidden",
                  background: theme().bg,
                }}
              >
                <SearchPanel
                  {...leftPanelProps}
                  focusNonce={searchFocus()}
                  onClose={closeSearch}
                />
              </section>
              <ResizeHandle
                direction="vertical"
                onDrag={(fraction) =>
                  setSearchHeight((cur) =>
                    Math.min(
                      0.9,
                      Math.max(0.08, (cur ?? autoSearchFraction()) + fraction),
                    ),
                  )
                }
              />
            </Show>
            <div
              style={{ flex: 1, overflow: "hidden", position: "relative" }}
              onMouseEnter={() => setMainViewHover(true)}
              onMouseLeave={() => setMainViewHover(false)}
              // Drop target for the single-pane main view. Every handler bails
              // in BSP mode: panes are the precise targets there and they sit
              // inside this div, so without the guard their drops would bubble
              // up and be handled twice.
              onDragOver={(e) => {
                if (inBsp() && activeLayout()) return;
                if (!isTileDrag(e)) return;
                e.preventDefault(); // allow the drop
                e.dataTransfer!.dropEffect = "copy";
                if (!mainViewDragOver()) setMainViewDragOver(true);
              }}
              onDragLeave={(e) => {
                // Ignore leaves into child elements; only clear when truly
                // leaving (same rule as a BSP pane).
                if (!e.currentTarget.contains(e.relatedTarget as Node | null))
                  setMainViewDragOver(false);
              }}
              onDrop={(e) => {
                setMainViewDragOver(false);
                if (inBsp() && activeLayout()) return;
                const assignment = tileDragAssignment(e);
                if (!assignment) return;
                e.preventDefault();
                focusAssignment(assignment);
              }}
            >
              <Show when={mainViewDragOver()}>
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
              {/* In BSP mode each pane carries its own ✕ (BSPContainer), so
                  this one would be a second, ambiguous control over whichever
                  pane happens to be focused. */}
              <Show when={!(inBsp() && activeLayout()) && mainViewClosable()}>
                <PaneTools
                  theme={theme()}
                  scale={chromeScale()}
                  alwaysVisible={isMobileTouch()}
                  hovered={mainViewHover()}
                  drag={
                    mainViewDragAssignment() != null
                      ? {
                          assignment: mainViewDragAssignment()!,
                          paneId: MAIN_PANE_SOURCE,
                        }
                      : undefined
                  }
                  onClose={closeFocusedPane}
                />
              </Show>
              <Show
                when={inBsp() && activeLayout()}
                fallback={
                  <Show
                    when={parseWebAssignment(activeTile())}
                    fallback={
                      <Show
                        when={activeTile()}
                        fallback={
                          <Show
                            when={focusedSurfaceId()}
                            fallback={
                              <Show
                                when={mainViewSessionId()}
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
                                    onCreateInPane={(
                                      _paneId,
                                      command,
                                      connectionId,
                                    ) => {
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
                                        void createAndFocus(
                                          command,
                                          connectionId,
                                        );
                                      }
                                    }}
                                    onSwitcher={() => toggleOverlay("expose")}
                                    onHelp={() => toggleOverlay("help")}
                                  />
                                }
                              >
                                {(fid) => (
                                  <>
                                    <TerminalDropTarget
                                      workspace={workspace}
                                      sessionId={fid()}
                                      connectionId={
                                        focusedSession()?.connectionId ??
                                        activeConnectionId()
                                      }
                                      surface={terminalSurface}
                                      theme={theme()}
                                      scale={chromeScale()}
                                    >
                                      <BlitTerminal
                                        sessionId={fid()}
                                        readOnly={isSessionReadOnly(fid())}
                                        onRender={countFrame}
                                        style={{
                                          width: "100%",
                                          height: "100%",
                                        }}
                                        fontFamily={resolvedFontWithFallback()}
                                        fontSize={fontSize()}
                                        palette={palette()}
                                        surfaceRef={(s) => {
                                          setTerminalSurface(s);
                                          bindTerminalLinks(s);
                                        }}
                                      />
                                    </TerminalDropTarget>
                                    <Show
                                      when={
                                        focusedSession()?.state === "exited"
                                      }
                                    >
                                      <div
                                        style={{
                                          position: "absolute",
                                          bottom: "32px",
                                          left: "50%",
                                          transform: "translateX(-50%)",
                                          "background-color":
                                            theme().solidPanelBg,
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
                                            "background-color":
                                              "rgba(255,100,100,0.3)",
                                          }}
                                        >
                                          {t("workspace.exited")}
                                        </mark>
                                        <Show
                                          when={connection()?.supportsRestart}
                                        >
                                          <button
                                            onClick={() =>
                                              handleRestartOrClose()
                                            }
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
                                            if (fs)
                                              void workspace.closeSession(
                                                fs.id,
                                              );
                                          }}
                                          style={mergeStyle(ui.btn, {
                                            "font-size": `${chromeScale().md}px`,
                                            opacity: 0.5,
                                          })}
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
                                zoom={surfaceZoom() / 100}
                                zoomMode={surfaceZoomMode()}
                                touchMode={surfaceTouchMode()}
                                style={{
                                  width: "100%",
                                  height: "100%",
                                }}
                              />
                            )}
                          </Show>
                        }
                      >
                        {/* No pinned ✕ here: PaneTools floats over the main
                            view for every pane kind, close included, and it
                            can be relocated out of the content's way. */}
                        {(tile) => (
                          <div
                            style={{
                              width: "100%",
                              height: "100%",
                              position: "relative",
                            }}
                          >
                            <BlitTile
                              workspace={workspace}
                              assignment={tile()}
                              focused
                              theme={theme()}
                              palette={palette()}
                              scale={chromeScale()}
                              fontFamily={resolvedFontWithFallback()}
                              fontSize={fontSize()}
                              onOpenTile={openTile}
                              isConnectionReadOnly={isConnectionReadOnly}
                            />
                          </div>
                        )}
                      </Show>
                    }
                  >
                    {/* Same as the tile branch: PaneTools is the close. */}
                    <div
                      style={{
                        width: "100%",
                        height: "100%",
                        position: "relative",
                      }}
                    >
                      <WebPaneHost
                        assignment={activeTile()!}
                        hostId={NAV_NONBSP}
                        register={webPaneHosts.register}
                        focused
                      />
                    </div>
                  </Show>
                }
              >
                {(al) => (
                  <BSPContainer
                    layout={al()}
                    onLayoutChange={setBspLayout}
                    connectionId={activeConnectionId()}
                    isSessionReadOnly={isSessionReadOnly}
                    isConnectionReadOnly={isConnectionReadOnly}
                    connectionLabels={connectionLabels()}
                    palette={palette()}
                    fontFamily={resolvedFontWithFallback()}
                    fontSize={fontSize()}
                    surfaceZoom={surfaceZoom() / 100}
                    surfaceZoomMode={surfaceZoomMode()}
                    surfaceTouchMode={surfaceTouchMode()}
                    focusedSessionId={wsState().focusedSessionId}
                    lruSessionIds={lru}
                    liveSurfaceKeys={surfaces().map(
                      (s) => `${s.connectionId}:${s.surfaceId}`,
                    )}
                    hasAttention={hasAttention}
                    manageVisibility={overlay() !== "expose"}
                    extraVisibleSessions={
                      previewPanelVisible()
                        ? offScreenSessions().map((s) => s.id)
                        : []
                    }
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
                      // Flush a tile queued while there was no layout (the fresh
                      // layout has settled its initial assignment by now).
                      if (pendingTilePlacement) {
                        const p = pendingTilePlacement;
                        pendingTilePlacement = null;
                        fn(p.assignment, p.paneId);
                      }
                    }}
                    onClearPaneAssignment={(fn) => {
                      clearPaneAssignmentFn = fn;
                    }}
                    onFocusedPaneChange={setBspFocusedPaneId}
                    onOpenTile={openTile}
                    registerWebPaneHost={webPaneHosts.register}
                    onDropTile={dropTileIntoPane}
                    isMobileTouch={isMobileTouch()}
                    onCloseTab={closeTab}
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
                    onTerminalSurface={bindTerminalLinks}
                  />
                )}
              </Show>
            </div>
          </div>
          <Show when={previewPanelVisible()}>
            <PreviewPanel
              parkDropActive={paneDragActive()}
              onParkDrop={parkDraggedAssignment}
              offScreenSessions={offScreenSessions()}
              surfaces={offScreenSurfaces()}
              focusedSurfaceId={focusedSurfaceId()}
              focusedSurfaceConnId={focusedSurfaceConnId()}
              hasAttention={hasAttention}
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
              onResize={persistPreviewPanelWidth}
              onClose={togglePreviewPanel}
              backgroundEditors={
                <For each={backgroundTiles()}>
                  {(assignment, index) => {
                    // Re-read, not read once: a manage tile's title carries the
                    // tab its panels are on, which changes under the card.
                    const d = () => tileDisplay(assignment);
                    const web = parseWebAssignment(assignment);
                    return (
                      // The same card parked terminals and surfaces get:
                      // swipe right dismisses, swipe left (or a hold)
                      // starts the drag, a click restores to the main view.
                      <Thumbnail
                        theme={theme()}
                        scale={chromeScale()}
                        isMobileTouch={isMobileTouch()}
                        assignment={assignment}
                        onFocus={() => restoreTile(assignment)}
                        onClose={() => closeBackgroundTile(assignment)}
                        closeTitle="Close"
                        header={() => (
                          <span
                            style={{
                              flex: 1,
                              "min-width": 0,
                              "text-align": "left",
                              display: "flex",
                              "flex-direction": "column",
                              overflow: "hidden",
                            }}
                          >
                            <span
                              style={{
                                "white-space": "nowrap",
                                overflow: "hidden",
                                "text-overflow": "ellipsis",
                                "max-width": "100%",
                                "font-size": `${chromeScale().sm}px`,
                              }}
                            >
                              {/* Address dim, then the name — the same shape
                                  the terminal and surface cards below use, so
                                  a column of parked things reads as one list
                                  rather than three conventions. */}
                              <Show when={d().prefix}>
                                <span style={{ opacity: 0.5 }}>
                                  {d().prefix}
                                </span>
                                <Show when={d().title}>{" \u203A "}</Show>
                              </Show>
                              {d().title}
                            </span>
                            <Show when={d().subtitle}>
                              <span
                                style={{
                                  "white-space": "nowrap",
                                  overflow: "hidden",
                                  "text-overflow": "ellipsis",
                                  "max-width": "100%",
                                  "font-size": `${chromeScale().xs}px`,
                                  opacity: 0.6,
                                }}
                              >
                                {d().subtitle}
                              </span>
                            </Show>
                          </span>
                        )}
                        body={() => (
                          // Read-only zoomed-out preview, terminal-thumbnail
                          // semantics: click to bring it back to the main
                          // view. Only the most recent cards are live — a
                          // mounted preview editor holds an fs sync and a web
                          // preview holds an iframe, so both are budgeted
                          // (LIVE_DOCK_PREVIEWS).
                          //
                          // A manage tile has no picture worth taking: its
                          // panels are lists of text at a size nobody can read,
                          // and mounting them to draw that would run a client
                          // catalog every second behind the card. Its title
                          // says which server and which tab, which is the whole
                          // of what the card is picked by.
                          <Show
                            when={
                              index() < LIVE_DOCK_PREVIEWS &&
                              d().kind !== "manage"
                            }
                          >
                            <div
                              style={{
                                position: "relative",
                                width: "100%",
                                height: `${Math.min(240, Math.max(120, Math.round(fontSize() * 12)))}px`,
                                overflow: "hidden",
                                "background-color": theme().bg,
                              }}
                            >
                              <Show
                                when={web}
                                fallback={
                                  <BlitTile
                                    workspace={workspace}
                                    assignment={assignment}
                                    theme={theme()}
                                    palette={palette()}
                                    scale={chromeScale()}
                                    fontFamily={resolvedFontWithFallback()}
                                    fontSize={Math.max(
                                      7,
                                      Math.round(fontSize() * 0.6),
                                    )}
                                    onOpenTile={openTile}
                                    isConnectionReadOnly={isConnectionReadOnly}
                                    preview
                                  />
                                }
                              >
                                {(_) => (
                                  <WebPaneHost
                                    assignment={assignment}
                                    hostId={`dock:${assignment}`}
                                    register={webPaneHosts.register}
                                    interactive={false}
                                  />
                                )}
                              </Show>
                            </div>
                          </Show>
                        )}
                      />
                    );
                  }}
                </For>
              }
            />
          </Show>
        </section>
        <Show when={overlay() === "expose"}>
          {(_) => (
            <SwitcherOverlay
              sessions={sessions()}
              focusedSessionId={
                focusedSurfaceId() != null || mainTerminalParked()
                  ? null
                  : wsState().focusedSessionId
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
                // Null first: closeOverlay restores previousFocus on a
                // timeout — see selectPane.
                previousFocus = null;
                closeOverlay();
              }}
              onApplyLayout={(l) => {
                // Re-applying the already-active layout object (e.g. the
                // current preset from the switcher) is a no-op: the signal
                // setter below would not notify on the same reference, so
                // clearing layoutAssignments here would leave it null
                // forever — tile counts vanish from the status bar and the
                // side panel goes empty until reload.
                if (l === activeLayout()) {
                  closeOverlay();
                  return;
                }
                // Clear stale assignments immediately so the hash sync
                // effect (which runs before BSPContainer re-computes)
                // doesn't write old pane IDs into the URL.
                setLayoutAssignments(null);
                // Clear any focused surface — BSP takes over the main
                // area so the surface overlay won't render, and leaving
                // focusedSurfaceId set would hide the surface from the
                // side panel as well (offScreenSurfaces filters it out).
                focusSurfaceById(null);
                // Same for a non-BSP tile: entering BSP hides the fullscreen
                // slot, so leaving activeTile set would count as "displayed"
                // and keep it out of the dock while nothing renders it.
                // Clearing hands it to the dock; the tab stays open.
                setActiveTile(null);
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
                exitBspLayout();
                saveActiveLayout(null);
                closeOverlay();
              }}
              recentLayouts={recentLayouts()}
              presetLayouts={PRESETS}
              onOpenWeb={() => toggleOverlay("web")}
              onOpenSearch={() => {
                // Null first: closeOverlay restores previousFocus (the
                // terminal) on a timeout, which would steal the search
                // input's focus right back.
                previousFocus = null;
                closeOverlay();
                if (!searchOpen()) setSearchOpen(true);
                setSearchFocus((n) => n + 1);
              }}
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
                // Null first: closeOverlay restores previousFocus on a
                // timeout — see selectPane.
                previousFocus = null;
                closeOverlay();
              }}
              backgroundTiles={backgroundTiles()}
              onRestoreTile={(assignment) => {
                restoreTile(assignment);
                closeOverlay();
              }}
              fileSearchLocal={(q) => {
                const s = activeSession();
                const root = s?.root() ?? "";
                if (!s || !root) return null;
                // A truncated list (giant tree) is still served — a best-
                // effort prefix beats nothing, and the budgets make it rare.
                const index = localFileIndex(workspace, s.connectionId, root);
                if (!index) return null;
                const recency = editorRecencySnapshot(s.connectionId);
                return searchFileIndex(index, q, 100, (rel) => {
                  return recency.get(`${root}/${rel}`) ?? null;
                });
              }}
              fileSearchWarm={() => {
                const s = activeSession();
                const root = s?.root() ?? "";
                if (s && root) localFileIndex(workspace, s.connectionId, root);
              }}
              onOpenFile={(relPath) => {
                const s = activeSession();
                if (!s) return;
                // "" when the session has no synced root yet.
                const a = s.fileAssignment(relPath);
                if (a) openTile(a);
              }}
              symbolSearchWarm={() => activeSession()?.ensureLsp()}
              symbolSearch={async (q) => {
                const s = activeSession();
                const h = s?.lspHandle();
                // An empty query asks most backends for everything; skip
                // it rather than pull the whole index over the wire.
                if (!h || !q) return [];
                const res = await h.workspaceSymbols(q);
                if (res.status !== LSP_STATUS_OK) return [];
                return res.records
                  .filter((r) => r.kind === "symbol")
                  .map((r) => ({
                    name: r.name,
                    symKind: r.symKind,
                    path: r.path,
                    line: r.line,
                    col: r.col,
                  }));
              }}
              onOpenSymbol={(hit) => {
                const s = activeSession();
                if (!s) return;
                // Symbol paths are relative to the LSP root, which is not
                // always the fs root — resolve against the attachment's
                // own root rather than through fileAssignment().
                const root = (s.lspHandle()?.root ?? s.root() ?? "").replace(
                  /\/+$/,
                  "",
                );
                const abs = hit.path.startsWith("/")
                  ? hit.path
                  : `${root}/${hit.path.replace(/^\/+/, "")}`;
                setReveal(s.connectionId, abs, {
                  text: "",
                  line: hit.line + 1, // LSP is 0-based, reveal is 1-based
                  col: hit.col,
                });
                openTile(editorAssignment(s.connectionId, abs));
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
              currentGamma={textGamma()}
              serverFonts={serverFonts()}
              fontChoices={fontCatalog()}
              palette={palette()}
              fontSize={fontSize()}
              onSelect={changeFont}
              onPreview={(family, size, gamma) => {
                setFont(family);
                setFontSize(size);
                setTextGamma(gamma);
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
        <Show when={overlay() === "link" && pendingLink()}>
          {(pending) => (
            <LinkOverlay
              palette={palette()}
              fontSize={fontSize()}
              assessment={pending().assessment}
              linkText={pending().text}
              onOpen={() => {
                const url = pending().assessment.raw;
                closeOverlay();
                window.open(url, "_blank", "noopener,noreferrer");
              }}
              onClose={closeOverlay}
            />
          )}
        </Show>
        <Show when={overlay() === "remotes" && shellCapabilities().remotes}>
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
              onToggle={(name) => toggleRemote(name)}
              onSetDefault={(name) => setDefaultRemote(name)}
              onReorder={(names) => reorderRemotes(names)}
              onReconnect={(name) => workspace.reconnectConnection(name)}
              onClose={closeOverlay}
              connections={allConnections()}
              onManage={(name) => {
                // The panels are a tile, so the dialog that asked for them is
                // in the way once they exist.
                closeOverlay();
                openTile(manageAssignment(name));
              }}
            />
          )}
        </Show>
        <Show when={overlay() === "web"}>
          <WebOverlay
            locations={webLocations()}
            remotes={allConnections().map((c) => ({
              id: c.id,
              label: connectionLabels().get(c.id) ?? c.id,
            }))}
            dest={webDestId()}
            onDest={setWebDest}
            palette={{
              bg: theme().bg,
              fg: theme().fg,
              accent: theme().accent,
              dim: theme().border,
              selectedBg: theme().selectedBg,
              subtleBorder: theme().subtleBorder,
            }}
            fontSize={chromeScale().md}
            unavailable={webUnavailable()}
            onOpen={(url, dest) => openWebPane(url, dest)}
            onForget={persistWebLocations}
            onClose={() => setOverlay(null)}
          />
        </Show>
        <Show when={overlay() === "roots"}>
          {(_) => (
            <RootsOverlay
              roots={roots()}
              remotes={remotes()}
              gatewayStatus={configWsStatus()}
              palette={palette()}
              fontSize={fontSize()}
              workspace={workspace}
              connectionForRemote={connectionForRemote}
              defaultRemote={
                activeConnectionId() === defaultConnectionId()
                  ? ""
                  : activeConnectionId()
              }
              defaultPath={activeSession()?.root() ?? ""}
              onAdd={(name, remote, path) => {
                const connId = connectionForRemote(remote);
                if (hasServerRoots(connId))
                  addServerRoot(workspace, connId, name, path);
                else addRoot(name, remote, path);
              }}
              onRemove={(name) => {
                const r = roots().find((x) => x.name === name);
                const connId = r && connectionForRemote(r.remote);
                if (connId && hasServerRoots(connId))
                  removeServerRoot(workspace, connId, name);
                else removeRoot(name);
              }}
              onToggle={(name) => {
                const r = roots().find((x) => x.name === name);
                const connId = r && connectionForRemote(r.remote);
                if (connId && hasServerRoots(connId))
                  toggleServerRoot(workspace, connId, name);
                else toggleRoot(name);
              }}
              onReorder={(names) => {
                // A global drag-order splits into each store's subset,
                // preserving relative order within it.
                const byConn = new Map<string, string[]>();
                const gateway: string[] = [];
                for (const name of names) {
                  const r = roots().find((x) => x.name === name);
                  if (!r) continue;
                  const connId = connectionForRemote(r.remote);
                  if (hasServerRoots(connId)) {
                    const list = byConn.get(connId) ?? [];
                    list.push(name);
                    byConn.set(connId, list);
                  } else {
                    gateway.push(name);
                  }
                }
                for (const [connId, subset] of byConn) {
                  reorderServerRoots(workspace, connId as ConnectionId, subset);
                }
                if (gateway.length > 0) reorderRoots(gateway);
              }}
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
              videoBandwidth={videoBandwidth()}
              videoSpeed={videoSpeed()}
              audioMuted={audioMuted()}
              audioAvailable={allConnections().some((c) => c.supportsAudio)}
              surfaceStreaming={surfaceStreaming()}
              surfaceSmoothing={surfaceSmoothing()}
              surfaceMaxFps={surfaceMaxFps()}
              surfaceZoom={surfaceZoom()}
              surfaceZoomMode={surfaceZoomMode()}
              surfaceTouchMode={surfaceTouchMode()}
              surfaceTouchAvailable={allConnections().some(
                (connection) => connection.supportsSurfaceTouch,
              )}
              waylandKeyboardRequests={waylandKeyboardRequests()}
              devices={mediaDevices}
              surfaceCodecs={surfaceCodecs()}
              probedSurfaceCodecs={probedSurfaceCodecs()}
              onSurfaceCodecsChange={changeSurfaceCodecs}
              onAudioBitrateChange={changeAudioBitrate}
              onVideoBandwidthChange={changeVideoBandwidth}
              onVideoSpeedChange={changeVideoSpeed}
              onSurfaceStreamingChange={changeSurfaceStreaming}
              onSurfaceSmoothingChange={changeSurfaceSmoothing}
              onSurfaceMaxFpsChange={changeSurfaceMaxFps}
              onSurfaceZoomChange={changeSurfaceZoom}
              onSurfaceZoomModeChange={changeSurfaceZoomMode}
              onSurfaceTouchModeChange={changeSurfaceTouchMode}
              onWaylandKeyboardRequestsChange={changeWaylandKeyboardRequests}
              onToggleAudio={toggleAudio}
              onClose={closeOverlay}
            />
          )}
        </Show>
        <footer
          style={{
            ...layout.statusBar,
            padding: showMobileToolbar()
              ? "0 1em"
              : "0 1em env(safe-area-inset-bottom)",
            "background-color": theme().bg,
            color: theme().fg,
            "border-top-color": theme().border,
            height: showMobileToolbar()
              ? `${statusBarHeight()}px`
              : `calc(${statusBarHeight()}px + env(safe-area-inset-bottom))`,
            "font-size": `${chromeScale().md}px`,
          }}
        >
          <StatusBar
            activities={activities()}
            sessions={sessions()}
            surfaceCount={surfaces().length}
            // Displayed panes plus docked tabs: backgroundTiles already
            // excludes whatever a pane (or the non-BSP slot) displays, so
            // the two never double count — and a parked editor still shows
            // up in the tally, like off-screen terminals do.
            tileCount={
              paneKindCount(isTileAssignment) +
              backgroundTiles().filter(isTileAssignment).length
            }
            webCount={
              paneKindCount(isWebAssignment) +
              backgroundTiles().filter(isWebAssignment).length
            }
            hoveredLink={hoveredLink()}
            focusedSession={
              focusedSurfaceId() != null ||
              bspFocusedSurface() != null ||
              mainTerminalParked()
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
            focusedCwd={(() => {
              // Only when the reading is about the session the bar is
              // naming — the poll keeps its last value when a pty can't
              // answer, and a cwd from the previous terminal is worse
              // than none.
              const f = focusedTerm();
              const fid = focusedSessionId();
              return f && fid && f.sessionId === fid ? f.cwd : null;
            })()}
            connectionLabels={connectionLabels()}
            connections={allConnections()}
            gatewayStatus={configWsStatus()}
            status={connectionStatus()}
            onRemotes={
              shellCapabilities().remotes
                ? () => toggleOverlay("remotes")
                : undefined
            }
            metrics={metrics()}
            palette={palette()}
            fontSize={fontSize()}
            fontFamily={resolvedFontWithFallback()}
            fontLoading={fontLoading()}
            debug={debugPanel()}
            toggleDebug={toggleDebug}
            previewPanelOpen={previewPanelOpen()}
            onPreviewPanel={togglePreviewPanel}
            leftDockOpen={leftDockOpen()}
            onToggleLeftDock={toggleLeftDock}
            webPane={focusedWebPane()}
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
            isMobileTouch={isMobileTouch()}
            // The icon and the toggle agree: lit means a keyboard input panel
            // is genuinely up, including the iPadOS shortcut bar.
            keyboardOpen={keyboardWanted() && viewportOccluded()}
            onToggleKeyboard={toggleMobileKeyboard}
            onMedia={() => toggleOverlay("media")}
            desktopChrome={(compact) => (
              <DesktopChrome
                workspace={workspace}
                connections={allConnections()}
                connectionLabels={connectionLabels()}
                readOnlyConnections={readOnlyConnections()}
                theme={theme()}
                scale={chromeScale()}
                compact={compact}
                focusedConnectionId={
                  focusedSurfaceConnId() ?? activeConnectionId()
                }
              />
            )}
          />
        </footer>
        <Show when={showMobileToolbar()}>
          <MobileToolbar
            keyboardTarget={() => {
              // Subscribe the lookup to pane/session focus. The target itself
              // is DOM-owned, but the toolbar must re-bind its modifier state
              // when focus moves while the software keyboard stays open.
              wsState().focusedSessionId;
              focusedSurfaceId();
              bspFocusedSurface();
              return focusedKeyboardInput();
            }}
            theme={theme()}
            scale={chromeScale()}
          />
        </Show>
      </main>
    </BlitWorkspaceProvider>
  );
}

function PreviewPanel(props: {
  offScreenSessions: BlitSession[];
  surfaces: BlitSurface[];
  focusedSurfaceId: number | null;
  focusedSurfaceConnId: ConnectionId | null;
  /** Is this pane assignment currently lit by an activation? */
  hasAttention: (assignment: string) => boolean;
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
  /** Live background-editor cards (rendered by WorkspaceScreen, which owns the
   *  tile assignments), shown above the terminal/surface thumbnails. */
  backgroundEditors?: JSX.Element;
  /** A grip drag is in flight: this panel is its drop-to-park target. */
  parkDropActive?: boolean;
  /** A grip drag landed here; park `assignment`, emptying `source`. */
  onParkDrop?: (assignment: string, source: string) => void;
}) {
  const [expandedId, setExpandedId] = createSignal<number | null>(null);
  const [resizeHover, setResizeHover] = createSignal(false);
  const [resizeActive, setResizeActive] = createSignal(false);
  /** The grip drag is hovering the panel (parallel to a pane's highlight). */
  const [parkOver, setParkOver] = createSignal(false);

  function handleResizePointerDown(e: PointerEvent) {
    e.preventDefault();
    (e.target as HTMLElement).setPointerCapture(e.pointerId);
    setResizeActive(true);
    const startX = e.clientX;
    const startWidth = props.width;
    // Cap the panel at a fraction of the viewport so a touch drag can't
    // push the terminal off-screen.
    const maxWidth = Math.max(
      MIN_PREVIEW_PANEL_WIDTH,
      Math.floor(window.innerWidth * 0.85),
    );

    const onMove = (me: PointerEvent) => {
      const delta = startX - me.clientX;
      props.onResize(
        Math.min(
          maxWidth,
          Math.max(MIN_PREVIEW_PANEL_WIDTH, startWidth + delta),
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

  // Touch targets need a fatter hit area than the 3px desktop bar to be
  // reliably grabbable with a finger.
  const handleWidth = () => (props.isMobileTouch ? 14 : 3);

  const resizeBg = () =>
    resizeActive()
      ? "rgba(128,128,128,0.5)"
      : resizeHover()
        ? "rgba(128,128,128,0.3)"
        : "transparent";

  return (
    <div
      // Named the way a BSP pane is (`data-blit-bsp-pane-id`): a parked card is
      // draggable, and so is every explorer row and commit, so "the parked
      // cards" is only expressible as a subtree.
      data-blit-preview-panel=""
      style={{
        width: `${props.width}px`,
        "flex-shrink": 0,
        display: "flex",
        "flex-direction": "row",
        overflow: "hidden",
        position: "relative",
      }}
      onDragOver={(e) => {
        if (!props.onParkDrop || !isPaneDrag(e)) return;
        e.preventDefault(); // allow the drop
        e.dataTransfer!.dropEffect = "move";
        if (!parkOver()) setParkOver(true);
      }}
      onDragLeave={(e) => {
        // Ignore leaves into child elements; only clear when truly leaving.
        if (!e.currentTarget.contains(e.relatedTarget as Node | null))
          setParkOver(false);
      }}
      onDrop={(e) => {
        setParkOver(false);
        const assignment = tileDragAssignment(e);
        const source = paneDragSource(e);
        if (assignment && source && props.onParkDrop) {
          e.preventDefault();
          props.onParkDrop(assignment, source);
        }
      }}
    >
      <Show when={props.parkDropActive}>
        <div
          style={{
            position: "absolute",
            inset: 0,
            "z-index": 5,
            "pointer-events": "none",
            "box-sizing": "border-box",
            border: parkOver()
              ? `2px solid ${props.theme.accent}`
              : `2px dashed ${props.theme.subtleBorder}`,
            background: parkOver()
              ? `color-mix(in srgb, ${props.theme.accent} 14%, transparent)`
              : "transparent",
          }}
        />
      </Show>
      <div
        onPointerDown={handleResizePointerDown}
        onPointerEnter={() => setResizeHover(true)}
        onPointerLeave={() => setResizeHover(false)}
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize panel"
        style={{
          width: `${handleWidth()}px`,
          "flex-shrink": 0,
          cursor: "col-resize",
          background: resizeBg(),
          "border-left": `1px solid ${props.theme.subtleBorder}`,
          transition: "background 0.1s",
          "touch-action": "none",
          display: "flex",
          "align-items": "center",
          "justify-content": "center",
        }}
      >
        <Show when={props.isMobileTouch}>
          <div
            style={{
              width: "3px",
              height: "32px",
              "border-radius": "2px",
              "background-color": props.theme.dimFg,
              opacity: resizeActive() ? 0.8 : 0.4,
              "pointer-events": "none",
            }}
          />
        </Show>
      </div>
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
            style={mergeStyle(ui.btn, {
              "font-size": `${props.scale.xs}px`,
              padding: `0 ${props.scale.tightGap}px`,
              opacity: 0.5,
            })}
          >
            {"\u00D7"}
          </button>
        </div>
        <div
          style={{
            flex: "1 1 0",
            "min-height": 0,
            "overflow-y": "auto",
            ...scrollbarStyle(props.theme),
          }}
        >
          {props.backgroundEditors}
          <Index each={props.offScreenSessions}>
            {(s) => (
              <SessionThumbnail
                session={s()}
                connectionLabel={props.connectionLabels?.get(s().connectionId)}
                theme={props.theme}
                scale={props.scale}
                palette={props.palette}
                fontFamily={props.fontFamily}
                fontSize={props.fontSize}
                isMobileTouch={props.isMobileTouch}
                onFocus={() => props.onFocusSession(s().id)}
                onClose={() => props.onCloseSession(s().id)}
              />
            )}
          </Index>
          <Index each={props.surfaces}>
            {(s) => (
              <SurfaceThumbnail
                surface={s()}
                connectionId={s().connectionId}
                connectionLabel={props.connectionLabels?.get(s().connectionId)}
                theme={props.theme}
                scale={props.scale}
                focused={
                  s().surfaceId === props.focusedSurfaceId &&
                  s().connectionId === props.focusedSurfaceConnId
                }
                attention={props.hasAttention(
                  surfaceAssignment(s().connectionId, s().surfaceId),
                )}
                isMobileTouch={props.isMobileTouch}
                onFocus={() =>
                  props.onFocusSurface(s().connectionId, s().surfaceId)
                }
                onClose={() =>
                  props.onCloseSurface(s().connectionId, s().surfaceId)
                }
              />
            )}
          </Index>
        </div>
      </div>
    </div>
  );
}

/** Minimum horizontal swipe distance (px) to trigger dismiss. */
const SWIPE_THRESHOLD = 60;
/** Minimum ratio of horizontal to vertical movement for a swipe. */
const SWIPE_RATIO = 1.5;

/** Shared wrapper for preview-panel thumbnails.  Handles swipe-right-to-
 *  dismiss (swipe-left starts a drag, via the touch-drag bridge), hover
 *  state, dismiss animation, header bar with close button. */
function Thumbnail(props: {
  theme: Theme;
  scale: UIScale;
  isMobileTouch: boolean;
  /** The pane assignment this card carries when dragged onto a BSP pane —
   *  a session id for a terminal, `surfaceAssignment(...)` for a surface,
   *  a tile assignment (`editor:`/…) for a background editor. */
  assignment: string;
  onFocus: () => void;
  onClose: () => void;
  closeTitle: string;
  /** Extra header-bar background (e.g. for focused highlight). */
  headerBg?: string;
  /** Pulse the header: this card's content asked to come forward. */
  attention?: boolean;
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
      // Only a rightward, horizontal-dominant swipe dismisses. A leftward
      // one is the touch-drag bridge's gesture (see onPointerDown), and a
      // vertical one is the list's scroll: neither is claimed here.
      if (dx <= 0 || dx < Math.abs(dy) * SWIPE_RATIO) return;
      setSwiping(true);
    }
    if (!swiping()) return;
    e.preventDefault();
    if (dx <= 0) {
      // Reversed through the origin: the gesture is now a leftward drag,
      // which the touch-drag bridge claims. Hand the card back to rest and
      // stay out of the way for the rest of the gesture.
      setSwiping(false);
      setSwipeX(0);
      return;
    }
    setSwipeX(dx);
  }

  function onTouchEnd() {
    if (swiping() && swipeX() >= SWIPE_THRESHOLD) {
      setDismissed(true);
      setSwipeX(400);
      setTimeout(() => props.onClose(), 200);
    } else {
      setSwipeX(0);
    }
    setSwiping(false);
  }

  return (
    // Draggable onto a BSP pane, like the background-tile cards above: the
    // card is inert (see the body wrapper), so the whole thing is the handle
    // and a drag can't be swallowed by the terminal or surface inside.
    // Touch is unaffected — mobile browsers don't synthesize dragstart, so
    // swipe-to-dismiss below keeps working.
    <div
      draggable={true}
      onDragStart={(e) => startTileDrag(e, props.assignment)}
      // Touch never reaches onDragStart. A leftward swipe starts the drag —
      // the one horizontal gesture the swipe-to-dismiss below does not claim
      // (it claims rightward) — and a hold works as a fallback. Either way
      // the card can still be flicked away to the right.
      onPointerDown={(e) =>
        startTouchDrag(
          e,
          (dt) => fillTileDrag(dt, props.assignment),
          "swipe-left",
        )
      }
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
        // The sweep drops the attribute when the window closes, so the next
        // activation adds it back and the animation plays again from the top.
        // Repeats *inside* the window never reach here (surfaceAttention.ts
        // absorbs them), which is what keeps this a pulse and not a strobe.
        data-blit-attention={props.attention ? "fill" : undefined}
        style={mergeStyle(ui.btn, {
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
        })}
      >
        {props.header()}
        <Show when={!props.isMobileTouch && hover()}>
          <button
            onClick={(e) => {
              e.stopPropagation();
              props.onClose();
            }}
            title={props.closeTitle}
            style={mergeStyle(ui.btn, {
              "font-size": `${props.scale.sm}px`,
              padding: `0 ${props.scale.tightGap}px`,
              opacity: 0.6,
              "flex-shrink": 0,
            })}
          >
            {"\u00D7"}
          </button>
        </Show>
      </button>
      <div
        style={{ overflow: "hidden", cursor: "pointer" }}
        onClick={props.onFocus}
      >
        {/* Parked content is inert, matching the background-tile cards.
            `inert` takes the subtree out of hit-testing *and* the tab order:
            a read-only BlitTerminal still attaches a keydown listener on a
            tabindex=0 input (scroll keys work), and a preview
            BlitSurfaceView's canvas is tabindex=0 too — so without this a
            parked card can take focus away from the live view. The explicit
            pointer-events keeps the click landing on the parent (restore),
            rather than relying on how each engine hit-tests inert. */}
        <div inert style={{ "pointer-events": "none" }}>
          {props.body()}
        </div>
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
      // A terminal's pane assignment is its bare session id.
      assignment={props.session.id}
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
          resizable={false}
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
  attention?: boolean;
  isMobileTouch: boolean;
  onFocus: () => void;
  onClose: () => void;
}) {
  return (
    <Thumbnail
      theme={props.theme}
      scale={props.scale}
      isMobileTouch={props.isMobileTouch}
      assignment={surfaceAssignment(
        props.surface.connectionId,
        props.surface.surfaceId,
      )}
      onFocus={props.onFocus}
      onClose={props.onClose}
      closeTitle="Close surface"
      headerBg={props.focused ? props.theme.selectedBg : undefined}
      attention={props.attention}
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
            {/* `dev:S3 \u203A Slack`. The id is the only thing that names a
                window unambiguously \u2014 titles repeat across an app's windows
                and change under you \u2014 and it is what `blit surface` takes,
                so the card doubles as the lookup for driving that window
                from a terminal. */}
            <span style={{ opacity: 0.5 }}>
              {props.connectionLabel ? `${props.connectionLabel}:` : ""}
              {`S${props.surface.surfaceId}`}
            </span>
            {" \u203A "}
            {props.surface.title ||
              props.surface.appId ||
              `Surface ${props.surface.surfaceId}`}
          </span>
        </>
      )}
      body={() => (
        <BlitSurfaceView
          connectionId={props.surface.connectionId}
          surfaceId={props.surface.surfaceId}
          // A card shares whatever stream the panes are already getting and
          // must not size the surface: its own height is derived from the
          // surface's aspect below, so driving a resize from it closes exactly
          // the loop that comment warns about.  It also has to stay in flow for
          // `height: auto` to have anything to measure.
          resizable={false}
          style={{
            display: "block",
            width: "100%",
            // The window's own aspect, *not* the canvas's. A card whose height
            // came from the canvas closed a loop: the height is what
            // BlitSurfaceCanvas measures into `_presentBox` to pick an encode
            // size, and the encode size is what sizes the canvas. So a card
            // whose height landed on an octave boundary (≈2^k/aspect: 113,
            // 227, 455 px wide at 16:9) asked for 256x128, got a 128-tall
            // stream, grew to 128.6, asked for 256x256, got a 144-tall
            // stream, shrank to 127.7, and repeated at ~16Hz. Each turn
            // retires and rebuilds a hardware encoder; a few hundred of those
            // segfaults the NVIDIA encode library and takes the server down.
            //
            // And the window's *logical* aspect, not the composited one. The
            // composite is the logical size times whatever scale the
            // highest-DPI viewer asked for, floored onto the even 4:2:0 grid,
            // so its ratio is off by up to a pixel per axis — and it moves
            // when another viewer's DPI does, for a window that never
            // changed. `logicalWidth`/`logicalHeight` move only when the app
            // resizes, which is the only thing this card should follow.
            //
            // Before the first surface info every dimension is 0; leave the
            // ratio off rather than emit a degenerate one, and the card is
            // laid out by the 640x480 placeholder canvas for that one frame.
            // A server too old to report a logical size falls back to the
            // composite, which is what it used to use throughout.
            ...cardAspectRatio(props.surface),
            height: "auto",
            "object-fit": "contain",
          }}
        />
      )}
    />
  );
}
