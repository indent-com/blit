import {
  useState,
  useCallback,
  useEffect,
  useRef,
} from "react";
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  createBlitWorkspace,
  useBlitConnection,
  useBlitFocusedSession,
  useBlitSessions,
  useBlitWorkspace,
  useBlitWorkspaceState,
  DEFAULT_FONT,
  CSS_GENERIC,
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
import { themeFor, layout, ui, z } from "./theme";
import { StatusBar } from "./StatusBar";
import { ExposeOverlay } from "./ExposeOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";
import { DisconnectedOverlay } from "./DisconnectedOverlay";

export type Overlay = "expose" | "palette" | "font" | "help" | null;

const PRIMARY_CONNECTION_ID = "default";

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
  const workspaceRef = useRef<ReturnType<typeof createBlitWorkspace> | null>(null);
  if (!workspaceRef.current) {
    workspaceRef.current = createBlitWorkspace({ wasm });
  }
  const workspace = workspaceRef.current;

  useEffect(() => {
    workspace.addConnection({
      id: PRIMARY_CONNECTION_ID,
      transport,
    });
    return () => {
      workspace.removeConnection(PRIMARY_CONNECTION_ID);
    };
  }, [workspace, transport]);

  useEffect(() => {
    return () => {
      workspace.dispose();
    };
  }, [workspace]);

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
  const toggleDebug = useCallback(() => setDebugPanel((value) => !value), []);
  const fontRequestVersionRef = useRef(0);
  const paletteOverlayOriginRef = useRef<TerminalPalette | null>(null);
  const fontOverlayOriginRef = useRef<{ family: string; size: number } | null>(null);

  const resolvedFontWithFallback =
    resolvedFont === DEFAULT_FONT ? resolvedFont : `${resolvedFont}, ${DEFAULT_FONT}`;

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

  const initialHashPtyIdRef = useRef<number | null>((() => {
    const hash = location.hash.substring(1);
    const id = parseInt(hash, 10);
    return Number.isFinite(id) && id >= 0 ? id : null;
  })());
  const initialHashAppliedRef = useRef(false);

  useEffect(() => {
    if (initialHashAppliedRef.current) return;
    if (!connection?.ready) return;
    const initialPtyId = initialHashPtyIdRef.current;
    if (initialPtyId == null) {
      initialHashAppliedRef.current = true;
      return;
    }
    const match = sessions.find(
      (session) =>
        session.connectionId === primaryConnectionId &&
        session.ptyId === initialPtyId &&
        session.state !== "closed",
    );
    if (match) {
      workspace.focusSession(match.id);
    }
    initialHashAppliedRef.current = true;
  }, [connection?.ready, primaryConnectionId, sessions, workspace]);

  useEffect(() => {
    const requestedFont = font.trim() || DEFAULT_FONT;
    const families = splitFontFamilies(requestedFont).filter(
      (family) => !CSS_GENERIC.has(family.toLowerCase()),
    );
    const requestVersion = ++fontRequestVersionRef.current;
    let cancelled = false;

    if (families.length === 0) {
      setResolvedFont(requestedFont);
      setFontLoading(false);
      return () => {
        cancelled = true;
      };
    }

    setFontLoading(true);

    const loadRequestedFont = async () => {
      for (const family of families) {
        if (cancelled || requestVersion !== fontRequestVersionRef.current) return;

        const loadSpec = `16px "${family}"`;
        if (document.fonts?.check?.(loadSpec)) continue;

        const id = fontStyleId(family);
        if (!document.getElementById(id)) {
          try {
            const response = await fetch(`${basePath}font/${encodeURIComponent(family)}`);
            if (response.ok) {
              const css = await response.text();
              if (cancelled || requestVersion !== fontRequestVersionRef.current) return;
              if (!document.getElementById(id)) {
                const style = document.createElement("style");
                style.id = id;
                style.textContent = css;
                document.head.appendChild(style);
              }
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
    const lru = lruRef.current.filter((id) => id !== workspaceState.focusedSessionId);
    lru.unshift(workspaceState.focusedSessionId);
    lruRef.current = lru;
    const focused = sessions.find((session) => session.id === workspaceState.focusedSessionId);
    if (focused) {
      history.replaceState(null, "", `#${focused.ptyId}`);
    }
  }, [sessions, workspaceState.focusedSessionId]);

  useEffect(() => {
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
  }, [overlay, sessions, workspace, workspaceState.focusedSessionId]);

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

  const focusTerminal = useCallback(() => {
    setTimeout(() => termRef.current?.focus(), 0);
  }, []);

  const closeOverlay = useCallback(() => {
    paletteOverlayOriginRef.current = null;
    fontOverlayOriginRef.current = null;
    setOverlay(null);
    focusTerminal();
  }, [focusTerminal]);

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

  const toggleOverlay = useCallback((target: Overlay) => {
    const current = overlayRef.current;
    if (current === target) {
      cancelOverlay();
      return;
    }

    restoreOverlayPreview(current);

    if (target === "palette") {
      paletteOverlayOriginRef.current = palette;
    } else if (target === "font") {
      fontOverlayOriginRef.current = {
        family: font,
        size: fontSize,
      };
    }

    setOverlay(target);
  }, [cancelOverlay, font, fontSize, palette, restoreOverlayPreview]);

  const changePalette = useCallback((nextPalette: TerminalPalette) => {
    setPalette(nextPalette);
    paletteOverlayOriginRef.current = null;
    writeStorage(PALETTE_KEY, nextPalette.id);
    closeOverlay();
  }, [closeOverlay]);

  const changeFont = useCallback((family: string, size: number) => {
    const value = family.trim() || DEFAULT_FONT;
    setFont(value);
    setFontSize(size);
    fontOverlayOriginRef.current = null;
    writeStorage(FONT_KEY, value);
    writeStorage(FONT_SIZE_KEY, String(size));
    closeOverlay();
  }, [closeOverlay]);

  const switchSession = useCallback((sessionId: SessionId) => {
    workspace.focusSession(sessionId);
    closeOverlay();
  }, [closeOverlay, workspace]);

  const createAndFocus = useCallback(async (command?: string) => {
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
      if (connection?.status !== "disconnected" && connection?.status !== "error") {
        console.error("blit: failed to create PTY", error);
      }
    }
  }, [closeOverlay, connection?.status, primaryConnectionId, workspace, workspaceState.focusedSessionId]);

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
      if (mod && e.shiftKey && e.key === "P") {
        e.preventDefault();
        toggleOverlay("palette");
        return;
      }
      if (mod && e.shiftKey && e.key === "F") {
        e.preventDefault();
        toggleOverlay("font");
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
        void createAndFocus();
        return;
      }
      if (e.key === "Enter" && !mod && !e.shiftKey && !overlayRef.current) {
        const current = stateRef.current;
        const focused = current.focusedSessionId
          ? current.sessions.find((session) => session.id === current.focusedSessionId)
          : null;
        if ((focused && focused.state === "exited") || current.focusedSessionId == null) {
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
  }, [cancelOverlay, createAndFocus, focusedSession, handleRestartOrClose, toggleDebug, toggleOverlay, workspace]);

  const debugStats = workspace.getConnectionDebugStats(
    primaryConnectionId,
    workspaceState.focusedSessionId,
  );

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette}
      fontFamily={resolvedFontWithFallback}
      fontSize={fontSize}
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
        {workspaceState.focusedSessionId != null ? (
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
                  padding: "6px 14px",
                  fontSize: 12,
                  zIndex: z.exitedBanner,
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                }}
              >
                <mark style={{ ...ui.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>
                  Exited
                </mark>
                {connection?.supportsRestart ? (
                  <button onClick={() => handleRestartOrClose()} style={{ ...ui.btn, fontSize: 12 }}>
                    Restart <kbd style={ui.kbd}>Enter</kbd>
                  </button>
                ) : null}
                <button
                  onClick={() => void workspace.closeSession(focusedSession.id)}
                  style={{ ...ui.btn, fontSize: 12, opacity: 0.5 }}
                >
                  Close <kbd style={ui.kbd}>Esc</kbd>
                </button>
              </div>
            )}
          </>
        ) : (
          <EmptyState
            theme={theme}
            mod={/Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl"}
            onCreate={() => void createAndFocus()}
            onExpose={() => toggleOverlay("expose")}
            onHelp={() => toggleOverlay("help")}
          />
        )}
      </section>
      {overlay === "expose" && (
        <ExposeOverlay
          sessions={sessions}
          focusedSessionId={workspaceState.focusedSessionId}
          lru={lruRef.current}
          palette={palette}
          onSelect={switchSession}
          onClose={closeOverlay}
          onCreate={createAndFocus}
        />
      )}
      {overlay === "palette" && (
        <PaletteOverlay
          current={palette}
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
          onSelect={changeFont}
          onPreview={(family, size) => {
            setFont(family);
            setFontSize(size);
          }}
          onClose={closeOverlay}
        />
      )}
      {overlay === "help" && <HelpOverlay onClose={closeOverlay} palette={palette} />}
      {offlineVisible && (
        <DisconnectedOverlay
          palette={palette}
          status={connection?.status ?? "disconnected"}
          retryCount={connection?.retryCount ?? 0}
          error={connection?.error ?? null}
          onReconnect={() => workspace.reconnectConnection(primaryConnectionId)}
        />
      )}
      <footer
        style={{
          ...layout.statusBar,
          backgroundColor: theme.bg,
          borderTopColor: theme.subtleBorder,
        }}
      >
        <StatusBar
          sessions={sessions}
          focusedSession={focusedSession}
          status={connection?.status ?? "disconnected"}
          metrics={metrics}
          palette={palette}
          termSize={termRef.current ? `${termRef.current.cols}x${termRef.current.rows}` : null}
          fontLoading={fontLoading}
          debug={debugPanel}
          toggleDebug={toggleDebug}
          debugStats={debugStats}
          timelineRef={timelineRef}
          netRef={netRef}
          onExpose={() => toggleOverlay("expose")}
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
  mod,
  onCreate,
  onExpose,
  onHelp,
}: {
  theme: ReturnType<typeof themeFor>;
  mod: string;
  onCreate: () => void;
  onExpose: () => void;
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
        gap: 12,
        opacity: 0.6,
      }}
    >
      <div style={{ fontSize: 14 }}>No terminal open</div>
      <div
        style={{
          fontSize: 12,
          display: "flex",
          flexDirection: "column",
          alignItems: "center",
          gap: 8,
        }}
      >
        <button onClick={onCreate} style={{ ...ui.btn, fontSize: 12 }}>
          New terminal <kbd style={ui.kbd}>Enter</kbd>{" "}
          <kbd style={ui.kbd}>{mod}+Shift+Enter</kbd>
        </button>
        <button onClick={onExpose} style={{ ...ui.btn, fontSize: 12 }}>
          Expose <kbd style={ui.kbd}>{mod}+K</kbd>
        </button>
        <button onClick={onHelp} style={{ ...ui.btn, fontSize: 12 }}>
          Help <kbd style={ui.kbd}>Ctrl+?</kbd>
        </button>
      </div>
      <button
        onClick={onCreate}
        style={{
          marginTop: 8,
          padding: "6px 16px",
          fontSize: 13,
          backgroundColor: theme.accent,
          color: "#fff",
          border: "none",
          cursor: "pointer",
        }}
      >
        New terminal
      </button>
    </div>
  );
}
