import { useState, useEffect, useCallback, useRef, useSyncExternalStore, type MouseEvent as ReactMouseEvent } from "react";
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  useBlitConnection,
  useBlitSessions,
  useBlitWorkspace,
  useBlitWorkspaceState,
} from "@blit-sh/react";
import type { BlitTerminalHandle } from "@blit-sh/react";
import { BlitWorkspace, PALETTES } from "@blit-sh/core";
import type {
  BlitDebug,
  BlitSession,
  BlitWasmModule,
  SessionId,
  TerminalPalette,
} from "@blit-sh/core";
import { initWasm } from "./wasm";

const HUB_URL = "wss://hub.blit.sh";
const CONNECTION_ID = "main";

type DebugEntry = { t: number; level: "log" | "warn" | "error"; msg: string };

class DebugLog implements BlitDebug {
  private entries: DebugEntry[] = [];
  private snapshot: readonly DebugEntry[] = [];
  private listeners = new Set<() => void>();

  private push(level: DebugEntry["level"], msg: string, args: unknown[]) {
    const formatted = args.length
      ? msg.replace(/%[sdo]/g, () => {
          const a = args.shift();
          return typeof a === "object" ? JSON.stringify(a) : String(a);
        })
      : msg;
    this.entries.push({ t: Date.now(), level, msg: formatted });
    if (this.entries.length > 500) this.entries.shift();
    this.snapshot = [...this.entries];
    for (const l of this.listeners) l();
  }

  log(msg: string, ...args: unknown[]) { this.push("log", msg, args); }
  warn(msg: string, ...args: unknown[]) { this.push("warn", msg, args); }
  error(msg: string, ...args: unknown[]) { this.push("error", msg, args); }

  subscribe(listener: () => void) {
    this.listeners.add(listener);
    return () => { this.listeners.delete(listener); };
  }

  getSnapshot() { return this.snapshot; }
}

function useDebugPanel() {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key === "d") {
        e.preventDefault();
        setOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, []);
  return [open, setOpen] as const;
}

function DebugPanel({ log, onClose }: { log: DebugLog; onClose: () => void }) {
  const entries = useSyncExternalStore(
    (cb) => log.subscribe(cb),
    () => log.getSnapshot(),
  );
  const bottomRef = useRef<HTMLDivElement>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [entries.length]);

  const colors = { log: "#8b949e", warn: "#d29922", error: "#f85149" };

  const copyLog = () => {
    const text = entries
      .map((e) => `${new Date(e.t).toISOString().slice(11, 23)} [${e.level}] ${e.msg}`)
      .join("\n");
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  };

  return (
    <div style={{
      position: "fixed", top: 0, right: 0, bottom: 0, width: 420,
      background: "rgba(13,17,23,0.95)", borderLeft: "1px solid #30363d",
      display: "flex", flexDirection: "column", zIndex: 9999,
      fontFamily: "'Fira Code', monospace", fontSize: 11,
    }}>
      <div style={{
        padding: "8px 12px", borderBottom: "1px solid #30363d",
        display: "flex", justifyContent: "space-between", alignItems: "center",
        color: "#c9d1d9", fontWeight: 700, fontSize: 12,
      }}>
        <span>blit debug</span>
        <div style={{ display: "flex", gap: 4 }}>
          <button onClick={copyLog} style={{
            background: "none", border: "1px solid #30363d", color: "#8b949e",
            cursor: "pointer", fontSize: 11, padding: "2px 8px", borderRadius: 4,
            fontFamily: "inherit",
          }}>{copied ? "Copied!" : "Copy"}</button>
          <button onClick={onClose} style={{
            background: "none", border: "none", color: "#8b949e",
            cursor: "pointer", fontSize: 14, padding: "2px 6px",
          }}>✕</button>
        </div>
      </div>
      <div style={{ flex: 1, overflowY: "auto", padding: "4px 0" }}>
        {entries.map((e, i) => (
          <div key={i} style={{
            padding: "2px 12px", color: colors[e.level],
            borderLeft: e.level !== "log" ? `2px solid ${colors[e.level]}` : "2px solid transparent",
          }}>
            <span style={{ opacity: 0.5 }}>
              {new Date(e.t).toISOString().slice(11, 23)}
            </span>{" "}
            {e.msg}
          </div>
        ))}
        <div ref={bottomRef} />
      </div>
    </div>
  );
}
const FONT_FAMILY = "'Fira Code', monospace";
const FONT_SIZE = 14;
const EXITED_LABEL_STYLE: React.CSSProperties = {
  cursor: "pointer",
  padding: "4px 12px",
  borderRadius: 6,
  border: "1px solid rgba(255,255,255,0.15)",
  background: "rgba(255,255,255,0.05)",
  transition: "background 0.15s",
};

const SHORTCUTS: [string, string][] = [
  ["Mod+Shift+Enter", "New terminal"],
  ["Mod+[ / ]", "Previous / next tab"],
  ["Mod+Shift+D", "Toggle debug panel"],
  ["Ctrl+?", "Toggle this panel"],
];

const MOD_LABEL = navigator.userAgent.includes("Mac") ? "\u2318" : "Ctrl";

function ShortcutsPanel({ onClose, dimFg, border }: { onClose: () => void; dimFg: string; border: string }) {
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); onClose(); }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  return (
    <div
      onClick={onClose}
      style={{
        position: "fixed", inset: 0, zIndex: 9998,
        display: "flex", alignItems: "center", justifyContent: "center",
        background: "rgba(0,0,0,0.5)",
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          background: "#0d1117", border: `1px solid ${border}`, borderRadius: 10,
          padding: "20px 28px", minWidth: 300,
          fontFamily: "'Fira Code', monospace", fontSize: 13, color: "#c9d1d9",
        }}
      >
        <div style={{ fontWeight: 700, fontSize: 14, marginBottom: 16 }}>Keyboard shortcuts</div>
        <table style={{ borderSpacing: "0 8px" }}>
          <tbody>
            {SHORTCUTS.map(([key, desc]) => (
              <tr key={key}>
                <td style={{ paddingRight: 24, color: dimFg, whiteSpace: "nowrap" }}>
                  {key.replace(/Mod/g, MOD_LABEL)}
                </td>
                <td>{desc}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

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
  const [debugLog] = useState(() => new DebugLog());
  const [debugOpen, setDebugOpen] = useDebugPanel();

  const [workspace] = useState(() => new BlitWorkspace({
    wasm,
    connections: [{
      id: CONNECTION_ID,
      transport: { type: "share", hubUrl: HUB_URL, passphrase, debug: debugLog },
    }],
  }));

  const palette = dark ? GITHUB_DARK : GITHUB_LIGHT;

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette}
      fontFamily={FONT_FAMILY}
      fontSize={FONT_SIZE}
    >
      <TabShell palette={palette} dark={dark} passphrase={passphrase} />
      {debugOpen && <DebugPanel log={debugLog} onClose={() => setDebugOpen(false)} />}
    </BlitWorkspaceProvider>
  );
}

function ShareButton({
  passphrase,
  dimFg,
  tabHover,
}: {
  passphrase: string;
  dimFg: string;
  tabHover: string;
}) {
  const [copied, setCopied] = useState(false);
  const timeout = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const handleClick = (e: ReactMouseEvent) => {
    e.stopPropagation();
    const url = `${location.origin}/#${encodeURIComponent(passphrase)}`;
    navigator.clipboard.writeText(url);
    setCopied(true);
    clearTimeout(timeout.current);
    timeout.current = setTimeout(() => setCopied(false), 2000);
  };

  return (
    <button
      onClick={handleClick}
      style={{
        display: "flex",
        alignItems: "center",
        gap: 5,
        padding: "0 10px",
        background: "transparent",
        border: "none",
        color: dimFg,
        cursor: "pointer",
        fontSize: 12,
        fontFamily: "'Fira Code', monospace",
        whiteSpace: "nowrap",
        transition: "background 0.1s",
        flexShrink: 0,
      }}
      onMouseEnter={(e) => (e.currentTarget.style.background = tabHover)}
      onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
      title="Copy share link"
    >
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
        <path d="M4 12a2 2 0 1 1 0-4 2 2 0 0 1 0 4ZM12 6a2 2 0 1 1 0-4 2 2 0 0 1 0 4ZM12 16a2 2 0 1 1 0-4 2 2 0 0 1 0 4ZM5.7 9.3l4.6 3.4M10.3 5.3l-4.6 3.4" />
      </svg>
      {copied ? "Copied!" : "Share"}
    </button>
  );
}

function TabShell({
  palette,
  dark,
  passphrase,
}: {
  palette: TerminalPalette;
  dark: boolean;
  passphrase: string;
}) {
  const workspace = useBlitWorkspace();
  const state = useBlitWorkspaceState();
  const sessions = useBlitSessions();
  const connection = useBlitConnection(CONNECTION_ID);
  const termRef = useRef<BlitTerminalHandle | null>(null);

  const [showShortcuts, setShowShortcuts] = useState(false);
  const visibleSessions = sessions.filter((s) => s.state !== "closed");
  const focusedId = state.focusedSessionId;

  const creatingRef = useRef(false);
  useEffect(() => {
    if (
      connection?.status === "connected" &&
      connection?.ready &&
      visibleSessions.length === 0 &&
      !creatingRef.current
    ) {
      creatingRef.current = true;
      workspace
        .createSession({
          connectionId: CONNECTION_ID,
          rows: termRef.current?.rows ?? 24,
          cols: termRef.current?.cols ?? 80,
        })
        .then((s) => workspace.focusSession(s.id))
        .finally(() => { creatingRef.current = false; });
    } else if (visibleSessions.length > 0 && !focusedId) {
      workspace.focusSession(visibleSessions[0].id);
    }
  }, [connection?.status, connection?.ready, visibleSessions.length, focusedId, workspace]);

  useEffect(() => {
    const desired = new Set<SessionId>();
    if (focusedId) desired.add(focusedId);
    workspace.setVisibleSessions(desired);
  }, [focusedId, workspace]);

  const focusedSession = sessions.find((s) => s.id === focusedId);
  const focusedExited = focusedSession?.state === "exited";

  useEffect(() => {
    if (!focusedExited || !focusedId) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        workspace.restartSession(focusedId);
      } else if (e.key === "Escape") {
        e.preventDefault();
        workspace.closeSession(focusedId);
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [focusedExited, focusedId, workspace]);

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
      if (mod && e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        workspace.createSession({ connectionId: CONNECTION_ID, rows: 24, cols: 80 })
          .then((s) => workspace.focusSession(s.id)).catch(() => {});
      } else if (e.ctrlKey && e.key === "?") {
        e.preventDefault();
        setShowShortcuts((v) => !v);
      } else if (mod && !e.shiftKey && (e.key === "[" || e.key === "]")) {
        e.preventDefault();
        if (visibleSessions.length < 2 || !focusedId) return;
        const idx = visibleSessions.findIndex((s) => s.id === focusedId);
        const next =
          e.key === "]"
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
  const red = palette.ansi[1] ?? palette.fg;
  const green = palette.ansi[2] ?? palette.fg;

  const statusText =
    connection?.status === "connected"
      ? visibleSessions.length === 0
        ? "Connected — waiting for terminal sessions..."
        : null
      : connection?.status === "connecting"
        ? "Connecting — waiting for blit share..."
        : connection?.status === "error"
          ? `Error: ${connection.error ?? "unknown"}`
          : connection?.status === "disconnected"
            ? "Disconnected"
            : "Connecting...";

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
            padding: "4px 4px 0",
            gap: 2,
          }}
        >
          <div style={{ display: "flex", flex: 1, overflowX: "auto", alignItems: "stretch" }}>
          {visibleSessions.map((s) => {
            const active = s.id === focusedId;
            return (
              <div
                key={s.id}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: 6,
                  padding: "0 14px",
                  cursor: "pointer",
                  fontSize: 13,
                  fontFamily: "'Fira Code', monospace",
                  color: active ? fg : dimFg,
                  borderBottom: active ? `2px solid ${accent}` : "2px solid transparent",
                  background: active ? rgba(palette.fg, dark ? 0.06 : 0.04) : "transparent",
                  borderRadius: "6px 6px 0 0",
                  transition: "background 0.15s",
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
                <span style={{
                  display: "inline-block",
                  width: 7, height: 7, borderRadius: "50%",
                  background: s.state === "active" ? rgb(green) : rgba(palette.fg, 0.3),
                  flexShrink: 0,
                }} />
                <span>{tabLabel(s)}</span>
                <button
                  onClick={(e) => {
                    e.stopPropagation();
                    closeTab(s.id);
                  }}
                  style={{
                    background: "none",
                    border: "none",
                    color: rgb(red),
                    cursor: "pointer",
                    padding: 0,
                    width: 18,
                    height: 18,
                    fontSize: 12,
                    lineHeight: "18px",
                    textAlign: "center",
                    borderRadius: "50%",
                    display: "inline-flex",
                    alignItems: "center",
                    justifyContent: "center",
                    transition: "background 0.15s, color 0.15s",
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.background = rgba(red, 0.2);
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.background = "none";
                  }}
                  aria-label={`Close ${tabLabel(s)}`}
                >
                  {"\u2715"}
                </button>
              </div>
            );
          })}
          <button
            onClick={() => {
              workspace
                .createSession({
                  connectionId: CONNECTION_ID,
                  rows: termRef.current?.rows ?? 24,
                  cols: termRef.current?.cols ?? 80,
                  ...(focusedId ? { cwdFromSessionId: focusedId } : {}),
                })
                .then((s) => workspace.focusSession(s.id))
                .catch(() => {});
            }}
            style={{
              background: "none",
              border: "none",
              color: dimFg,
              cursor: "pointer",
              padding: "0 10px",
              fontSize: 18,
              fontFamily: "'Fira Code', monospace",
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              flexShrink: 0,
              transition: "color 0.15s",
            }}
            onMouseEnter={(e) => (e.currentTarget.style.color = fg)}
            onMouseLeave={(e) => (e.currentTarget.style.color = dimFg)}
            aria-label="New tab"
          >
            +
          </button>
          </div>
          <ShareButton passphrase={passphrase} dimFg={dimFg} tabHover={tabHover} />
          <button
            onClick={() => setShowShortcuts(true)}
            style={{
              background: "transparent",
              border: "none",
              color: dimFg,
              cursor: "pointer",
              padding: "0 10px",
              fontSize: 14,
              fontFamily: "'Fira Code', monospace",
              fontWeight: 700,
              flexShrink: 0,
              transition: "background 0.1s",
            }}
            onMouseEnter={(e) => (e.currentTarget.style.background = tabHover)}
            onMouseLeave={(e) => (e.currentTarget.style.background = "transparent")}
            title="Keyboard shortcuts"
          >
            ?
          </button>
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
        {focusedExited && (
          <div
            style={{
              position: "absolute",
              bottom: 16, left: "50%", transform: "translateX(-50%)",
              display: "flex",
              alignItems: "center",
              gap: 12,
              zIndex: 2,
              padding: "8px 16px",
              background: rgba(palette.bg, 0.9),
              backdropFilter: "blur(8px)",
              border: `1px solid ${border}`,
              borderRadius: 10,
              fontFamily: "'Fira Code', monospace",
              fontSize: 13,
              color: dimFg,
              whiteSpace: "nowrap",
            }}
          >
            <span>Exited</span>
            <span
              role="button"
              tabIndex={0}
              style={EXITED_LABEL_STYLE}
              onClick={() => workspace.restartSession(focusedId!)}
              onMouseEnter={(e) => (e.currentTarget.style.background = "rgba(255,255,255,0.12)")}
              onMouseLeave={(e) => (e.currentTarget.style.background = "rgba(255,255,255,0.05)")}
            >
              Enter — reopen
            </span>
            <span
              role="button"
              tabIndex={0}
              style={EXITED_LABEL_STYLE}
              onClick={() => workspace.closeSession(focusedId!)}
              onMouseEnter={(e) => (e.currentTarget.style.background = "rgba(255,255,255,0.12)")}
              onMouseLeave={(e) => (e.currentTarget.style.background = "rgba(255,255,255,0.05)")}
            >
              Esc — close
            </span>
          </div>
        )}
      </div>
      {showShortcuts && (
        <ShortcutsPanel
          onClose={() => setShowShortcuts(false)}
          dimFg={dimFg}
          border={border}
        />
      )}
    </div>
  );
}
