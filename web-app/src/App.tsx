import {
  useState,
  useCallback,
  useEffect,
  useRef,
} from "react";
import {
  BlitTerminal,
  useBlitSessions,
  WebSocketTransport,
  PALETTES,
  TerminalStore,
  DEFAULT_FONT,
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
} from "blit-react";
import type {
  BlitTerminalHandle,
  TerminalPalette,
  UseBlitSessionsReturn,
  SearchResult,
} from "blit-react";
import { useMetrics, formatBw } from "./useMetrics";
import type { Metrics } from "./useMetrics";

const PASS_KEY = "blit.passphrase";
const PALETTE_KEY = "blit.palette";
const FONT_KEY = "blit.fontFamily";

function readStorage(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
function writeStorage(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return proto + "//" + location.host + location.pathname;
}

function preferredPalette(): TerminalPalette {
  const q = new URLSearchParams(location.search).get("palette");
  if (q) {
    const p = PALETTES.find((x) => x.id === q);
    if (p) return p;
  }
  const s = readStorage(PALETTE_KEY);
  if (s) {
    const p = PALETTES.find((x) => x.id === s);
    if (p) return p;
  }
  return PALETTES[0];
}

function preferredFont(): string {
  const q = new URLSearchParams(location.search).get("font");
  if (q?.trim()) return q.trim();
  const s = readStorage(FONT_KEY);
  if (s?.trim()) return s.trim();
  return DEFAULT_FONT;
}

export function App() {
  const savedPass = readStorage(PASS_KEY);
  const [transport, setTransport] = useState<WebSocketTransport | null>(() =>
    savedPass ? new WebSocketTransport(wsUrl(), savedPass) : null,
  );
  const [authError, setAuthError] = useState<string | null>(null);
  const passRef = useRef<HTMLInputElement>(null);

  const connect = useCallback(
    (pass: string) => {
      setAuthError(null);
      transport?.close();
      const t = new WebSocketTransport(wsUrl(), pass, { reconnect: false });
      const onStatus = (status: string) => {
        if (status === "connected") {
          writeStorage(PASS_KEY, pass);
          t.removeEventListener("statuschange", onStatus);
          setTransport(new WebSocketTransport(wsUrl(), pass));
        } else if (status === "error") {
          setAuthError("Authentication failed");
          t.close();
          t.removeEventListener("statuschange", onStatus);
        }
      };
      t.addEventListener("statuschange", onStatus);
    },
    [transport],
  );

  if (!transport) {
    return (
      <AuthScreen
        error={authError}
        passRef={passRef}
        onSubmit={(pass) => connect(pass)}
      />
    );
  }

  return <Workspace transport={transport} onAuthError={() => {
    transport.close();
    setTransport(null);
    setAuthError("Authentication failed");
  }} />;
}

function AuthScreen({
  error,
  passRef,
  onSubmit,
}: {
  error: string | null;
  passRef: React.RefObject<HTMLInputElement | null>;
  onSubmit: (pass: string) => void;
}) {
  return (
    <main style={styles.authContainer}>
      <form
        style={styles.authForm}
        onSubmit={(e) => {
          e.preventDefault();
          const v = passRef.current?.value;
          if (v) onSubmit(v);
        }}
      >
        <input
          ref={passRef}
          type="password"
          placeholder="passphrase"
          autoFocus
          style={styles.authInput}
        />
        {error && <output style={styles.authError}>{error}</output>}
      </form>
    </main>
  );
}

type Overlay = "expose" | "palette" | "font" | "help" | null;

function Workspace({ transport, onAuthError }: { transport: WebSocketTransport; onAuthError: () => void }) {
  const [palette, setPalette] = useState<TerminalPalette>(preferredPalette);
  const [font, setFont] = useState(preferredFont);
  const [fontSize] = useState(13);
  const [overlay, setOverlay] = useState<Overlay>(null);
  const termRef = useRef<BlitTerminalHandle>(null);
  const overlayRef = useRef<Overlay>(null);
  overlayRef.current = overlay;
  const sessionsRef = useRef<UseBlitSessionsReturn | null>(null);
  const searchResultsCbRef = useRef<((reqId: number, results: SearchResult[]) => void) | null>(null);

  const storeRef = useRef<TerminalStore | null>(null);
  if (!storeRef.current) {
    storeRef.current = new TerminalStore(transport);
  }
  const store = storeRef.current;

  const onSearchResults = useCallback(
    (reqId: number, results: SearchResult[]) => {
      searchResultsCbRef.current?.(reqId, results);
    },
    [],
  );

  const sessions = useBlitSessions(transport, {
    autoCreateIfEmpty: true,
    getInitialSize: () => ({
      rows: termRef.current?.rows ?? 24,
      cols: termRef.current?.cols ?? 80,
    }),
    onSearchResults,
  });
  sessionsRef.current = sessions;
  const metrics = useMetrics(transport);

  const dark = palette.dark;

  useEffect(() => {
    store.setPalette(palette);
  }, [store, palette]);

  useEffect(() => {
    store.setFontFamily(font);
  }, [store, font]);

  useEffect(() => {
    store.setLead(sessions.focusedPtyId);
  }, [store, sessions.focusedPtyId]);

  useEffect(() => {
    const desired = new Set<number>();
    if (sessions.focusedPtyId !== null) desired.add(sessions.focusedPtyId);
    if (overlay === "expose") {
      for (const s of sessions.sessions) {
        if (s.state === "active") desired.add(s.ptyId);
      }
    }
    store.setDesiredSubscriptions(desired);
  }, [store, sessions.focusedPtyId, sessions.sessions, overlay]);

  useEffect(() => {
    return () => store.dispose();
  }, [store]);

  useEffect(() => {
    let wasConnected = false;
    const onStatus = (status: string) => {
      if (status === "connected") wasConnected = true;
      if (status === "error" && !wasConnected) onAuthError();
    };
    transport.addEventListener("statuschange", onStatus);
    return () => transport.removeEventListener("statuschange", onStatus);
  }, [transport, onAuthError]);

  const termCallbackRef = useCallback((handle: BlitTerminalHandle | null) => {
    (termRef as React.MutableRefObject<BlitTerminalHandle | null>).current = handle;
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
    const focused = sessions.sessions.find(
      (s) => s.ptyId === sessions.focusedPtyId,
    );
    document.title = focused?.title ? `${focused.title} — blit` : "blit";
  }, [sessions.focusedPtyId, sessions.sessions]);

  const focusTerminal = useCallback(() => {
    setTimeout(() => termRef.current?.focus(), 0);
  }, []);

  const closeOverlay = useCallback(() => {
    setOverlay(null);
    focusTerminal();
  }, [focusTerminal]);

  const toggleOverlay = useCallback((target: Overlay) => {
    setOverlay((cur) => {
      if (cur === target) {
        focusTerminal();
        return null;
      }
      return target;
    });
  }, [focusTerminal]);

  const changePalette = useCallback((p: TerminalPalette) => {
    setPalette(p);
    writeStorage(PALETTE_KEY, p.id);
    closeOverlay();
  }, [closeOverlay]);

  const changeFont = useCallback((f: string) => {
    const value = f.trim() || DEFAULT_FONT;
    setFont(value);
    writeStorage(FONT_KEY, value);
    closeOverlay();
  }, [closeOverlay]);

  const switchPty = useCallback(
    (ptyId: number) => {
      sessions.focusPty(ptyId);
      closeOverlay();
    },
    [sessions, closeOverlay],
  );

  const createAndFocus = useCallback(async (command?: string) => {
    const id = await sessions.createPty({
      ...(command ? { command } : {}),
      // Inherit cwd from focused PTY, but only when not running a specific command
      // (C2S_CREATE_AT doesn't support the command field).
      ...(!command && sessions.focusedPtyId != null ? { srcPtyId: sessions.focusedPtyId } : {}),
    });
    sessions.focusPty(id);
    closeOverlay();
  }, [sessions, closeOverlay]);

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
      if (mod && e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        createAndFocus();
        return;
      }
      if (mod && e.shiftKey && e.key === "W") {
        e.preventDefault();
        const s = sessionsRef.current;
        if (s && s.focusedPtyId != null) s.closePty(s.focusedPtyId);
        return;
      }
      if (mod && e.shiftKey && (e.key === "{" || e.key === "}")) {
        e.preventDefault();
        const s = sessionsRef.current;
        if (!s) return;
        const ids = s.sessions
          .filter((x) => x.state === "active")
          .map((x) => x.ptyId);
        if (ids.length < 2 || s.focusedPtyId == null) return;
        const idx = ids.indexOf(s.focusedPtyId);
        const next =
          e.key === "}"
            ? ids[(idx + 1) % ids.length]
            : ids[(idx - 1 + ids.length) % ids.length];
        s.focusPty(next);
        return;
      }
      if (e.key === "Escape" && overlayRef.current) {
        e.preventDefault();
        closeOverlay();
        return;
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [toggleOverlay, closeOverlay, createAndFocus]);

  const bg = `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`;

  return (
    <main
      style={{
        ...styles.workspace,
        backgroundColor: bg,
        color: dark ? "#e0e0e0" : "#333",
      }}
    >
      <section style={styles.termContainer}>
        {sessions.focusedPtyId != null && (
          <BlitTerminal
            ref={termCallbackRef}
            transport={transport}
            store={store}
            ptyId={sessions.focusedPtyId}
            palette={palette}
            fontFamily={font}
            fontSize={fontSize}
            style={{ width: "100%", height: "100%" }}
          />
        )}
        {(sessions.status === "disconnected" || sessions.status === "error") && (
          <output style={styles.disconnected}>Disconnected</output>
        )}
      </section>
      {overlay === "expose" && (
        <ExposeOverlay
          sessions={sessions}
          transport={transport}
          store={store}
          palette={palette}
          font={font}
          fontSize={fontSize}
          onSelect={switchPty}
          onClose={closeOverlay}
          onCreate={createAndFocus}
          searchResultsCbRef={searchResultsCbRef}
        />
      )}
      {overlay === "palette" && (
        <PaletteOverlay
          current={palette}
          onSelect={changePalette}
          onPreview={setPalette}
          onClose={closeOverlay}
          dark={dark}
        />
      )}
      {overlay === "font" && (
        <FontOverlay
          current={font}
          onSelect={changeFont}
          onClose={closeOverlay}
          dark={dark}
        />
      )}
      {overlay === "help" && (
        <HelpOverlay onClose={closeOverlay} dark={dark} />
      )}
      <footer
        style={{
          ...styles.statusBar,
          borderTopColor: dark ? "rgba(255,255,255,0.1)" : "rgba(0,0,0,0.1)",
        }}
      >
        <StatusBar
          sessions={sessions}
          metrics={metrics}
          palette={palette}
          onExpose={() => toggleOverlay("expose")}
          onPalette={() => toggleOverlay("palette")}
          onFont={() => toggleOverlay("font")}
        />
      </footer>
    </main>
  );
}

function StatusBar({
  sessions,
  metrics,
  palette,
  onExpose,
  onPalette,
  onFont,
}: {
  sessions: UseBlitSessionsReturn;
  metrics: Metrics;
  palette: TerminalPalette;
  onExpose: () => void;
  onPalette: () => void;
  onFont: () => void;
}) {
  const active = sessions.sessions.filter((s) => s.state === "active");
  return (
    <>
      <button onClick={onExpose} style={styles.statusBtn} title="Expose (Cmd+K)">
        {active.length} PTY{active.length !== 1 ? "s" : ""}
      </button>
      <span style={styles.statusTitle}>
        {sessions.focusedPtyId != null &&
          (sessions.sessions.find(
            (s) => s.ptyId === sessions.focusedPtyId,
          )?.title ??
            `PTY ${sessions.focusedPtyId}`)}
      </span>
      <span style={styles.statusMetrics}>
        {formatBw(metrics.bw)} &middot; {metrics.ups} UPS &middot; {metrics.fps} FPS
      </span>
      <button onClick={onPalette} style={styles.statusBtn} title="Palette (Cmd+Shift+P)">
        <span style={{
          ...styles.swatch,
          backgroundColor: `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`,
          border: "1px solid rgba(128,128,128,0.3)",
          verticalAlign: "middle",
        }} />
      </button>
      <button onClick={onFont} style={styles.statusBtn} title="Font (Cmd+Shift+F)">
        Aa
      </button>
      <span
        role="status"
        aria-label={sessions.status}
        style={{
          ...styles.statusDot,
          backgroundColor: sessions.status === "connected" ? "#4a4" : "#a44",
        }}
      />
    </>
  );
}

const SOURCE_LABEL: Record<number, string> = {
  [SEARCH_SOURCE_TITLE]: "Title",
  [SEARCH_SOURCE_VISIBLE]: "Terminal",
  [SEARCH_SOURCE_SCROLLBACK]: "Backlog",
};

function ExposeOverlay({
  sessions,
  transport,
  store,
  palette,
  font,
  fontSize,
  onSelect,
  onClose,
  onCreate,
  searchResultsCbRef,
}: {
  sessions: UseBlitSessionsReturn;
  transport: WebSocketTransport;
  store: TerminalStore;
  palette: TerminalPalette;
  font: string;
  fontSize: number;
  onSelect: (id: number) => void;
  onClose: () => void;
  onCreate: (command?: string) => void;
  searchResultsCbRef: React.RefObject<((reqId: number, results: SearchResult[]) => void) | null>;
}) {
  const active = sessions.sessions.filter((s) => s.state === "active");
  const dark = palette.dark;
  const [query, setQuery] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const [searchResults, setSearchResults] = useState<SearchResult[] | null>(null);
  const requestIdRef = useRef(0);

  const isCommand = query.startsWith(">");
  const commandText = isCommand ? query.slice(1).trim() : "";
  const searching = !isCommand && query.length > 0;

  useEffect(() => {
    if (!searching) {
      setSearchResults(null);
      return;
    }
    const id = (requestIdRef.current = (requestIdRef.current + 1) & 0xffff);
    sessions.sendSearch(id, query);
  }, [query, searching, sessions]);

  const onSearchResultsRef = useRef<((reqId: number, results: SearchResult[]) => void) | null>(null);
  onSearchResultsRef.current = (reqId: number, results: SearchResult[]) => {
    if (reqId === requestIdRef.current) {
      setSearchResults(results);
    }
  };

  useEffect(() => {
    const handler = (reqId: number, results: SearchResult[]) => {
      onSearchResultsRef.current?.(reqId, results);
    };
    (searchResultsCbRef as React.MutableRefObject<typeof handler | null>).current = handler;
    return () => {
      if (searchResultsCbRef.current === handler) {
        (searchResultsCbRef as React.MutableRefObject<typeof handler | null>).current = null;
      }
    };
  }, [searchResultsCbRef]);

  const sessionsByPtyId = new Map(active.map((s) => [s.ptyId, s]));

  const items: { ptyId: number; title: string; context?: string; source?: number }[] = searching && searchResults
    ? searchResults
        .filter((r) => sessionsByPtyId.has(r.ptyId))
        .map((r) => ({
          ptyId: r.ptyId,
          title: sessionsByPtyId.get(r.ptyId)!.title ?? `PTY ${r.ptyId}`,
          context: r.context,
          source: r.primarySource,
        }))
    : active.map((s) => ({
        ptyId: s.ptyId,
        title: s.title ?? `PTY ${s.ptyId}`,
      }));

  const itemCount = isCommand ? 1 : items.length + 1;
  const initialIdx = items.findIndex((it) => it.ptyId === sessions.focusedPtyId);
  const [selectedIdx, setSelectedIdx] = useState(initialIdx >= 0 ? initialIdx : 0);

  useEffect(() => {
    setSelectedIdx(0);
  }, [query]);

  useEffect(() => {
    searchRef.current?.focus();
  }, []);

  const gridColsRef = useRef(1);

  useEffect(() => {
    const ul = listRef.current;
    if (!ul) return;
    const style = getComputedStyle(ul);
    const cols = style.gridTemplateColumns
      .split(/\s+/)
      .filter((s) => s && s !== "none").length;
    gridColsRef.current = Math.max(1, cols);
  });

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      const gridCols = gridColsRef.current;
      switch (e.key) {
        case "ArrowRight":
          e.preventDefault();
          setSelectedIdx((i) => (i + 1) % itemCount);
          break;
        case "ArrowLeft":
          e.preventDefault();
          setSelectedIdx((i) => (i - 1 + itemCount) % itemCount);
          break;
        case "ArrowDown":
          e.preventDefault();
          setSelectedIdx((i) => Math.min(i + gridCols, itemCount - 1));
          break;
        case "ArrowUp":
          e.preventDefault();
          setSelectedIdx((i) => Math.max(i - gridCols, 0));
          break;
        case "Enter": {
          e.preventDefault();
          if (isCommand) {
            onCreate(commandText || undefined);
          } else if (selectedIdx < items.length) {
            onSelect(items[selectedIdx].ptyId);
          } else {
            onCreate();
          }
          break;
        }
        case "w":
        case "W":
          if (e.ctrlKey || e.metaKey) {
            e.preventDefault();
            if (!isCommand && selectedIdx < items.length) {
              sessions.closePty(items[selectedIdx].ptyId);
              setSelectedIdx((i) => Math.min(i, Math.max(0, items.length - 2)));
            }
          }
          break;
      }
    },
    [selectedIdx, items, itemCount, isCommand, commandText, onSelect, onCreate, sessions],
  );

  useEffect(() => {
    const el = listRef.current?.children[selectedIdx] as HTMLElement | undefined;
    el?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx]);

  const itemBg = (selected: boolean) =>
    selected
      ? dark ? "rgba(255,255,255,0.06)" : "rgba(0,0,0,0.04)"
      : "transparent";
  const itemBorder = (selected: boolean) =>
    selected
      ? "#58f"
      : dark ? "rgba(255,255,255,0.15)" : "rgba(0,0,0,0.15)";

  return (
    <div style={styles.overlay} onClick={onClose}>
      <nav
        style={{
          ...styles.exposePanel,
          backgroundColor: dark ? "rgba(0,0,0,0.85)" : "rgba(255,255,255,0.9)",
          color: dark ? "#e0e0e0" : "#333",
          fontFamily: font,
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <header style={styles.exposeHeader}>
          <input
            ref={searchRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Search terminals, or >command"
            style={{
              ...styles.exposeSearch,
              backgroundColor: dark ? "rgba(255,255,255,0.08)" : "rgba(0,0,0,0.05)",
              color: "inherit",
            }}
          />
          <button style={styles.exposeCloseBtn} onClick={onClose}>
            Esc
          </button>
        </header>
        <ul ref={listRef} style={searching ? styles.exposeSearchResults : styles.exposeCards}>
          {isCommand ? (
            <li
              style={{
                ...styles.exposeItem,
                borderColor: itemBorder(true),
                backgroundColor: itemBg(true),
              }}
              onClick={() => onCreate(commandText || undefined)}
            >
              <span style={styles.exposeItemLabel}>
                Run: <strong>{commandText || "(shell)"}</strong>
              </span>
            </li>
          ) : (
            <>
              {items.map((it, i) => (
                <li
                  key={it.ptyId}
                  style={searching ? {
                    ...styles.exposeItem,
                    borderColor: itemBorder(i === selectedIdx),
                    backgroundColor: itemBg(i === selectedIdx),
                  } : {
                    ...styles.card,
                    borderColor: itemBorder(i === selectedIdx),
                    backgroundColor: itemBg(i === selectedIdx),
                  }}
                  onClick={() => onSelect(it.ptyId)}
                  onMouseEnter={() => setSelectedIdx(i)}
                >
                  {searching ? (
                    <>
                      <figure style={{ margin: 0, width: 120, height: 68, flexShrink: 0, overflow: "hidden" }}>
                        <BlitTerminal
                          transport={transport}
                          store={store}
                          ptyId={it.ptyId}
                          palette={palette}
                          fontFamily={font}
                          fontSize={fontSize}
                          readOnly
                          style={{ width: "100%", height: "100%", pointerEvents: "none" }}
                        />
                      </figure>
                      <div style={{ flex: 1, minWidth: 0 }}>
                        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                          <span style={styles.exposeItemLabel}>{it.title}</span>
                          {it.source != null && (
                            <mark style={styles.badge}>{SOURCE_LABEL[it.source] ?? "Match"}</mark>
                          )}
                          {it.ptyId === sessions.focusedPtyId && (
                            <mark style={styles.badge}>Lead</mark>
                          )}
                        </div>
                        {it.context && (
                          <div style={{
                            fontSize: 11,
                            opacity: 0.6,
                            marginTop: 2,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap" as const,
                          }}>
                            {it.context}
                          </div>
                        )}
                      </div>
                      <button
                        style={styles.exposeCloseItemBtn}
                        title="Close (Ctrl+W)"
                        onClick={(e) => {
                          e.stopPropagation();
                          sessions.closePty(it.ptyId);
                        }}
                      >
                        x
                      </button>
                    </>
                  ) : (
                    <>
                      <header style={styles.cardHeader}>
                        <span style={styles.exposeItemLabel}>{it.title}</span>
                        {it.ptyId === sessions.focusedPtyId && (
                          <mark style={styles.badge}>Lead</mark>
                        )}
                        <button
                          style={styles.exposeCloseItemBtn}
                          title="Close (Ctrl+W)"
                          onClick={(e) => {
                            e.stopPropagation();
                            sessions.closePty(it.ptyId);
                          }}
                        >
                          x
                        </button>
                      </header>
                      <figure style={styles.cardPreview}>
                        <BlitTerminal
                          transport={transport}
                          store={store}
                          ptyId={it.ptyId}
                          palette={palette}
                          fontFamily={font}
                          fontSize={fontSize}
                          readOnly
                          style={{ width: "100%", height: "100%", pointerEvents: "none" }}
                        />
                      </figure>
                    </>
                  )}
                </li>
              ))}
              <li
                style={searching ? {
                  ...styles.exposeItem,
                  borderColor: itemBorder(selectedIdx === items.length),
                  backgroundColor: itemBg(selectedIdx === items.length),
                } : {
                  ...styles.card,
                  ...styles.cardCreate,
                  borderColor: itemBorder(selectedIdx === items.length),
                  backgroundColor: itemBg(selectedIdx === items.length),
                }}
                onClick={() => onCreate()}
                onMouseEnter={() => setSelectedIdx(items.length)}
              >
                <span style={{ fontSize: searching ? 16 : 32, opacity: 0.5 }}>+</span>
              </li>
            </>
          )}
        </ul>
      </nav>
    </div>
  );
}

function PaletteOverlay({
  current,
  onSelect,
  onPreview,
  onClose,
  dark,
}: {
  current: TerminalPalette;
  onSelect: (p: TerminalPalette) => void;
  onPreview: (p: TerminalPalette) => void;
  onClose: () => void;
  dark: boolean;
}) {
  const originalRef = useRef(current);
  const initialIdx = PALETTES.findIndex((p) => p.id === current.id);
  const [selectedIdx, setSelectedIdx] = useState(initialIdx >= 0 ? initialIdx : 0);
  const listRef = useRef<HTMLMenuElement>(null);

  const dismiss = useCallback(() => {
    onPreview(originalRef.current);
    onClose();
  }, [onPreview, onClose]);

  const preview = useCallback((idx: number) => {
    setSelectedIdx(idx);
    onPreview(PALETTES[idx]);
  }, [onPreview]);

  useEffect(() => {
    listRef.current?.focus();
  }, []);

  useEffect(() => {
    const el = listRef.current?.children[selectedIdx + 1] as HTMLElement | undefined;
    el?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          preview((selectedIdx + 1) % PALETTES.length);
          break;
        case "ArrowUp":
          e.preventDefault();
          preview((selectedIdx - 1 + PALETTES.length) % PALETTES.length);
          break;
        case "Enter":
          e.preventDefault();
          onSelect(PALETTES[selectedIdx]);
          break;
        case "Escape":
          e.preventDefault();
          dismiss();
          break;
      }
    },
    [selectedIdx, onSelect, preview, dismiss],
  );

  return (
    <div
      open
      style={styles.overlay}
      onClick={dismiss}
    >
      <menu
        ref={listRef}
        tabIndex={0}
        onKeyDown={handleKeyDown}
        style={{
          ...styles.paletteBox,
          backgroundColor: dark ? "#1e1e1e" : "#f5f5f5",
          color: dark ? "#e0e0e0" : "#333",
          outline: "none",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <li style={{ fontWeight: 600, marginBottom: 8, listStyle: "none" }}>Palette</li>
        {PALETTES.map((p, i) => (
          <li key={p.id} style={{ listStyle: "none" }}>
            <button
              onClick={() => onSelect(p)}
              onMouseEnter={() => preview(i)}
              style={{
                ...styles.paletteItem,
                backgroundColor:
                  i === selectedIdx
                    ? dark
                      ? "rgba(255,255,255,0.1)"
                      : "rgba(0,0,0,0.08)"
                    : "transparent",
              }}
            >
              <span style={styles.paletteSwatches}>
                <span
                  style={{
                    ...styles.swatch,
                    backgroundColor: `rgb(${p.bg[0]},${p.bg[1]},${p.bg[2]})`,
                    border: "1px solid rgba(128,128,128,0.3)",
                  }}
                />
                <span
                  style={{
                    ...styles.swatch,
                    backgroundColor: `rgb(${p.fg[0]},${p.fg[1]},${p.fg[2]})`,
                  }}
                />
                {p.ansi.slice(0, 8).map((c, j) => (
                  <span
                    key={j}
                    style={{
                      ...styles.swatch,
                      backgroundColor: `rgb(${c[0]},${c[1]},${c[2]})`,
                    }}
                  />
                ))}
              </span>
              <span style={{ fontSize: 13 }}>{p.name}</span>
            </button>
          </li>
        ))}
      </menu>
    </div>
  );
}

function FontOverlay({
  current,
  onSelect,
  onClose,
  dark,
}: {
  current: string;
  onSelect: (font: string) => void;
  onClose: () => void;
  dark: boolean;
}) {
  const [value, setValue] = useState(current);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  return (
    <div open style={styles.overlay} onClick={onClose}>
      <section
        style={{
          ...styles.helpBox,
          backgroundColor: dark ? "#1e1e1e" : "#f5f5f5",
          color: dark ? "#e0e0e0" : "#333",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 style={{ fontWeight: 600, marginBottom: 12, fontSize: 16 }}>Font</h2>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            onSelect(value);
          }}
          style={{ display: "flex", flexDirection: "column", gap: 10 }}
        >
          <input
            ref={inputRef}
            type="text"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder="Font family (CSS value)"
            style={{
              ...styles.exposeSearch,
              backgroundColor: dark ? "rgba(255,255,255,0.08)" : "rgba(0,0,0,0.05)",
              color: "inherit",
            }}
          />
          <span style={{ fontSize: 13, opacity: 0.6 }}>
            Preview: <span style={{ fontFamily: value || DEFAULT_FONT }}>The quick brown fox jumps over the lazy dog</span>
          </span>
          <button
            type="submit"
            style={{
              ...styles.statusBtn,
              alignSelf: "flex-end",
              padding: "4px 12px",
              border: "1px solid rgba(128,128,128,0.3)",
          
            }}
          >
            Apply
          </button>
        </form>
      </section>
    </div>
  );
}

function HelpOverlay({
  onClose,
  dark,
}: {
  onClose: () => void;
  dark: boolean;
}) {
  const mod_ = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";
  const shortcuts = [
    [`${mod_}+K`, "Expose (switch PTYs)"],
    [`${mod_}+Shift+Enter`, "New PTY in cwd"],
    [`${mod_}+Shift+W`, "Close PTY"],
    [`${mod_}+Shift+{ / }`, "Prev / Next PTY"],
    ["Shift+PageUp/Down", "Scroll"],
    [`${mod_}+Shift+P`, "Palette picker"],
    [`${mod_}+Shift+F`, "Font picker"],
    ["Ctrl+?", "Help"],
  ];
  return (
    <div
      open
      style={styles.overlay}
      onClick={onClose}
    >
      <article
        style={{
          ...styles.helpBox,
          backgroundColor: dark ? "#1e1e1e" : "#f5f5f5",
          color: dark ? "#e0e0e0" : "#333",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 style={{ fontWeight: 600, marginBottom: 12, fontSize: 16 }}>
          Keyboard shortcuts
        </h2>
        <table style={{ borderSpacing: "12px 6px" }}>
          <tbody>
            {shortcuts.map(([key, desc]) => (
              <tr key={key}>
                <td>
                  <kbd style={styles.kbd}>{key}</kbd>
                </td>
                <td style={{ fontSize: 13 }}>{desc}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </article>
    </div>
  );
}

const styles: Record<string, React.CSSProperties> = {
  authContainer: {
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    height: "100%",
    backgroundColor: "#1a1a1a",
  },
  authForm: {
    display: "flex",
    flexDirection: "column",
    gap: 8,
  },
  authInput: {
    padding: "8px 12px",
    fontSize: 16,

    border: "1px solid #444",
    backgroundColor: "#2a2a2a",
    color: "#eee",
    outline: "none",
    width: 260,
    fontFamily: "inherit",
  },
  authError: {
    color: "#f55",
    fontSize: 13,
  },
  workspace: {
    display: "flex",
    flexDirection: "column",
    height: "100%",
    width: "100%",
  },
  statusBar: {
    display: "flex",
    alignItems: "center",
    height: 28,
    padding: "0 8px",
    fontSize: 12,
    gap: 8,
    borderTop: "1px solid",
    flexShrink: 0,
    userSelect: "none",
  },
  statusBtn: {
    background: "none",
    border: "none",
    color: "inherit",
    cursor: "pointer",
    fontSize: 12,
    opacity: 0.7,
    padding: "2px 6px",

  },
  statusTitle: {
    flex: 1,
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap",
    opacity: 0.7,
  },
  statusMetrics: {
    fontSize: 11,
    opacity: 0.5,
    whiteSpace: "nowrap" as const,
    flexShrink: 0,
  },
  statusDot: {
    width: 6,
    height: 6,
    borderRadius: "50%",
    flexShrink: 0,
  },
  termContainer: {
    flex: 1,
    overflow: "hidden",
    position: "relative",
  },
  disconnected: {
    position: "absolute",
    inset: 0,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    fontSize: 32,
    fontWeight: 700,
    color: "#e33",
    pointerEvents: "none",
  },
  overlay: {
    position: "fixed",
    inset: 0,
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    backgroundColor: "rgba(0,0,0,0.5)",
    zIndex: 100,
    border: "none",
    width: "100%",
    height: "100%",
    maxWidth: "100%",
    maxHeight: "100%",
    padding: 0,
    margin: 0,
  },
  exposePanel: {
    width: "90%",
    maxWidth: 900,
    maxHeight: "80vh",

    padding: 16,
    overflow: "auto",
  },
  exposeHeader: {
    display: "flex",
    justifyContent: "space-between",
    alignItems: "center",
    marginBottom: 12,
  },
  exposeCloseBtn: {
    background: "none",
    border: "none",
    color: "inherit",
    cursor: "pointer",
    opacity: 0.5,
    fontSize: 12,
  },
  exposeCards: {
    display: "grid",
    gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))",
    gap: 12,
    listStyle: "none",
    padding: 0,
    margin: 0,
  },
  exposeSearchResults: {
    display: "grid",
    gridTemplateColumns: "minmax(0, 1fr)",
    gap: 8,
    listStyle: "none",
    padding: 0,
    margin: 0,
  },
  card: {
    border: "2px solid",
    overflow: "hidden",
    cursor: "pointer",
  },
  cardHeader: {
    display: "flex",
    justifyContent: "space-between",
    alignItems: "center",
    padding: "6px 10px",
    fontSize: 12,
    opacity: 0.8,
  },
  cardPreview: {
    margin: 0,
    overflow: "hidden",
  },
  cardCreate: {
    display: "flex",
    alignItems: "center",
    justifyContent: "center",
    minHeight: 120,
    cursor: "pointer",
  },
  exposeSearch: {
    flex: 1,
    padding: "6px 10px",
    fontSize: 14,

    border: "1px solid rgba(128,128,128,0.3)",
    outline: "none",
    fontFamily: "inherit",
  },
  exposeItem: {
    display: "flex",
    alignItems: "center",
    gap: 8,
    padding: "8px 12px",
    border: "1px solid",

    cursor: "pointer",
    listStyle: "none",
  },
  exposeItemLabel: {
    flex: 1,
    overflow: "hidden",
    textOverflow: "ellipsis",
    whiteSpace: "nowrap" as const,
    fontSize: 13,
  },
  exposeCloseItemBtn: {
    background: "none",
    border: "none",
    color: "inherit",
    cursor: "pointer",
    opacity: 0.4,
    fontSize: 14,
    padding: "0 4px",
    fontFamily: "inherit",
  },
  exposeCreateItem: {},
  badge: {
    fontSize: 10,
    padding: "1px 5px",

    backgroundColor: "rgba(88,136,255,0.3)",
    color: "inherit",
    flexShrink: 0,
  },
  paletteBox: {

    padding: 16,
    maxHeight: "80vh",
    overflow: "auto",
    minWidth: 280,
    listStyle: "none",
  },
  paletteItem: {
    display: "flex",
    alignItems: "center",
    gap: 10,
    padding: "6px 8px",
    border: "none",

    cursor: "pointer",
    width: "100%",
    color: "inherit",
    textAlign: "left" as const,
  },
  paletteSwatches: {
    display: "flex",
    gap: 2,
  },
  swatch: {
    display: "inline-block",
    width: 14,
    height: 14,

  },
  helpBox: {

    padding: 20,
    minWidth: 300,
  },
  kbd: {
    display: "inline-block",
    padding: "2px 6px",
    fontSize: 12,

    border: "1px solid rgba(128,128,128,0.4)",
    whiteSpace: "nowrap" as const,
  },
};
