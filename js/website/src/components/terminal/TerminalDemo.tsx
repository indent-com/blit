import {
  createSignal,
  createEffect,
  createMemo,
  onMount,
  onCleanup,
  Show,
} from "solid-js";
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  createBlitWorkspaceState,
  createBlitSessions,
} from "@blit-sh/solid";
import { BlitWorkspace, PALETTES } from "@blit-sh/core";
import type { BlitSession, BlitWasmModule, SessionId } from "@blit-sh/core";
import { initWasm } from "../../lib/wasm";
import {
  isRawPassphrase,
  encryptPassphrase,
  decryptPassphrase,
} from "../../lib/passphrase-crypto";
import { createDebugLog, type DebugLog } from "./DebugPanel";
import TabBar from "./TabBar";
import StatusOverlay from "./StatusOverlay";
import ShortcutsPanel from "./ShortcutsPanel";
import DebugPanel from "./DebugPanel";

const HUB_URL = "wss://hub.blit.sh";
const CONNECTION_ID = "main";
const FONT_FAMILY = "Fira Code, ui-monospace, monospace";
const FONT_SIZE = 14;

const GITHUB_DARK = PALETTES.find((p) => p.id === "github-dark")!;
const GITHUB_LIGHT = PALETTES.find((p) => p.id === "github-light")!;

// ---------------------------------------------------------------------------
// Passphrase resolution
// ---------------------------------------------------------------------------

type PassphraseResult =
  | { ok: true; passphrase: string }
  | { ok: false; error: string };

function resolvePassphrase(): PassphraseResult {
  const raw = decodeURIComponent(location.hash.slice(1));
  if (!raw) {
    return { ok: false, error: "No session specified." };
  }
  if (isRawPassphrase(raw)) {
    // Raw UUID — encrypt and replace URL
    const encrypted = encryptPassphrase(raw);
    history.replaceState(null, "", `/s#${encodeURIComponent(encrypted)}`);
    return { ok: true, passphrase: raw };
  }
  // Try to decrypt
  const decrypted = decryptPassphrase(raw);
  if (decrypted === null) {
    return {
      ok: false,
      error:
        "Cannot decrypt session link. This link was created on a different device.",
    };
  }
  return { ok: true, passphrase: decrypted };
}

// ---------------------------------------------------------------------------
// Root component
// ---------------------------------------------------------------------------

export default function TerminalDemo() {
  const [result, setResult] = createSignal<PassphraseResult | null>(null);
  const [wasm, setWasm] = createSignal<BlitWasmModule | null>(null);
  const [wasmError, setWasmError] = createSignal<string | null>(null);

  onMount(() => {
    const r = resolvePassphrase();
    setResult(r);
    if (r.ok) {
      initWasm()
        .then(setWasm)
        .catch((e) => setWasmError(String(e)));
    }
  });

  return (
    <>
      <Show when={result()?.ok === false}>
        <StatusOverlay
          status={(result() as { ok: false; error: string }).error}
          isError
        />
      </Show>
      <Show when={wasmError()}>
        <StatusOverlay status={`Failed to load: ${wasmError()}`} isError />
      </Show>
      <Show when={!result()}>
        <StatusOverlay status="Loading..." />
      </Show>
      <Show when={result()?.ok && !wasm() && !wasmError()}>
        <StatusOverlay status="Loading WASM..." />
      </Show>
      <Show when={result()?.ok && wasm()}>
        <TerminalInner
          wasm={wasm()!}
          passphrase={(result() as { ok: true; passphrase: string }).passphrase}
        />
      </Show>
    </>
  );
}

// ---------------------------------------------------------------------------
// TerminalInner: workspace setup + tab shell
// ---------------------------------------------------------------------------

function TerminalInner(props: { wasm: BlitWasmModule; passphrase: string }) {
  const debugLog = createDebugLog();
  const [debugOpen, setDebugOpen] = createSignal(false);

  // Theme
  const COOKIE_MAX_AGE = 60 * 60 * 24 * 400;
  const [dark, setDark] = createSignal(true);
  onMount(() => {
    setDark(document.documentElement.getAttribute("data-theme") !== "light");
  });
  const toggleTheme = () => {
    const next = !dark();
    setDark(next);
    const name = next ? "dark" : "light";
    localStorage.setItem("blit-theme", name);
    document.cookie = `blit-theme=${name};path=/;max-age=${COOKIE_MAX_AGE};SameSite=Lax`;
    document.documentElement.setAttribute("data-theme", name);
  };

  // Keyboard shortcut for debug panel
  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && e.key === "d") {
        e.preventDefault();
        setDebugOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", handler);
    onCleanup(() => window.removeEventListener("keydown", handler));
  });

  const workspace = new BlitWorkspace({
    wasm: props.wasm,
    connections: [
      {
        id: CONNECTION_ID,
        transport: {
          type: "share",
          hubUrl: HUB_URL,
          passphrase: props.passphrase,
          debug: debugLog,
        },
      },
    ],
  });

  onCleanup(() => workspace.dispose());

  const palette = () => (dark() ? GITHUB_DARK : GITHUB_LIGHT);

  return (
    <BlitWorkspaceProvider
      workspace={workspace}
      palette={palette()}
      fontFamily={FONT_FAMILY}
      fontSize={FONT_SIZE}
    >
      <TabShell
        workspace={workspace}
        palette={palette}
        dark={dark}
        passphrase={props.passphrase}
        onToggleTheme={toggleTheme}
      />
      <Show when={debugOpen()}>
        <DebugPanel log={debugLog} onClose={() => setDebugOpen(false)} />
      </Show>
    </BlitWorkspaceProvider>
  );
}

// ---------------------------------------------------------------------------
// ToolbarMenu: dropdown from the "..." button
// ---------------------------------------------------------------------------

function MenuRow(props: { label: string; onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={props.onClick}
      class="flex w-full items-center px-3 py-1.5 bg-transparent border-none text-[var(--fg)] text-xs font-sans cursor-pointer rounded transition-colors hover:bg-[var(--surface)]"
    >
      {props.label}
    </button>
  );
}

function ToolbarMenu(props: {
  onCopyLink: () => void;
  copied: boolean;
  onShortcuts: () => void;
  dark: boolean;
  onToggleTheme: () => void;
  onClose: () => void;
}) {
  // Close menu on outside click
  let menuRef!: HTMLDivElement;
  onMount(() => {
    const handler = (e: MouseEvent) => {
      if (menuRef && !menuRef.contains(e.target as Node)) {
        props.onClose();
      }
    };
    // Defer to avoid catching the same click that opened the menu
    requestAnimationFrame(() =>
      document.addEventListener("click", handler, true),
    );
    onCleanup(() => document.removeEventListener("click", handler, true));
  });

  // Close on Escape
  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        props.onClose();
      }
    };
    window.addEventListener("keydown", handler);
    onCleanup(() => window.removeEventListener("keydown", handler));
  });

  return (
    <div
      ref={menuRef}
      class="absolute right-0 top-full mt-1 z-[100] min-w-[160px] rounded-lg border border-[var(--border)] bg-[var(--bg)] py-1 shadow-lg"
    >
      <MenuRow
        label={props.copied ? "Copied!" : "Copy link"}
        onClick={props.onCopyLink}
      />
      <MenuRow label="Shortcuts" onClick={props.onShortcuts} />
      <MenuRow
        label={props.dark ? "Light mode" : "Dark mode"}
        onClick={props.onToggleTheme}
      />
    </div>
  );
}

// ---------------------------------------------------------------------------
// TabShell: manages sessions, tab bar, terminal rendering
// ---------------------------------------------------------------------------

function TabShell(props: {
  workspace: BlitWorkspace;
  palette: () => typeof GITHUB_DARK;
  dark: () => boolean;
  passphrase: string;
  onToggleTheme: () => void;
}) {
  const workspace = props.workspace;
  const state = createBlitWorkspaceState(workspace);
  const sessions = createBlitSessions(workspace);

  const [showShortcuts, setShowShortcuts] = createSignal(false);
  const [menuOpen, setMenuOpen] = createSignal(false);
  const [copied, setCopied] = createSignal(false);
  let copyTimeout: ReturnType<typeof setTimeout> | undefined;

  const visibleSessions = createMemo(() =>
    sessions().filter((s) => s.state !== "closed"),
  );
  const focusedId = createMemo(() => state().focusedSessionId);

  // Track previous focused for fallback
  let prevFocused: { id: SessionId; index: number } | null = null;
  createEffect(() => {
    const fid = focusedId();
    if (fid) {
      const idx = visibleSessions().findIndex((s) => s.id === fid);
      if (idx >= 0) prevFocused = { id: fid, index: idx };
    }
  });

  // Auto-create session when connected + no visible sessions
  let creating = false;
  createEffect(() => {
    const conn = state().connections[0];
    if (
      conn?.status === "connected" &&
      conn?.ready &&
      visibleSessions().length === 0 &&
      !creating
    ) {
      creating = true;
      workspace
        .createSession({
          connectionId: CONNECTION_ID,
          rows: 24,
          cols: 80,
        })
        .then((s) => workspace.focusSession(s.id))
        .finally(() => {
          creating = false;
        });
    } else if (visibleSessions().length > 0 && !focusedId()) {
      const idx = prevFocused
        ? Math.min(prevFocused.index, visibleSessions().length - 1)
        : 0;
      workspace.focusSession(visibleSessions()[idx].id);
    }
  });

  // Keep visible sessions in sync
  createEffect(() => {
    const desired = new Set<SessionId>();
    const vs = visibleSessions();
    for (const s of vs) desired.add(s.id);
    workspace.setVisibleSessions(desired);
  });

  const focusedSession = createMemo(() =>
    sessions().find((s) => s.id === focusedId()),
  );
  const focusedExited = createMemo(() => focusedSession()?.state === "exited");

  // Handle Enter/Esc on exited sessions
  createEffect(() => {
    const fid = focusedId();
    if (!focusedExited() || !fid) return;
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Enter") {
        e.preventDefault();
        workspace.restartSession(fid);
      } else if (e.key === "Escape") {
        e.preventDefault();
        workspace.closeSession(fid);
      }
    };
    window.addEventListener("keydown", handler);
    onCleanup(() => window.removeEventListener("keydown", handler));
  });

  // If focused session vanished, focus the last visible
  createEffect(() => {
    const fid = focusedId();
    const vs = visibleSessions();
    if (fid && vs.every((s) => s.id !== fid)) {
      const next = vs[vs.length - 1];
      workspace.focusSession(next?.id ?? null);
    }
  });

  // Keyboard shortcuts
  onMount(() => {
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;
      if (mod && e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        workspace
          .createSession({ connectionId: CONNECTION_ID, rows: 24, cols: 80 })
          .then((s) => workspace.focusSession(s.id))
          .catch(() => {});
      } else if (
        e.ctrlKey &&
        e.shiftKey &&
        (e.key === "?" || e.code === "Slash")
      ) {
        e.preventDefault();
        setShowShortcuts((v) => !v);
      } else if (mod && !e.shiftKey && (e.key === "[" || e.key === "]")) {
        e.preventDefault();
        const vs = visibleSessions();
        const fid = focusedId();
        if (vs.length < 2 || !fid) return;
        const idx = vs.findIndex((s) => s.id === fid);
        const next =
          e.key === "]"
            ? vs[(idx + 1) % vs.length]
            : vs[(idx - 1 + vs.length) % vs.length];
        workspace.focusSession(next.id);
      }
    };
    window.addEventListener("keydown", handler, true);
    onCleanup(() => window.removeEventListener("keydown", handler, true));
  });

  // Connection status text
  const statusText = createMemo(() => {
    const conn = state().connections[0];
    if (!conn) return "Connecting...";
    if (conn.status === "connected") {
      return visibleSessions().length === 0
        ? "Connected \u2014 waiting for terminal sessions..."
        : null;
    }
    if (conn.status === "connecting")
      return "Connecting \u2014 waiting for blit share...";
    if (conn.status === "error") return `Error: ${conn.error ?? "unknown"}`;
    if (conn.status === "disconnected") return "Disconnected";
    return "Connecting...";
  });

  const handleSelectTab = (id: SessionId) => workspace.focusSession(id);
  const handleCloseTab = (id: SessionId) => workspace.closeSession(id);
  const handleNewTab = () => {
    const fid = focusedId();
    workspace
      .createSession({
        connectionId: CONNECTION_ID,
        rows: 24,
        cols: 80,
        ...(fid ? { cwdFromSessionId: fid } : {}),
      })
      .then((s) => workspace.focusSession(s.id))
      .catch(() => {});
  };

  return (
    <div class="fixed inset-0 z-50 flex flex-col bg-[var(--bg)]">
      <Show when={visibleSessions().length > 0}>
        <div class="flex items-stretch shrink-0 border-b border-[var(--border)] bg-[var(--surface)]">
          <div class="flex-1 min-w-0">
            <TabBar
              sessions={visibleSessions()}
              focusedSessionId={focusedId()}
              onSelect={handleSelectTab}
              onClose={handleCloseTab}
            />
          </div>
          {/* New tab button */}
          <button
            type="button"
            onClick={handleNewTab}
            class="flex w-9 shrink-0 cursor-pointer items-center justify-center border-none bg-transparent text-[var(--dim)] transition-colors hover:text-[var(--fg)]"
            title="New tab"
          >
            <svg
              class="block"
              width="14"
              height="14"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              stroke-width="1.5"
              stroke-linecap="round"
            >
              <path d="M8 3v10M3 8h10" />
            </svg>
          </button>
          {/* Menu button */}
          <div class="relative shrink-0">
            <button
              type="button"
              onClick={() => setMenuOpen((v) => !v)}
              class="flex w-9 h-full cursor-pointer items-center justify-center border-l border-[var(--border)] bg-transparent border-y-0 border-r-0 text-[var(--dim)] transition-colors hover:text-[var(--fg)]"
              title="Menu"
            >
              {/* Three vertical dots */}
              <svg
                class="block"
                width="14"
                height="14"
                viewBox="0 0 16 16"
                fill="currentColor"
              >
                <circle cx="8" cy="3" r="1.5" />
                <circle cx="8" cy="8" r="1.5" />
                <circle cx="8" cy="13" r="1.5" />
              </svg>
            </button>
            <Show when={menuOpen()}>
              <ToolbarMenu
                onCopyLink={() => {
                  const url = `${location.origin}/s#${encodeURIComponent(props.passphrase)}`;
                  navigator.clipboard.writeText(url);
                  setCopied(true);
                  clearTimeout(copyTimeout);
                  copyTimeout = setTimeout(() => setCopied(false), 2000);
                }}
                copied={copied()}
                onShortcuts={() => {
                  setShowShortcuts(true);
                  setMenuOpen(false);
                }}
                dark={props.dark()}
                onToggleTheme={() => {
                  props.onToggleTheme();
                  setMenuOpen(false);
                }}
                onClose={() => setMenuOpen(false)}
              />
            </Show>
          </div>
        </div>
      </Show>

      <div class="flex-1 overflow-hidden relative p-2 pb-1">
        <Show when={statusText()}>
          <StatusOverlay status={statusText()!} />
        </Show>
        <Show when={focusedId()}>
          <BlitTerminal
            sessionId={focusedId()!}
            fontFamily={FONT_FAMILY}
            fontSize={FONT_SIZE}
            palette={props.palette()}
            style={{ width: "100%", height: "100%" }}
          />
        </Show>
        <Show when={focusedExited()}>
          <div class="absolute bottom-4 left-1/2 -translate-x-1/2 flex items-center gap-3 z-[2] px-4 py-2 bg-[var(--bg)]/90 backdrop-blur-sm border border-[var(--border)] rounded-xl font-mono text-[13px] text-[var(--dim)] whitespace-nowrap">
            <span>Exited</span>
            <span
              role="button"
              tabIndex={0}
              class="cursor-pointer px-3 py-1 rounded-md border border-[var(--border)] bg-[var(--surface)] transition-colors hover:brightness-110"
              onClick={() => workspace.restartSession(focusedId()!)}
            >
              Enter &mdash; reopen
            </span>
            <span
              role="button"
              tabIndex={0}
              class="cursor-pointer px-3 py-1 rounded-md border border-[var(--border)] bg-[var(--surface)] transition-colors hover:brightness-110"
              onClick={() => workspace.closeSession(focusedId()!)}
            >
              Esc &mdash; close
            </span>
          </div>
        </Show>
      </div>
      <Show when={showShortcuts()}>
        <ShortcutsPanel onClose={() => setShowShortcuts(false)} />
      </Show>
    </div>
  );
}
