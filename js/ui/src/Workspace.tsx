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
  BlitWorkspace,
  PALETTES,
  DEFAULT_FONT,
  LSP_STATUS_OK,
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
} from "@blit-sh/core";
import type { ConnectionSpec } from "./App";
import { createMetrics } from "./createMetrics";
import { createFontLoader } from "./createFontLoader";
import { createKeyboardShortcuts } from "./createKeyboardShortcuts";
import {
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  TEXT_GAMMA_KEY,
  AUDIO_BITRATE_KEY,
  AUDIO_MUTED_KEY,
  VIDEO_QUALITY_KEY,
  SURFACE_STREAMING_KEY,
  LEFT_DOCK_WIDTH_KEY,
  PREVIEW_PANEL_WIDTH_KEY,
  writeStorage,
  useConfigValue,
  preferredPalette,
  preferredFont,
  preferredFontSize,
  preferredTextGamma,
  preferredAudioBitrate,
  preferredAudioMuted,
  preferredVideoQuality,
  preferredSurfaceStreaming,
  preferredLeftDockWidth,
  preferredPreviewPanelWidth,
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
} from "./storage";
import type { UIScale, Theme } from "./theme";
import {
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
import { StatusBar } from "./StatusBar";
import { LeftDock, LEFT_PANELS, type LeftPanel } from "./LeftDock";
import { ExplorerPanel } from "./ide/ExplorerPanel";
import { LogPanel } from "./ide/LogPanel";
import { SearchPanel } from "./ide/SearchPanel";
import { ResizeHandle } from "./bsp/ResizeHandle";
import { searchInputFocused } from "./ide/searchStore";
import { ProblemsPanel } from "./ide/ProblemsPanel";
import { BlitTile } from "./ide/BlitTile";
import { tileDisplay } from "./ide/tileDisplay";
import {
  tabId,
  stripConn,
  registerTab,
  unregisterTab,
  resolveTab,
} from "./ide/tabRegistry";
import {
  allServerRoots,
  hasServerRoots,
  ensureServerRoots,
  addServerRoot,
  removeServerRoot,
  toggleServerRoot,
  reorderServerRoots,
} from "./ide/rootsStore";
import { useIdeSession, type IdeSessionDescriptor } from "./ide/session";
import { localFileIndex, searchFileIndex } from "./ide/fileIndex";
import { clearTileChrome } from "./ide/activeEditor";
import { editorRecencySnapshot } from "./ide/editorPositions";
import { SwitcherOverlay } from "./SwitcherOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { RemotesOverlay } from "./RemotesOverlay";
import { RootsOverlay } from "./RootsOverlay";
import { MediaOverlay } from "./MediaOverlay";
import { BSPContainer, EmptyPane } from "./bsp/BSPContainer";
import { WebOverlay } from "./WebOverlay";
import { WebPane, type WebPaneHandle } from "./WebPane";
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
import type { BSPAssignments, BSPLayout } from "./bsp/layout";
import {
  loadActiveLayout,
  loadAssignmentsFromHash,
  loadLayoutFromHash,
  saveActiveLayout,
  saveToHistory,
  removeFromHistory,
  loadRecentLayouts,
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
  layoutFromDSL,
  leafCount,
  loadFocusedTileFromHash,
} from "./bsp/layout";
import { setReveal } from "./ide/reveal";

export type Overlay =
  | "expose"
  | "palette"
  | "font"
  | "help"
  | "remotes"
  | "roots"
  | "media"
  | "web"
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

  // A terminal taking focus owns the status bar. Under BSP an editor tile
  // and a terminal are on screen together, so focus can leave the tile
  // without unmounting it — and a tile only clears its own chrome on
  // unmount, which would leave the bar showing the editor's filename and
  // its Save/Def/Refs buttons while the user types in a terminal. Focusing
  // a tile pane pushes a null session id, so this only fires for real
  // terminals and never races the tile's own focus registration.
  //
  // Memoized on the id, not read off the snapshot: snapshots churn on every
  // terminal frame, and clearing on each one would wipe the chrome an
  // editor had just registered.
  const focusedSessionId = createMemo(() => wsState().focusedSessionId);
  createEffect(() => {
    if (focusedSessionId()) clearTileChrome();
  });

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
  const [textGamma, setTextGamma] = createSignal(preferredTextGamma());
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
    queueMicrotask(() => focusedTerminalInput()?.focus());
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
  const [leftDockOpen, setLeftDockOpen] = createSignal(preferredLeftDockOpen());
  const [collapsedSections, setCollapsedSections] = createSignal<
    Set<LeftPanel>
  >(new Set(preferredCollapsedSections() as LeftPanel[]));
  const [sectionWeights, setSectionWeights] = createSignal<
    Record<LeftPanel, number>
  >({ explorer: 1, log: 1, problems: 1 });
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
  // The picker's list: per-server roots for kv connections, gateway entries
  // only for targets that don't have server-side roots (avoids doubling
  // seeded entries).
  const roots = createMemo<Root[]>(() => [
    ...allServerRoots(),
    ...gatewayRoots().filter(
      (r) => !hasServerRoots(connectionForRemote(r.remote)),
    ),
  ]);
  type RootSelection = { kind: "focused" } | { kind: "declared"; name: string };
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
    cwd: string;
  } | null>(null);
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
    return term ? { kind: "terminal", session: term } : null;
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
    const sel = rootSel();
    if (sel.kind === "declared") {
      const r = roots().find((x) => x.name === sel.name && !x.disabled);
      if (!r) return null;
      const connectionId = connectionForRemote(r.remote);
      return { key: `d ${connectionId} ${r.path}`, connectionId, path: r.path };
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
      return {
        key: `f ${a.session.connectionId} pty${a.session.ptyId}`,
        connectionId: a.session.connectionId,
        path: "",
        fromSessionId: a.session.id,
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

  // Keyboard open when visualViewport shrinks >150px from its baseline.
  const keyboardOpen = createMemo(() => {
    if (!isMobileTouch()) return false;
    const h = vpHeight();
    const full = vpBaseHeight();
    if (h === null || full === 0) return false;
    return full - h > 150;
  });

  // Sticky virtual keyboard: track explicit user intent so the keyboard
  // isn't dismissed when tapping elsewhere on the page.
  const [keyboardWanted, setKeyboardWanted] = createSignal(false);
  const terminalInputSelector =
    'textarea[aria-label="Terminal input"][tabindex]:not([readonly])';

  function focusedTerminalInput(): HTMLElement | null {
    const focusedPane = document.querySelector<HTMLElement>(
      '[data-blit-bsp-focused="true"]',
    );
    if (focusedPane) {
      return focusedPane.querySelector<HTMLElement>(terminalInputSelector);
    }
    if (document.querySelector("[data-blit-bsp-pane-id]")) return null;
    return document.querySelector<HTMLElement>(
      `section ${terminalInputSelector}`,
    );
  }

  function focusSettledElsewhere(): boolean {
    const active = document.activeElement;
    if (!(active instanceof HTMLElement)) return false;
    if (active.matches(terminalInputSelector)) return true;
    if (!active.closest("section")) return false;
    return active.matches("input, textarea, select, canvas[tabindex]");
  }

  // Re-focus the terminal textarea when it blurs while the user wants
  // the keyboard open, unless an overlay is active.
  createEffect(() => {
    if (!isMobileTouch() || !keyboardWanted()) return;
    const handler = (e: FocusEvent) => {
      if (!(e.target instanceof HTMLTextAreaElement)) return;
      if (!e.target.matches(terminalInputSelector)) return;
      if (!(e.target as Element).closest?.("section")) return;
      if (overlay()) return;
      setTimeout(() => {
        if (!keyboardWanted() || overlay()) return;
        if (focusSettledElsewhere()) return;
        focusedTerminalInput()?.focus();
      }, 50);
    };
    document.addEventListener("focusout", handler, true);
    onCleanup(() => document.removeEventListener("focusout", handler, true));
  });

  /** Toggle the virtual keyboard on mobile. */
  function toggleMobileKeyboard() {
    if (keyboardWanted()) {
      setKeyboardWanted(false);
      const active = document.activeElement;
      if (
        active instanceof HTMLElement &&
        active.matches(terminalInputSelector)
      ) {
        active.blur();
      } else {
        focusedTerminalInput()?.blur();
      }
    } else {
      const el = focusedTerminalInput();
      if (!el) return;
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
  let serverFontsLoaded = false;
  let serverFontsRequest: Promise<void> | null = null;

  function loadServerFonts(): void {
    if (serverFontsLoaded || serverFontsRequest) return;

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
    DEFAULT_FONT,
  );
  const [activeLayout, setActiveLayout] = createSignal<BSPLayout | null>(
    loadActiveLayout(),
  );
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
  // Backgrounded IDE tiles (Ctrl+Shift+Q), most-recent first. Recoverable from
  // the Cmd+K/expose switcher. Session-only (not persisted across reload).
  const [backgroundTiles, setBackgroundTiles] = createSignal<string[]>([]);
  // Auto-parking pushes one entry per file navigated past, so the list is
  // LRU-capped — an unbounded dock also meant unbounded live fs syncs,
  // which is how BLIT_FS_MAX_SYNCS got exhausted in normal browsing.
  const BACKGROUND_TILES_MAX = 50;
  // Only the most recent cards render as live tiles (each live editor holds
  // a content sync of its parent dir); the rest are title-only.
  const LIVE_DOCK_PREVIEWS = 6;
  function pushBackgroundTile(assignment: string) {
    setBackgroundTiles((prev) =>
      [assignment, ...prev.filter((a) => a !== assignment)].slice(
        0,
        BACKGROUND_TILES_MAX,
      ),
    );
  }
  // One prev/next pass over "every open tile" serves three jobs (the union of
  // pane assignments, the non-BSP active tile, and the background dock):
  //
  //  - displacement: a pane reassigned tile→terminal/surface sends the tile
  //    to the recoverable background list (tile→tile switches are left alone,
  //    and a tile still shown elsewhere is never backgrounded);
  //  - registration: a tile ENTERING the union is written to the server's
  //    tabs/ registry (docs/design/kv.md) so hash refs resolve anywhere;
  //  - deletion: a tile LEAVING the union entirely is unregistered.
  //
  // Gated on hash resolution (and the pending tile= ref) so boot churn never
  // writes: the prev-set starts empty, so nothing is ever deleted spuriously.
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
    const union = new Set<string>();
    for (const v of Object.values(next)) {
      if (
        typeof v === "string" &&
        (isTileAssignment(v) || isWebAssignment(v))
      ) {
        union.add(v);
      }
    }
    const at = activeTile();
    if (at && (isTileAssignment(at) || isWebAssignment(at))) union.add(at);
    for (const b of backgroundTiles()) union.add(b);
    if (la) {
      for (const [paneId, prev] of Object.entries(prevPaneAssignments)) {
        // A displaced web pane parks in the dock like a displaced tile: the
        // location is cheap to reopen but the pane's history is not, and
        // silently dropping it loses where you had navigated to.
        if (typeof prev === "string" && isWebAssignment(prev)) {
          const now = next[paneId];
          if (now !== prev && typeof now === "string" && !union.has(prev)) {
            pushBackgroundTile(prev);
            union.add(prev);
          }
          continue;
        }
        if (typeof prev !== "string" || !isTileAssignment(prev)) continue;
        const now = next[paneId];
        const nowIsTile = typeof now === "string" && isTileAssignment(now);
        // Displaced by a terminal/surface, or REPLACED by a tile of a
        // different file (clicking a diff over an editor): park it in the
        // dock. Same-file view switches stay in place, and a pane simply
        // cleared (layout teardown) backgrounds nothing.
        if (
          now !== prev &&
          typeof now === "string" &&
          !union.has(prev) &&
          (!nowIsTile || !sameTileFile(prev, now))
        ) {
          pushBackgroundTile(prev);
          union.add(prev); // stays open (in the dock) — do not unregister
        }
      }
    }
    // The non-BSP flavor of the same rule: a new tile replacing the active
    // tile parks the old one (dismissal to null stays a dismissal).
    if (
      prevActiveTile &&
      at &&
      at !== prevActiveTile &&
      isTileAssignment(at) &&
      !union.has(prevActiveTile) &&
      !sameTileFile(prevActiveTile, at)
    ) {
      pushBackgroundTile(prevActiveTile);
      union.add(prevActiveTile);
    }
    // Same rule in the fullscreen slot, where either side may be a web pane.
    if (
      prevActiveTile &&
      at &&
      at !== prevActiveTile &&
      (isWebAssignment(prevActiveTile) || isWebAssignment(at)) &&
      isWebAssignment(prevActiveTile) &&
      !union.has(prevActiveTile)
    ) {
      pushBackgroundTile(prevActiveTile);
      union.add(prevActiveTile);
    }
    // Web panes are registered like every other tab. They used to be
    // skipped, on the belief that their URL rode in the hash — but the hash
    // writer emits `w:<conn>:<tabId>`, a *reference* to the KV record
    // (docs/design/kv.md). Skipping registration left that reference
    // dangling, so a web pane resolved to nothing and vanished on reload.
    for (const a of union) {
      if (!prevOpenTiles.has(a)) registerTab(workspace, a);
    }
    for (const a of prevOpenTiles) {
      if (!union.has(a)) unregisterTab(workspace, a);
    }
    prevPaneAssignments = { ...next };
    // Remember a web pane here too, or the displacement rule below can never
    // see one leave the fullscreen slot.
    prevActiveTile =
      at && (isTileAssignment(at) || isWebAssignment(at)) ? at : null;
    prevOpenTiles = union;
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
      const fromHash = loadLayoutFromHash();
      if (fromHash && fromHash.dsl !== activeLayout()?.dsl) {
        setActiveLayout(fromHash);
      } else if (!fromHash && activeLayout()) {
        setActiveLayout(null);
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
  function persistCollapsed(next: Set<LeftPanel>) {
    writeStorage(LEFT_COLLAPSED_KEY, [...next].join(","));
  }
  function toggleLeftDock() {
    const next = !leftDockOpen();
    setLeftDockOpen(next);
    writeStorage(LEFT_DOCK_OPEN_KEY, next ? "1" : "0");
  }
  function toggleSectionCollapse(panel: LeftPanel) {
    setCollapsedSections((cur) => {
      const next = new Set(cur);
      if (next.has(panel)) next.delete(panel);
      else next.add(panel);
      persistCollapsed(next);
      return next;
    });
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

  function panelBody(panel: LeftPanel): JSX.Element {
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
    const value = () => {
      const s = rootSel();
      return s.kind === "declared" ? s.name : "__focused__";
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
          value={value()}
          onChange={(e) => {
            const v = e.currentTarget.value;
            setRootSel(
              v === "__focused__"
                ? { kind: "focused" }
                : { kind: "declared", name: v },
            );
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
          <For each={declared()}>
            {(r) => <option value={r.name}>{r.name}</option>}
          </For>
        </select>
        <button
          onClick={() => toggleOverlay("roots")}
          title="Manage workspace roots"
          style={{
            ...ui.btn,
            "flex-shrink": 0,
            "font-size": `${chromeScale().sm}px`,
            padding: `0 ${chromeScale().tightGap}px`,
            opacity: 0.7,
          }}
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
  const NAV_NONBSP = " non-bsp"; // the non-BSP activeTile slot
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
  // A tile shown in the main view / a pane must leave the background dock: it's
  // foreground now, and the same file shouldn't be live in both the dock and a
  // pane at once. (No-op ref when it wasn't parked, so opens/navigations that
  // never touch the dock don't churn backgroundTiles subscribers.)
  function evictFromBackground(assignment: string) {
    setBackgroundTiles((prev) =>
      prev.includes(assignment) ? prev.filter((a) => a !== assignment) : prev,
    );
  }
  // Place a tile into a pane without recording history (a history move itself).
  const placeTile = (assignment: string, paneId: string | null) => {
    evictFromBackground(assignment);
    if (navKeyFor(paneId) === NAV_NONBSP) {
      if (activeLayout()) {
        setActiveLayout(null);
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
  // Navigation handles by pane, published by each WebPane for the status bar.
  const [webHandles, setWebHandles] = createSignal<
    Record<string, WebPaneHandle>
  >({});

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
    const paneId = inBsp() ? (bspFocusedPaneId() ?? "") : NAV_NONBSP;
    const handle = webHandles()[paneId];
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
    evictFromBackground(assignment);
    if (inBsp()) {
      const paneId = preferredTilePane();
      recordNav(paneId, assignment);
      if (moveToPaneFn) moveToPaneFn(assignment, paneId);
      else pendingTilePlacement = { assignment, paneId };
      return;
    }
    recordNav(NAV_NONBSP, assignment);
    if (activeLayout()) {
      setActiveLayout(null);
      saveActiveLayout(null); // persist, or a remount resurrects it
    }
    setActiveTile(assignment);
  }

  // Drop a dragged tile into a specific BSP pane (records nav history there).
  function dropTileIntoPane(assignment: string, paneId: string) {
    evictFromBackground(assignment);
    recordNav(paneId, assignment);
    if (moveToPaneFn) moveToPaneFn(assignment, paneId);
    else pendingTilePlacement = { assignment, paneId };
  }

  // Send the currently-focused IDE tile to the recoverable background list
  // (Ctrl+Shift+Q). Handles both the non-BSP focused tile and a tile occupying
  // the focused BSP pane. Returns true if a tile was backgrounded (so the
  // keyboard handler knows it consumed the key).
  function backgroundFocusedTile(): boolean {
    const tile = activeTile();
    if (tile) {
      pushBackgroundTile(tile);
      setActiveTile(null);
      return true;
    }
    const paneId = bspFocusedPaneId();
    if (activeLayout() && paneId) {
      const assign = layoutAssignments()?.assignments[paneId] ?? null;
      if (assign && isTileAssignment(assign)) {
        pushBackgroundTile(assign);
        clearPaneAssignmentFn?.(paneId);
        return true;
      }
    }
    return false;
  }

  /**
   * Close the focused tile outright — the Ctrl+Alt+Shift+Q counterpart to
   * {@link backgroundFocusedTile}'s Ctrl+Shift+Q. Same targets (a non-BSP
   * active tile, or a tile in the focused BSP pane), but the assignment is
   * dropped instead of parked in the dock, matching what the same chord
   * does to a terminal or a surface.
   */
  function closeFocusedTile(): boolean {
    if (activeTile()) {
      setActiveTile(null);
      return true;
    }
    const paneId = bspFocusedPaneId();
    if (activeLayout() && paneId) {
      const assign = layoutAssignments()?.assignments[paneId] ?? null;
      if (assign && isTileAssignment(assign)) {
        clearPaneAssignmentFn?.(paneId);
        return true;
      }
    }
    return false;
  }

  // Restore a backgrounded tile: remove it from the list and re-open it in the
  // main view / focused pane. (openTile also evicts it from the dock.)
  function restoreTile(assignment: string) {
    openTile(assignment);
  }
  // Dismiss a backgrounded tile from the dock without re-opening it — the ✕ on a
  // background-editor card. It leaves backgroundTiles, so its live dock tile
  // unmounts (fs-sync/LSP torn down); nothing else references it.
  function closeBackgroundTile(assignment: string) {
    setBackgroundTiles((prev) => prev.filter((a) => a !== assignment));
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
    const s = remoteTextGamma();
    if (!s) return;
    const n = Number(s);
    if (Number.isFinite(n) && n >= 0.5 && n <= 2.5) setTextGamma(n);
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
    const sessionFocused = al ? bspHasSession : focusedSurfaceId() == null;
    const fs = sessionFocused ? focusedSession() : null;
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
    const value = family.trim() || DEFAULT_FONT;
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

  function switchSession(sessionId: SessionId) {
    focusSessionFromUi(sessionId);
    previousFocus = null;
    closeOverlay();
  }

  function focusSessionFromUi(sessionId: SessionId) {
    focusSurfaceById(null);
    setActiveTile(null); // focusing a terminal dismisses a non-BSP tile
    if (activeLayout()) {
      focusBySessionFn?.(sessionId);
    }
    workspace.focusSession(sessionId);
  }

  function focusSurface(surfaceId: number, connectionId?: ConnectionId) {
    setActiveTile(null); // focusing a surface dismisses a non-BSP tile
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

  const autoShownSurfaceKeys = new Set<string>();
  let autoShowSurfacesPrimed = false;
  let pendingAutoShowSurfaceKey: string | null = null;
  const surfaceKey = (surface: BlitSurface) =>
    `${surface.connectionId}:${surface.surfaceId}`;

  function surfaceVisibleInWorkspace(surface: BlitSurface): boolean {
    const key = surfaceKey(surface);
    if (
      surface.surfaceId === focusedSurfaceId() &&
      (focusedSurfaceConnId() == null ||
        surface.connectionId === focusedSurfaceConnId())
    ) {
      return true;
    }
    const la = layoutAssignments();
    if (!la) return false;
    for (const value of Object.values(la.assignments)) {
      const parsed = parseSurfaceAssignment(value);
      if (parsed && `${parsed.connectionId}:${parsed.surfaceId}` === key) {
        return true;
      }
    }
    return false;
  }

  createEffect(() => {
    const streaming = surfaceStreaming();
    const topLevelSurfaces = surfaces().filter((s) => s.parentId === 0);
    if (!streaming) return;

    const byKey = new Map(topLevelSurfaces.map((s) => [surfaceKey(s), s]));
    for (const key of autoShownSurfaceKeys) {
      if (!byKey.has(key)) autoShownSurfaceKeys.delete(key);
    }

    if (!autoShowSurfacesPrimed) {
      for (const key of byKey.keys()) autoShownSurfaceKeys.add(key);
      autoShowSurfacesPrimed = true;
      const candidate = [...topLevelSurfaces]
        .reverse()
        .find((surface) => !surfaceVisibleInWorkspace(surface));
      if (candidate) pendingAutoShowSurfaceKey = surfaceKey(candidate);
    } else {
      const added = topLevelSurfaces.filter(
        (surface) => !autoShownSurfaceKeys.has(surfaceKey(surface)),
      );
      if (added.length > 0) {
        for (const surface of added) {
          autoShownSurfaceKeys.add(surfaceKey(surface));
        }
        const candidate = [...added]
          .reverse()
          .find((surface) => !surfaceVisibleInWorkspace(surface));
        if (candidate) pendingAutoShowSurfaceKey = surfaceKey(candidate);
      }
    }

    if (!pendingAutoShowSurfaceKey) return;
    const surface = byKey.get(pendingAutoShowSurfaceKey);
    if (!surface) {
      pendingAutoShowSurfaceKey = null;
      return;
    }
    if (surfaceVisibleInWorkspace(surface)) {
      pendingAutoShowSurfaceKey = null;
      return;
    }

    if (activeLayout()) {
      const paneId = bspFocusedPaneId();
      if (!paneId || !layoutAssignments() || !assignmentsResolved()) return;
      moveToPaneFn?.(
        surfaceAssignment(surface.connectionId, surface.surfaceId),
        paneId,
      );
      focusSurfaceById(null);
    } else {
      focusSurfaceById(surface.surfaceId, surface.connectionId);
    }
    pendingAutoShowSurfaceKey = null;
  });

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
      setActiveTile(null); // creating a terminal dismisses a non-BSP tile
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
    focusBySession: (sessionId) => {
      focusSessionFromUi(sessionId);
    },
    clearFocusedPaneAssignment: () => {
      const paneId = bspFocusedPaneId();
      if (paneId) clearPaneAssignmentFn?.(paneId);
    },
    backgroundFocusedTile,
    closeFocusedTile,
    resetAudio,
    navigateBack: () => navigateHistory("back"),
    navigateForward: () => navigateHistory("forward"),
  });

  // Follow the focused terminal's cwd: poll it and expand the Explorer tree so
  // a `cd` reveals the directory. Server reads the pty cwd (no OSC-7 needed).
  // The same poll feeds the root-picker label (conn:cwd), so it runs whenever a
  // terminal is focused — not only when an IDE root is active.
  let lastFollowedCwd = "";
  const pollFocusedCwd = () => {
    const fid = wsState().focusedSessionId;
    if (!fid) {
      setFocusedTerm(null);
      return;
    }
    const connId = wsState().sessions.find((x) => x.id === fid)?.connectionId;
    if (!connId) {
      setFocusedTerm(null);
      return;
    }
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
        setFocusedTerm({ sessionId: fid, conn: connId, cwd });
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
    const existing = location.hash.slice(1);
    // Strip layout-managed keys (l, p, a) from the old hash only when we
    // have fresh values to replace them.  While BSPContainer is still
    // resolving hash assignments (assignmentsResolved is false), keep
    // the existing `a=` (and `p=`) so the original shareable hash
    // survives until resolution completes.
    const written = new Set(parts.map((p) => p.slice(0, p.indexOf("="))));
    written.add("l");
    if (paneId) written.add("p");
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
  const showMobileToolbar = createMemo(
    () => isMobileTouch() && keyboardWanted(),
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
          // While the virtual keyboard is open, pin to the visual viewport so
          // content is not hidden.  When closed, let the 100dvh root size the
          // app natively to avoid double-counting keyboard/browser-chrome space.
          ...(isMobileTouch() && keyboardOpen() && vpHeight()
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
        <section
          style={{
            ...layout.termContainer,
            display: "flex",
            "flex-direction": "row",
          }}
        >
          <Show when={leftDockOpen()}>
            <LeftDock
              collapsed={collapsedSections()}
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
            <div style={{ flex: 1, overflow: "hidden", position: "relative" }}>
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
                                    <BlitTerminal
                                      sessionId={fid()}
                                      onRender={countFrame}
                                      style={{ width: "100%", height: "100%" }}
                                      fontFamily={resolvedFontWithFallback()}
                                      fontSize={fontSize()}
                                      palette={palette()}
                                      surfaceRef={setTerminalSurface}
                                    />
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
                              theme={theme()}
                              palette={palette()}
                              scale={chromeScale()}
                              fontFamily={resolvedFontWithFallback()}
                              fontSize={fontSize()}
                              onOpenTile={openTile}
                            />
                            <button
                              onClick={() => setActiveTile(null)}
                              title="Close"
                              style={{
                                position: "absolute",
                                top: `${chromeScale().gap}px`,
                                right: `${chromeScale().gap}px`,
                                "z-index": z.exitedBanner,
                                ...ui.btn,
                              }}
                            >
                              {"✕"}
                            </button>
                          </div>
                        )}
                      </Show>
                    }
                  >
                    {(web) => (
                      <div
                        style={{
                          width: "100%",
                          height: "100%",
                          position: "relative",
                        }}
                      >
                        <WebPane
                          dest={web().connectionId}
                          url={web().url}
                          focus
                          onHandle={(handle) =>
                            setWebHandles((prev) => ({
                              ...prev,
                              [NAV_NONBSP]: handle,
                            }))
                          }
                        />
                        <button
                          onClick={() => setActiveTile(null)}
                          title="Close"
                          style={{
                            position: "absolute",
                            top: `${chromeScale().gap}px`,
                            right: `${chromeScale().gap}px`,
                            "z-index": z.exitedBanner,
                            ...ui.btn,
                          }}
                        >
                          {"✕"}
                        </button>
                      </div>
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
                    onWebPaneHandle={(paneId, handle) =>
                      setWebHandles((prev) => ({ ...prev, [paneId]: handle }))
                    }
                    onDropTile={dropTileIntoPane}
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
          </div>
          <Show
            when={
              previewPanelOpen() &&
              (offScreenSessions().length > 0 ||
                offScreenSurfaces().length > 0 ||
                backgroundTiles().length > 0)
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
              onResize={persistPreviewPanelWidth}
              onClose={togglePreviewPanel}
              backgroundEditors={
                <For each={backgroundTiles()}>
                  {(assignment, index) => {
                    const d = tileDisplay(assignment);
                    return (
                      <div
                        style={{
                          "border-bottom": `1px solid ${theme().subtleBorder}`,
                          display: "flex",
                          "flex-direction": "column",
                          "flex-shrink": 0,
                        }}
                      >
                        <div
                          style={{
                            display: "flex",
                            "align-items": "center",
                            gap: `${chromeScale().tightGap}px`,
                            padding: `${chromeScale().controlY}px ${chromeScale().tightGap}px`,
                          }}
                        >
                          <button
                            onClick={() => restoreTile(assignment)}
                            title="Open in main view"
                            style={{
                              ...ui.btn,
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
                              {d.title}
                            </span>
                            <Show when={d.subtitle}>
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
                                {d.subtitle}
                              </span>
                            </Show>
                          </button>
                          <button
                            onClick={() => closeBackgroundTile(assignment)}
                            title="Close"
                            style={{
                              ...ui.btn,
                              "flex-shrink": 0,
                              "font-size": `${chromeScale().sm}px`,
                              padding: `0 ${chromeScale().tightGap}px`,
                              opacity: 0.5,
                            }}
                          >
                            {"✕"}
                          </button>
                        </div>
                        {/* Read-only zoomed-out preview, terminal-thumbnail
                            semantics: click to bring it back to the main
                            view. Only the most recent cards are live — a
                            mounted preview editor holds an fs sync, and
                            those are budgeted (LIVE_DOCK_PREVIEWS). */}
                        <Show when={index() < LIVE_DOCK_PREVIEWS}>
                          <div
                            onClick={() => restoreTile(assignment)}
                            title="Open in main view"
                            style={{
                              position: "relative",
                              width: "100%",
                              height: `${Math.min(240, Math.max(120, Math.round(fontSize() * 12)))}px`,
                              overflow: "hidden",
                              "background-color": theme().bg,
                              cursor: "pointer",
                            }}
                          >
                            {/* Inert content: pointer-events none keeps every
                              wheel/click on the card itself, so a dock pane
                              can't be scrolled or interacted with — a click
                              only ever restores it. */}
                            <div
                              style={{
                                position: "absolute",
                                inset: 0,
                                "pointer-events": "none",
                              }}
                            >
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
                                preview
                              />
                            </div>
                          </div>
                        </Show>
                      </div>
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
              onOpenWeb={() => toggleOverlay("web")}
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
                if (s) openTile(s.fileAssignment(relPath));
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
              onToggle={(name) => toggleRemote(name)}
              onSetDefault={(name) => setDefaultRemote(name)}
              onReorder={(names) => reorderRemotes(names)}
              onReconnect={(name) => workspace.reconnectConnection(name)}
              onClose={closeOverlay}
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
            sessions={sessions()}
            surfaceCount={surfaces().length}
            tileCount={paneKindCount(isTileAssignment)}
            webCount={paneKindCount(isWebAssignment)}
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
            onRemotes={() => toggleOverlay("remotes")}
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
            onRoots={() => toggleOverlay("roots")}
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
            hasSurfaces={surfaces().length > 0}
            isMobileTouch={isMobileTouch()}
            keyboardOpen={keyboardWanted()}
            onToggleKeyboard={toggleMobileKeyboard}
            onMedia={() => toggleOverlay("media")}
          />
        </footer>
        <Show when={showMobileToolbar()}>
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
  /** Live background-editor cards (rendered by WorkspaceScreen, which owns the
   *  tile assignments), shown above the terminal/surface thumbnails. */
  backgroundEditors?: JSX.Element;
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
    // Cap the panel at a fraction of the viewport so a touch drag can't
    // push the terminal off-screen.
    const maxWidth = Math.max(
      MIN_PANEL_WIDTH,
      Math.floor(window.innerWidth * 0.85),
    );

    const onMove = (me: PointerEvent) => {
      const delta = startX - me.clientX;
      props.onResize(
        Math.min(maxWidth, Math.max(MIN_PANEL_WIDTH, startWidth + delta)),
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
