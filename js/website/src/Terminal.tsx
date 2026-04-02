import { useState, useEffect, useCallback, useRef } from "react";
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  useBlitConnection,
  useBlitSessions,
  useBlitWorkspace,
  useBlitWorkspaceState,
} from "@blit-sh/react";
import type { BlitTerminalHandle } from "@blit-sh/react";
import { BlitWorkspace, PALETTES, createShareTransport } from "@blit-sh/core";
import type {
  BlitTransport,
  BlitSession,
  BlitWasmModule,
  SessionId,
  TerminalPalette,
} from "@blit-sh/core";
import { initWasm } from "./wasm";

const HUB_URL = "wss://hub.blit.sh";
const CONNECTION_ID = "main";
const FONT_FAMILY = "'Fira Code', monospace";
const FONT_SIZE = 14;
const AUTOCLOSE_DELAY = 1000;

const GITHUB_DARK = PALETTES.find((p) => p.id === "github-dark")!;
const GITHUB_LIGHT = PALETTES.find((p) => p.id === "github-light")!;

function useDarkMode(): boolean {
  const [dark, setDark] = useState(
    window.matchMedia("(prefers-color-scheme: dark)").matches,
  );
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) => setDark(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);
  return dark;
}

function rgb([r, g, b]: [number, number, number]): string {
  return `rgb(${r}, ${g}, ${b})`;
}

function rgba([r, g, b]: [number, number, number], a: number): string {
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}

function tabLabel(s: BlitSession): string {
  return s.title ?? s.tag ?? "Terminal";
}

export function Terminal({ passphrase }: { passphrase: string }) {
  const [wasm, setWasm] = useState<BlitWasmModule | null>(null);
  const [error, setError] = useState<string | null>(null);
  const dark = useDarkMode();

  useEffect(() => {
    initWasm().then(setWasm).catch((e) => setError(String(e)));
  }, []);

  if (error) {
    return (
      <div style={{ padding: "2rem", color: "#f55", fontFamily: "monospace" }}>
        Failed to load: {error}
      </div>
    );
  }
  if (!wasm) {
    return (
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          height: "100%",
          background: dark ? rgb(GITHUB_DARK.bg) : rgb(GITHUB_LIGHT.bg),
          color: dark ? rgb(GITHUB_DARK.fg) : rgb(GITHUB_LIGHT.fg),
          fontFamily: "monospace",
        }}
      >
        Loading...
      </div>
    );
  }

  return (
    <TerminalInner
      wasm={wasm}
      passphrase={passphrase}
      dark={dark}
    />
  );
}

function TerminalInner({
  wasm,
  passphrase,
  dark,
}: {
  wasm: BlitWasmModule;
  passphrase: string;
  dark: boolean;
}) {
  const [workspace] = useState(() => new BlitWorkspace({ wasm }));
  const [transport] = useState<BlitTransport>(() =>
    createShareTransport(HUB_URL, passphrase),
  );

  useEffect(() => {
    workspace.addConnection({ id: CONNECTION_ID, transport });
    return () => {
      workspace.removeConnection(CONNECTION_ID);
      transport.close();
    };
  }, [workspace, transport]);

  const palette = dark ? GITHUB_DARK : GITHUB_LIGHT;

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette}
      fontFamily={FONT_FAMILY}
      fontSize={FONT_SIZE}
    >
      <TabShell palette={palette} dark={dark} />
    </BlitWorkspaceProvider>
  );
}

function TabShell({
  palette,
  dark,
}: {
  palette: TerminalPalette;
  dark: boolean;
}) {
  const workspace = useBlitWorkspace();
  const state = useBlitWorkspaceState();
  const sessions = useBlitSessions();
  const connection = useBlitConnection(CONNECTION_ID);
  const termRef = useRef<BlitTerminalHandle | null>(null);

  const visibleSessions = sessions.filter((s) => s.state !== "closed");
  const focusedId = state.focusedSessionId;

  useEffect(() => {
    if (
      connection?.status === "connected" &&
      visibleSessions.length > 0 &&
      !focusedId
    ) {
      workspace.focusSession(visibleSessions[0].id);
    }
  }, [connection?.status, visibleSessions.length, focusedId, workspace]);

  useEffect(() => {
    const desired = new Set<SessionId>();
    if (focusedId) desired.add(focusedId);
    workspace.setVisibleSessions(desired);
  }, [focusedId, workspace]);

  const exitTimers = useRef(new Map<SessionId, ReturnType<typeof setTimeout>>());
  useEffect(() => {
    for (const s of sessions) {
      if (s.state === "exited" && !exitTimers.current.has(s.id)) {
        const timer = setTimeout(() => {
          workspace.closeSession(s.id);
          exitTimers.current.delete(s.id);
        }, AUTOCLOSE_DELAY);
        exitTimers.current.set(s.id, timer);
      }
    }
    return () => {};
  }, [sessions, workspace]);

  useEffect(() => {
    return () => {
      for (const timer of exitTimers.current.values()) clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    if (focusedId && visibleSessions.every((s) => s.id !== focusedId)) {
      const next = visibleSessions[visibleSessions.length - 1];
      workspace.focusSession(next?.id ?? null);
    }
  }, [focusedId, visibleSessions, workspace]);

  const switchTab = useCallback(
    (id: SessionId) => {
      workspace.focusSession(id);
      termRef.current?.focus();
    },
    [workspace],
  );

  const closeTab = useCallback(
    (id: SessionId) => {
      workspace.closeSession(id);
    },
    [workspace],
  );

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.shiftKey && (e.key === "{" || e.key === "}")) {
        e.preventDefault();
        if (visibleSessions.length < 2 || !focusedId) return;
        const idx = visibleSessions.findIndex((s) => s.id === focusedId);
        const next =
          e.key === "}"
            ? visibleSessions[(idx + 1) % visibleSessions.length]
            : visibleSessions[
                (idx - 1 + visibleSessions.length) % visibleSessions.length
              ];
        switchTab(next.id);
      }
    };
    window.addEventListener("keydown", handler, true);
    return () => window.removeEventListener("keydown", handler, true);
  }, [focusedId, visibleSessions, switchTab]);

  const bg = rgb(palette.bg);
  const fg = rgb(palette.fg);
  const dimFg = rgba(palette.fg, dark ? 0.5 : 0.6);
  const border = rgba(palette.fg, 0.15);
  const accent = rgb(palette.ansi[12] ?? palette.ansi[4] ?? palette.fg);
  const tabHover = rgba(palette.fg, dark ? 0.06 : 0.04);

  const statusText =
    connection?.status === "connected"
      ? null
      : connection?.status === "connecting"
        ? "Connecting..."
        : connection?.status === "error"
          ? `Error: ${connection.error ?? "unknown"}`
          : connection?.status ?? "Initializing...";

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        height: "100%",
        background: bg,
        color: fg,
      }}
    >
      {visibleSessions.length > 0 && (
        <div
          style={{
            display: "flex",
            alignItems: "stretch",
            borderBottom: `1px solid ${border}`,
            background: bg,
            flexShrink: 0,
            overflowX: "auto",
            overflowY: "hidden",
            minHeight: 36,
          }}
        >
          {visibleSessions.map((s) => {
            const active = s.id === focusedId;
            return (
              <div
                key={s.id}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 6,
                  padding: "0 12px",
                  cursor: "pointer",
                  fontSize: 13,
                  fontFamily: "'Fira Code', monospace",
                  color: active ? fg : dimFg,
                  borderBottom: active ? `2px solid ${accent}` : "2px solid transparent",
                  background: "transparent",
                  transition: "background 0.1s",
                  whiteSpace: "nowrap",
                  userSelect: "none",
                  flexShrink: 0,
                }}
                onClick={() => switchTab(s.id)}
                onMouseEnter={(e) =>
                  (e.currentTarget.style.background = tabHover)
                }
                onMouseLeave={(e) =>
                  (e.currentTarget.style.background = "transparent")
                }
              >
                <span>{tabLabel(s)}</span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    closeTab(s.id);
                  }}
                  style={{
                    background: "none",
                    border: "none",
                    color: dimFg,
                    cursor: "pointer",
                    padding: "2px 4px",
                    fontSize: 14,
                    lineHeight: 1,
                    borderRadius: 3,
                    opacity: 0.6,
                  }}
                  onMouseEnter={(e) => (e.currentTarget.style.opacity = "1")}
                  onMouseLeave={(e) => (e.currentTarget.style.opacity = "0.6")}
                  aria-label={`Close ${tabLabel(s)}`}
                >
                  x
                </button>
              </div>
            );
          })}
        </div>
      )}

      <div style={{ flex: 1, overflow: "hidden", position: "relative" }}>
        {statusText && (
          <div
            style={{
              position: "absolute",
              inset: 0,
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              zIndex: 1,
              fontFamily: "'Fira Code', monospace",
              fontSize: 14,
              color: dimFg,
            }}
          >
            {statusText}
          </div>
        )}
        {focusedId && (
          <BlitTerminal
            ref={termRef}
            sessionId={focusedId}
            fontFamily={FONT_FAMILY}
            fontSize={FONT_SIZE}
            palette={palette}
            style={{ width: "100%", height: "100%" }}
          />
        )}
      </div>
    </div>
  );
}
