import { useState, useCallback, useEffect, useRef } from "react";
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  BlitWorkspace,
  useBlitConnection,
  useBlitFocusedSession,
  useBlitSessions,
  useBlitWorkspace,
  useBlitWorkspaceState,
  DEFAULT_FONT,
} from "blit-react";
import type {
  BlitTransport,
  BlitTerminalHandle,
  BlitSession,
  BlitWasmModule,
  SessionId,
  TerminalPalette,
} from "blit-react";
import { useMetrics } from "./useMetrics";
import {
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  writeStorage,
  preferredPalette,
  preferredFont,
  preferredFontSize,
  blitHost,
  basePath,
} from "./storage";
import type { UIScale } from "./theme";
import { themeFor, layout, ui, uiScale, z } from "./theme";
import { t } from "./i18n";
import { StatusBar } from "./StatusBar";
import { SwitcherOverlay } from "./SwitcherOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { DisconnectedOverlay } from "./DisconnectedOverlay";
import { BSPContainer } from "./bsp/BSPContainer";
import type { BSPAssignments, BSPLayout } from "./bsp/layout";
import {
  loadActiveLayout,
  saveActiveLayout,
  saveToHistory,
  loadRecentLayouts,
  PRESETS,
} from "./bsp/layout";

export type Overlay = "expose" | "palette" | "font" | "help" | null;

const PRIMARY_CONNECTION_ID = "main";

// Module-level cache so the workspace and connection survive HMR.
let cachedWorkspace: BlitWorkspace | null = null;
let cachedTransport: BlitTransport | null = null;

const CSS_GENERIC = new Set([
  "serif",
  "sans-serif",
  "monospace",
  "cursive",
  "fantasy",
  "system-ui",
  "ui-serif",
  "ui-sans-serif",
  "ui-monospace",
  "ui-rounded",
  "math",
  "emoji",
  "fangsong",
]);

function splitFontFamilies(value: string): string[] {
  return value
    .split(",")
    .map((family) => family.trim().replace(/^['"]|['"]$/g, ""))
    .filter(Boolean);
}

function fontStyleId(family: string): string {
  return `blit-font-${family.replace(/\s+/g, "-").toLowerCase()}`;
}

export function Workspace({
  transport,
  wasm,
  onAuthError,
}: {
  transport: BlitTransport;
  wasm: BlitWasmModule;
  onAuthError: () => void;
}) {
  if (!cachedWorkspace) {
    cachedWorkspace = new BlitWorkspace({ wasm });
  }
  const workspace = cachedWorkspace;

  if (cachedTransport !== transport) {
    if (cachedTransport) {
      workspace.removeConnection(PRIMARY_CONNECTION_ID);
    }
    cachedTransport = transport;
    workspace.addConnection({
      id: PRIMARY_CONNECTION_ID,
      transport,
    });
  }

  return (
    <BlitWorkspaceProvider workspace={workspace}>
      <WorkspaceScreen
        transport={transport}
        primaryConnectionId={PRIMARY_CONNECTION_ID}
        onAuthError={onAuthError}
      />
    </BlitWorkspaceProvider>
  );
}

function WorkspaceScreen({
  transport,
  primaryConnectionId,
  onAuthError,
}: {
  transport: BlitTransport;
  primaryConnectionId: string;
  onAuthError: () => void;
}) {
  const workspace = useBlitWorkspace();
  const workspaceState = useBlitWorkspaceState();
  const sessions = useBlitSessions();
  const focusedSession = useBlitFocusedSession();
  const connection = useBlitConnection(primaryConnectionId);

  const [palette, setPalette] = useState<TerminalPalette>(preferredPalette);
  const [font, setFont] = useState(preferredFont);
  const [resolvedFont, setResolvedFont] = useState(preferredFont);
  const [fontSize, setFontSize] = useState(preferredFontSize);
  const [overlay, setOverlay] = useState<Overlay>(null);
  const [debugPanel, setDebugPanel] = useState(false);
  const [serverFonts, setServerFonts] = useState<string[]>([]);
  const [offlineVisible, setOfflineVisible] = useState(false);
  const [fontLoading, setFontLoading] = useState(false);
  const [advanceRatio, setAdvanceRatio] = useState<number | undefined>(
    undefined,
  );
  const [activeLayout, setActiveLayout] = useState<BSPLayout | null>(
    loadActiveLayout,
  );
  const activeLayoutRef = useRef(activeLayout);
  activeLayoutRef.current = activeLayout;
  const [layoutAssignments, setLayoutAssignments] =
    useState<BSPAssignments | null>(null);
  const [pendingPaneTargetId, setPendingPaneTargetId] = useState<string | null>(
    null,
  );
  const toggleDebug = useCallback(() => setDebugPanel((value) => !value), []);
  const fontRequestVersionRef = useRef(0);
  const paletteOverlayOriginRef = useRef<TerminalPalette | null>(null);
  const fontOverlayOriginRef = useRef<{ family: string; size: number } | null>(
    null,
  );

  const resolvedFontWithFallback =
    resolvedFont === DEFAULT_FONT
      ? resolvedFont
      : `${resolvedFont}, ${DEFAULT_FONT}`;

  useEffect(() => {
    fetch(`${basePath}fonts`)
      .then((response) => (response.ok ? response.json() : []))
      .then(setServerFonts)
      .catch(() => {});
  }, []);

  const termRef = useRef<BlitTerminalHandle | null>(null);
  const overlayRef = useRef<Overlay>(null);
  overlayRef.current = overlay;

  const stateRef = useRef<{
    focusedSessionId: SessionId | null;
    sessions: readonly BlitSession[];
    supportsRestart: boolean;
  }>({
    focusedSessionId: null,
    sessions: [],
    supportsRestart: false,
  });
  stateRef.current = {
    focusedSessionId: workspaceState.focusedSessionId,
    sessions,
    supportsRestart: connection?.supportsRestart ?? false,
  };

  const { countFrame, timelineRef, netRef, ...metrics } = useMetrics(transport);
  const dark = palette.dark;
  const theme = themeFor(palette);

  useEffect(() => {
    const requestedFont = font.trim() || DEFAULT_FONT;
    const families = splitFontFamilies(requestedFont).filter(
      (family) => !CSS_GENERIC.has(family.toLowerCase()),
    );
    const requestVersion = ++fontRequestVersionRef.current;
    let cancelled = false;

    if (families.length === 0) {
      setResolvedFont(requestedFont);
      setAdvanceRatio(undefined);
      setFontLoading(false);
      return () => {
        cancelled = true;
      };
    }

    setFontLoading(true);

    const loadRequestedFont = async () => {
      let ratio: number | undefined;
      for (const family of families) {
        if (cancelled || requestVersion !== fontRequestVersionRef.current)
          return;

        const loadSpec = `16px "${family}"`;
        const id = fontStyleId(family);
        if (!document.getElementById(id)) {
          try {
            const response = await fetch(
              `${basePath}font/${encodeURIComponent(family)}`,
            );
            if (response.ok) {
              const css = await response.text();
              if (cancelled || requestVersion !== fontRequestVersionRef.current)
                return;
              if (!document.getElementById(id)) {
                const style = document.createElement("style");
                style.id = id;
                style.textContent = css;
                document.head.appendChild(style);
              }
            }
          } catch {}
        }

        if (ratio == null) {
          try {
            const metricsResp = await fetch(
              `${basePath}font-metrics/${encodeURIComponent(family)}`,
            );
            if (metricsResp.ok) {
              const json = await metricsResp.json();
              if (typeof json.advanceRatio === "number")
                ratio = json.advanceRatio;
            }
          } catch {}
        }

        try {
          if (typeof document.fonts?.load === "function") {
            await document.fonts.load(loadSpec, "BESbswy");
          } else if (document.fonts?.ready) {
            await document.fonts.ready;
          }
        } catch {}
      }

      if (cancelled || requestVersion !== fontRequestVersionRef.current) return;
      setAdvanceRatio(ratio);
      setResolvedFont(requestedFont);
      setFontLoading(false);
    };

    void loadRequestedFont();

    return () => {
      cancelled = true;
    };
  }, [font]);

  const lruRef = useRef<SessionId[]>([]);

  useEffect(() => {
    if (!workspaceState.focusedSessionId) return;
    const lru = lruRef.current.filter(
      (id) => id !== workspaceState.focusedSessionId,
    );
    lru.unshift(workspaceState.focusedSessionId);
    lruRef.current = lru;
  }, [workspaceState.focusedSessionId]);

  useEffect(() => {
    if (activeLayout) return;
    setLayoutAssignments(null);
    setPendingPaneTargetId(null);
  }, [activeLayout]);

  useEffect(() => {
    if (activeLayout && overlay !== "expose") return;
    const desired = new Set<SessionId>();
    if (workspaceState.focusedSessionId) {
      desired.add(workspaceState.focusedSessionId);
    }
    if (overlay === "expose") {
      for (const session of sessions) {
        if (session.state !== "closed") desired.add(session.id);
      }
    }
    workspace.setVisibleSessions(desired);
  }, [
    activeLayout,
    overlay,
    sessions,
    workspace,
    workspaceState.focusedSessionId,
  ]);

  const hasConnectedRef = useRef(connection?.status === "connected");
  useEffect(() => {
    const status = connection?.status;
    if (!status) return;
    if (status === "connected") {
      hasConnectedRef.current = true;
    } else if (connection?.error === "auth") {
      onAuthError();
    }
  }, [connection?.status, connection?.error, onAuthError]);

  useEffect(() => {
    const status = connection?.status;
    if (!status) return;

    if (status === "connected") {
      setOfflineVisible(false);
      return;
    }

    if (
      hasConnectedRef.current ||
      (connection?.retryCount ?? 0) > 0 ||
      (connection?.error != null && connection.error !== "auth")
    ) {
      setOfflineVisible(true);
    }
  }, [connection?.status, connection?.retryCount, connection?.error]);

  const termCallbackRef = useCallback((handle: BlitTerminalHandle | null) => {
    termRef.current = handle;
    if (handle && !overlayRef.current) {
      handle.focus();
    }
  }, []);

  useEffect(() => {
    document.documentElement.setAttribute(
      "data-theme",
      dark ? "dark" : "light",
    );
  }, [dark]);

  useEffect(() => {
    document.documentElement.style.fontFamily = "system-ui, sans-serif";
  }, []);

  useEffect(() => {
    const host = blitHost();
    const parts: string[] = [];
    if (focusedSession?.title) parts.push(focusedSession.title);
    if (host && host !== "localhost" && host !== "127.0.0.1") parts.push(host);
    parts.push("blit");
    document.title = parts.join(" — ");
  }, [focusedSession?.title]);

  const previousFocusRef = useRef<Element | null>(null);

  const closeOverlay = useCallback(() => {
    paletteOverlayOriginRef.current = null;
    fontOverlayOriginRef.current = null;
    setOverlay(null);
    const el = previousFocusRef.current;
    previousFocusRef.current = null;
    if (el instanceof HTMLElement) {
      setTimeout(() => el.focus(), 0);
    }
  }, []);

  const restoreOverlayPreview = useCallback((target: Overlay) => {
    if (target === "palette") {
      const original = paletteOverlayOriginRef.current;
      if (original) {
        setPalette(original);
      }
      paletteOverlayOriginRef.current = null;
      return;
    }

    if (target === "font") {
      const original = fontOverlayOriginRef.current;
      if (original) {
        setFont(original.family);
        setFontSize(original.size);
      }
      fontOverlayOriginRef.current = null;
    }
  }, []);

  const cancelOverlay = useCallback(() => {
    restoreOverlayPreview(overlayRef.current);
    closeOverlay();
  }, [closeOverlay, restoreOverlayPreview]);

  const toggleOverlay = useCallback(
    (target: Overlay) => {
      const current = overlayRef.current;
      if (current === target) {
        cancelOverlay();
        return;
      }

      restoreOverlayPreview(current);

      if (!current) {
        previousFocusRef.current = document.activeElement;
      }

      if (target === "palette") {
        paletteOverlayOriginRef.current = palette;
      } else if (target === "font") {
        fontOverlayOriginRef.current = {
          family: font,
          size: fontSize,
        };
      }

      setOverlay(target);
    },
    [cancelOverlay, font, fontSize, palette, restoreOverlayPreview],
  );

  const changePalette = useCallback(
    (nextPalette: TerminalPalette) => {
      setPalette(nextPalette);
      paletteOverlayOriginRef.current = null;
      writeStorage(PALETTE_KEY, nextPalette.id);
      closeOverlay();
    },
    [closeOverlay],
  );

  const changeFont = useCallback(
    (family: string, size: number) => {
      const value = family.trim() || DEFAULT_FONT;
      setFont(value);
      setFontSize(size);
      fontOverlayOriginRef.current = null;
      writeStorage(FONT_KEY, value);
      writeStorage(FONT_SIZE_KEY, String(size));
      closeOverlay();
    },
    [closeOverlay],
  );

  const focusBySessionRef = useRef<((sessionId: SessionId) => void) | null>(
    null,
  );
  const moveSessionToPaneRef = useRef<
    ((sessionId: SessionId, targetPaneId: string) => void) | null
  >(null);
  const [bspFocusedPaneId, setBspFocusedPaneId] = useState<string | null>(null);

  const switchSession = useCallback(
    (sessionId: SessionId) => {
      workspace.focusSession(sessionId);
      focusBySessionRef.current?.(sessionId);
      closeOverlay();
    },
    [closeOverlay, workspace],
  );

  const createAndFocus = useCallback(
    async (command?: string) => {
      try {
        const session = await workspace.createSession({
          connectionId: primaryConnectionId,
          rows: termRef.current?.rows ?? 24,
          cols: termRef.current?.cols ?? 80,
          ...(command ? { command } : {}),
          ...(!command && workspaceState.focusedSessionId
            ? { cwdFromSessionId: workspaceState.focusedSessionId }
            : {}),
        });
        workspace.focusSession(session.id);
        closeOverlay();
      } catch (error) {
        if (
          connection?.status !== "disconnected" &&
          connection?.status !== "error"
        ) {
          console.error("blit: failed to create PTY", error);
        }
      }
    },
    [
      closeOverlay,
      connection?.status,
      primaryConnectionId,
      workspace,
      workspaceState.focusedSessionId,
    ],
  );

  const focusPaneRef = useRef<((paneId: string) => void) | null>(null);

  const createInPane = useCallback(
    async (paneId: string, command?: string) => {
      setPendingPaneTargetId(paneId);
      try {
        await workspace.createSession({
          connectionId: primaryConnectionId,
          rows: termRef.current?.rows ?? 24,
          cols: termRef.current?.cols ?? 80,
          ...(command ? { command } : {}),
          ...(!command && workspaceState.focusedSessionId
            ? { cwdFromSessionId: workspaceState.focusedSessionId }
            : {}),
        });
      } catch (error) {
        if (
          connection?.status !== "disconnected" &&
          connection?.status !== "error"
        ) {
          console.error("blit: failed to create PTY", error);
        }
      }
    },
    [
      connection?.status,
      primaryConnectionId,
      workspace,
      workspaceState.focusedSessionId,
    ],
  );

  const selectPane = useCallback(
    (paneId: string, sessionId: SessionId | null, command?: string) => {
      if (sessionId && !command) {
        workspace.focusSession(sessionId);
        focusBySessionRef.current?.(sessionId);
      } else {
        void createInPane(paneId, command);
      }
      closeOverlay();
    },
    [closeOverlay, createInPane, workspace],
  );

  const handleRestartOrClose = useCallback(() => {
    if (!focusedSession) {
      void createAndFocus();
      return;
    }
    if (focusedSession.state !== "exited") return;
    if (connection?.supportsRestart) {
      workspace.restartSession(focusedSession.id);
    } else {
      void workspace.closeSession(focusedSession.id);
    }
  }, [connection?.supportsRestart, createAndFocus, focusedSession, workspace]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;

      if (mod && !e.shiftKey && e.key === "k") {
        e.preventDefault();
        toggleOverlay("expose");
        return;
      }
      if (e.ctrlKey && e.shiftKey && (e.key === "?" || e.code === "Slash")) {
        e.preventDefault();
        toggleOverlay("help");
        return;
      }
      if (e.ctrlKey && e.shiftKey && (e.key === "~" || e.key === "`")) {
        e.preventDefault();
        toggleDebug();
        return;
      }
      if (mod && e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        if (bspFocusedPaneId) {
          void createInPane(bspFocusedPaneId);
        } else {
          void createAndFocus();
        }
        return;
      }
      if (
        e.key === "Enter" &&
        !mod &&
        !e.shiftKey &&
        !overlayRef.current &&
        !activeLayoutRef.current
      ) {
        const current = stateRef.current;
        const focused = current.focusedSessionId
          ? current.sessions.find(
              (session) => session.id === current.focusedSessionId,
            )
          : null;
        if (
          (focused && focused.state === "exited") ||
          current.focusedSessionId == null
        ) {
          e.preventDefault();
          handleRestartOrClose();
          return;
        }
      }
      if (mod && e.shiftKey && e.key === "W") {
        if (overlayRef.current) return;
        e.preventDefault();
        if (stateRef.current.focusedSessionId) {
          void workspace.closeSession(stateRef.current.focusedSessionId);
        }
        return;
      }
      if (mod && e.shiftKey && (e.key === "{" || e.key === "}")) {
        e.preventDefault();
        const visible = stateRef.current.sessions
          .filter((session) => session.state !== "closed")
          .map((session) => session.id);
        const currentId = stateRef.current.focusedSessionId;
        if (visible.length < 2 || !currentId) return;
        const index = visible.indexOf(currentId);
        const nextId =
          e.key === "}"
            ? visible[(index + 1) % visible.length]
            : visible[(index - 1 + visible.length) % visible.length];
        workspace.focusSession(nextId);
        return;
      }
      if (e.key === "Escape") {
        if (overlayRef.current) {
          e.preventDefault();
          cancelOverlay();
          return;
        }
        if (focusedSession?.state === "exited") {
          e.preventDefault();
          void workspace.closeSession(focusedSession.id);
        }
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [
    cancelOverlay,
    createAndFocus,
    focusedSession,
    handleRestartOrClose,
    toggleDebug,
    toggleOverlay,
    workspace,
  ]);

  // Set global font defaults on the connection store so new terminals get the right size.
  // Only update the defaults (for new terminal creation), don't propagate to existing
  // terminals — individual BlitTerminals handle their own font size including DPR.
  const conn = workspace.getConnection(primaryConnectionId);
  if (conn) {
    const dpr = window.devicePixelRatio || 1;
    conn.setFontSize(fontSize * dpr);
    conn.setFontFamily(resolvedFontWithFallback);
  }

  // Sync layout state to URL hash once connected (avoid clobbering
  // hash-loaded assignments before sessions have reconnected).
  useEffect(() => {
    if (connection?.status !== "connected") return;
    const parts: string[] = [];
    if (activeLayout) parts.push(`l=${activeLayout.dsl}`);
    if (bspFocusedPaneId) parts.push(`p=${bspFocusedPaneId}`);
    if (layoutAssignments) {
      const a = Object.entries(layoutAssignments.assignments)
        .filter(([, sid]) => sid != null)
        .map(([pane, sid]) => `${pane}:${sid}`)
        .join(",");
      if (a) parts.push(`a=${a}`);
    }
    history.replaceState(
      null,
      "",
      parts.length > 0 ? `#${parts.join("&")}` : location.pathname,
    );
  }, [activeLayout, bspFocusedPaneId, connection?.status, layoutAssignments]);

  const debugStats = workspace.getConnectionDebugStats(
    primaryConnectionId,
    workspaceState.focusedSessionId,
  );
  const chromeScale = uiScale(fontSize);

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette}
      fontFamily={resolvedFontWithFallback}
      fontSize={fontSize}
      advanceRatio={advanceRatio}
    >
      <main
        style={{
          ...layout.workspace,
          backgroundColor: theme.bg,
          color: theme.fg,
          fontFamily: resolvedFontWithFallback,
        }}
      >
        <section style={layout.termContainer}>
          {activeLayout ? (
            <BSPContainer
              layout={activeLayout}
              onLayoutChange={setActiveLayout}
              connectionId={primaryConnectionId}
              palette={palette}
              fontFamily={resolvedFontWithFallback}
              fontSize={fontSize}
              focusedSessionId={workspaceState.focusedSessionId}
              lruSessionIds={lruRef.current}
              manageVisibility={overlay !== "expose"}
              preferredEmptyPaneId={pendingPaneTargetId}
              onAssignmentsChange={setLayoutAssignments}
              onPreferredEmptyPaneResolved={() => setPendingPaneTargetId(null)}
              onFocusSession={(id) => workspace.focusSession(id)}
              onFocusBySession={(fn) => {
                focusBySessionRef.current = fn;
              }}
              onFocusPane={(fn) => {
                focusPaneRef.current = fn;
              }}
              onMoveSessionToPane={(fn) => {
                moveSessionToPaneRef.current = fn;
              }}
              onFocusedPaneChange={setBspFocusedPaneId}
              onCreateInPane={createInPane}
            />
          ) : workspaceState.focusedSessionId != null ? (
            <>
              <BlitTerminal
                ref={termCallbackRef}
                sessionId={workspaceState.focusedSessionId}
                onRender={countFrame}
                style={{ width: "100%", height: "100%" }}
                fontFamily={resolvedFontWithFallback}
                fontSize={fontSize}
                palette={palette}
              />
              {focusedSession?.state === "exited" && (
                <div
                  style={{
                    position: "absolute",
                    bottom: 32,
                    left: "50%",
                    transform: "translateX(-50%)",
                    backgroundColor: theme.solidPanelBg,
                    border: `1px solid ${theme.border}`,
                    padding: `${chromeScale.controlY}px ${chromeScale.controlX}px`,
                    fontSize: chromeScale.sm,
                    zIndex: z.exitedBanner,
                    display: "flex",
                    alignItems: "center",
                    gap: chromeScale.gap,
                  }}
                >
                  <mark
                    style={{
                      ...ui.badge,
                      backgroundColor: "rgba(255,100,100,0.3)",
                    }}
                  >
                    {t("workspace.exited")}
                  </mark>
                  {connection?.supportsRestart ? (
                    <button
                      onClick={() => handleRestartOrClose()}
                      style={{ ...ui.btn, fontSize: chromeScale.md }}
                    >
                      {t("workspace.restart")} <kbd style={ui.kbd}>Enter</kbd>
                    </button>
                  ) : null}
                  <button
                    onClick={() =>
                      void workspace.closeSession(focusedSession.id)
                    }
                    style={{
                      ...ui.btn,
                      fontSize: chromeScale.md,
                      opacity: 0.5,
                    }}
                  >
                    {t("workspace.close")} <kbd style={ui.kbd}>Esc</kbd>
                  </button>
                </div>
              )}
            </>
          ) : (
            <EmptyState
              theme={theme}
              scale={chromeScale}
              mod={/Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl"}
              onCreate={() => void createAndFocus()}
              onSwitcher={() => toggleOverlay("expose")}
              onHelp={() => toggleOverlay("help")}
            />
          )}
        </section>
        {overlay === "expose" && (
          <SwitcherOverlay
            sessions={sessions}
            focusedSessionId={workspaceState.focusedSessionId}
            lru={lruRef.current}
            palette={palette}
            fontFamily={resolvedFontWithFallback}
            fontSize={fontSize}
            onSelect={switchSession}
            onClose={closeOverlay}
            onCreate={createAndFocus}
            activeLayout={activeLayout}
            layoutAssignments={layoutAssignments}
            onSelectPane={selectPane}
            focusedPaneId={activeLayout ? bspFocusedPaneId : null}
            onMoveToPane={(sessionId, targetPaneId) => {
              moveSessionToPaneRef.current?.(sessionId, targetPaneId);
              workspace.focusSession(sessionId);
              closeOverlay();
            }}
            onApplyLayout={(l) => {
              setPendingPaneTargetId(null);
              setActiveLayout(l);
              saveActiveLayout(l);
              saveToHistory(l);
              closeOverlay();
            }}
            onClearLayout={() => {
              setPendingPaneTargetId(null);
              setLayoutAssignments(null);
              setActiveLayout(null);
              saveActiveLayout(null);
              closeOverlay();
            }}
            recentLayouts={loadRecentLayouts()}
            presetLayouts={PRESETS}
            onChangeFont={() => toggleOverlay("font")}
            onChangePalette={() => toggleOverlay("palette")}
          />
        )}
        {overlay === "palette" && (
          <PaletteOverlay
            current={palette}
            fontSize={fontSize}
            onSelect={changePalette}
            onPreview={setPalette}
            onClose={closeOverlay}
          />
        )}
        {overlay === "font" && (
          <FontOverlay
            currentFamily={font}
            currentSize={fontSize}
            serverFonts={serverFonts}
            palette={palette}
            fontSize={fontSize}
            onSelect={changeFont}
            onPreview={(family, size) => {
              setFont(family);
              setFontSize(size);
            }}
            onClose={closeOverlay}
          />
        )}
        {overlay === "help" && (
          <HelpOverlay
            onClose={closeOverlay}
            palette={palette}
            fontSize={fontSize}
          />
        )}
        {offlineVisible && (
          <DisconnectedOverlay
            palette={palette}
            fontSize={fontSize}
            status={connection?.status ?? "disconnected"}
            retryCount={connection?.retryCount ?? 0}
            error={connection?.error ?? null}
            onReconnect={() =>
              workspace.reconnectConnection(primaryConnectionId)
            }
          />
        )}
        <footer
          style={{
            ...layout.statusBar,
            padding: "0 1em",
            backgroundColor: theme.bg,
            borderTopColor: theme.subtleBorder,
            height: chromeScale.md + chromeScale.controlY * 2,
            fontSize: chromeScale.sm,
          }}
        >
          <StatusBar
            sessions={sessions}
            focusedSession={focusedSession}
            status={connection?.status ?? "disconnected"}
            metrics={metrics}
            palette={palette}
            fontSize={fontSize}
            termSize={
              termRef.current
                ? `${termRef.current.cols}x${termRef.current.rows}`
                : null
            }
            fontLoading={fontLoading}
            debug={debugPanel}
            toggleDebug={toggleDebug}
            debugStats={debugStats}
            timelineRef={timelineRef}
            netRef={netRef}
            onSwitcher={() => toggleOverlay("expose")}
            onPalette={() => toggleOverlay("palette")}
            onFont={() => toggleOverlay("font")}
          />
        </footer>
      </main>
    </BlitWorkspaceProvider>
  );
}

function EmptyState({
  theme,
  scale,
  mod,
  onCreate,
  onSwitcher,
  onHelp,
}: {
  theme: ReturnType<typeof themeFor>;
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
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        height: "100%",
        gap: scale.gap,
        opacity: 0.6,
      }}
    >
      <div style={{ fontSize: scale.lg }}>{t("workspace.welcome")}</div>
      <div
        style={{
          fontSize: scale.sm,
          display: "flex",
          flexDirection: "column",
          alignItems: "center",
          gap: scale.tightGap,
        }}
      >
        <button onClick={onCreate} style={{ ...ui.btn, fontSize: scale.md }}>
          {t("workspace.newTerminal")} <kbd style={ui.kbd}>Enter</kbd>{" "}
          <kbd style={ui.kbd}>{mod}+Shift+Enter</kbd>
        </button>
        <button onClick={onSwitcher} style={{ ...ui.btn, fontSize: scale.md }}>
          {t("workspace.menu")} <kbd style={ui.kbd}>{mod}+K</kbd>
        </button>
        <button onClick={onHelp} style={{ ...ui.btn, fontSize: scale.md }}>
          {t("workspace.help")} <kbd style={ui.kbd}>Ctrl+?</kbd>
        </button>
      </div>
      <button
        onClick={onCreate}
        style={{
          marginTop: scale.tightGap,
          padding: `${scale.controlY}px ${scale.controlX * 2}px`,
          fontSize: scale.md,
          backgroundColor: theme.accent,
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
