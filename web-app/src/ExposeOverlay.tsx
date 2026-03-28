import {
  useState,
  useCallback,
  useEffect,
  useRef,
  type KeyboardEvent,
} from "react";
import {
  BlitTerminal,
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
  useBlitContext,
  useBlitWorkspace,
} from "blit-react";
import type {
  BlitSearchResult,
  BlitSession,
  SessionId,
  TerminalPalette,
} from "blit-react";
import { overlayChromeStyles, themeFor, ui } from "./theme";
import { OverlayBackdrop, OverlayPanel } from "./Overlay";

const SOURCE_LABEL: Record<number, string> = {
  [SEARCH_SOURCE_TITLE]: "Title",
  [SEARCH_SOURCE_VISIBLE]: "Terminal",
  [SEARCH_SOURCE_SCROLLBACK]: "Backlog",
};

export function ExposeOverlay({
  sessions,
  focusedSessionId,
  lru,
  palette,
  onSelect,
  onClose,
  onCreate,
}: {
  sessions: readonly BlitSession[];
  focusedSessionId: SessionId | null;
  lru: SessionId[];
  palette: TerminalPalette;
  onSelect: (sessionId: SessionId) => void;
  onClose: () => void;
  onCreate: (command?: string) => void;
}) {
  const workspace = useBlitWorkspace();
  const notClosed = sessions.filter((session) => session.state !== "closed");
  const lruIndex = new Map(lru.map((id, index) => [id, index]));
  const visible = [...notClosed].sort((left, right) => {
    const leftIndex = lruIndex.get(left.id) ?? Infinity;
    const rightIndex = lruIndex.get(right.id) ?? Infinity;
    return leftIndex - rightIndex;
  });

  const { fontFamily: font } = useBlitContext();
  const dark = palette.dark;
  const theme = themeFor(palette);
  const chrome = overlayChromeStyles(theme, dark);
  const [query, setQuery] = useState("");
  const [searchResults, setSearchResults] = useState<BlitSearchResult[] | null>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const requestIdRef = useRef(0);

  const isCommand = query.startsWith(">");
  const commandText = isCommand ? query.slice(1).trim() : "";
  const searching = !isCommand && query.length > 0;
  const showConnectionBadge = new Set(notClosed.map((session) => session.connectionId)).size > 1;

  useEffect(() => {
    if (!searching) {
      setSearchResults(null);
      return;
    }

    let cancelled = false;
    const requestId = ++requestIdRef.current;
    workspace.search(query).then((results) => {
      if (!cancelled && requestId === requestIdRef.current) {
        setSearchResults(results);
      }
    }).catch(() => {
      if (!cancelled && requestId === requestIdRef.current) {
        setSearchResults([]);
      }
    });

    return () => {
      cancelled = true;
    };
  }, [query, searching, workspace]);

  useEffect(() => {
    searchRef.current?.focus();
  }, []);

  const sessionsById = new Map(visible.map((session) => [session.id, session]));
  const items: Array<{
    sessionId: SessionId;
    ptyId: number;
    connectionId: string;
    title: string;
    exited: boolean;
    context?: string;
    source?: number;
  }> = searching && searchResults
    ? searchResults
        .filter((result) => sessionsById.has(result.sessionId))
        .map((result) => {
          const session = sessionsById.get(result.sessionId)!;
          return {
            sessionId: result.sessionId,
            ptyId: result.ptyId,
            connectionId: result.connectionId,
            title: session.title ?? session.tag ?? `PTY ${result.ptyId}`,
            exited: session.state === "exited",
            context: result.context,
            source: result.primarySource,
          };
        })
    : visible.map((session) => ({
        sessionId: session.id,
        ptyId: session.ptyId,
        connectionId: session.connectionId,
        title: session.title ?? session.tag ?? `PTY ${session.ptyId}`,
        exited: session.state === "exited",
      }));

  const itemCount = isCommand ? 0 : items.length + 1;
  const altIdx = items.findIndex((item) => item.sessionId !== focusedSessionId);
  const defaultIdx = altIdx >= 0 ? altIdx : items.length;
  const [selectedIdx, setSelectedIdx] = useState(defaultIdx);
  const prevQueryRef = useRef(query);

  useEffect(() => {
    if (prevQueryRef.current !== query) {
      prevQueryRef.current = query;
      setSelectedIdx(0);
    }
  }, [query]);

  const gridColsRef = useRef(1);
  useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const style = getComputedStyle(list);
    const cols = style.gridTemplateColumns
      .split(/\s+/)
      .filter((value) => value && value !== "none").length;
    gridColsRef.current = Math.max(1, cols);
  });

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      const gridCols = gridColsRef.current;
      switch (e.key) {
        case "ArrowRight":
          e.preventDefault();
          setSelectedIdx((index) => (index + 1) % itemCount);
          break;
        case "ArrowLeft":
          e.preventDefault();
          setSelectedIdx((index) => (index - 1 + itemCount) % itemCount);
          break;
        case "ArrowDown":
          e.preventDefault();
          setSelectedIdx((index) => Math.min(index + gridCols, itemCount - 1));
          break;
        case "ArrowUp":
          e.preventDefault();
          setSelectedIdx((index) => Math.max(index - gridCols, 0));
          break;
        case "Enter":
          e.preventDefault();
          if (isCommand) {
            onCreate(commandText || undefined);
          } else if (selectedIdx < items.length) {
            onSelect(items[selectedIdx].sessionId);
          } else {
            onCreate();
          }
          break;
        case "w":
        case "W":
          if (e.ctrlKey || e.metaKey) {
            e.preventDefault();
            if (!isCommand && selectedIdx < items.length) {
              void workspace.closeSession(items[selectedIdx].sessionId);
              setSelectedIdx((index) => Math.min(index, Math.max(0, items.length - 2)));
            }
          }
          break;
      }
    },
    [commandText, isCommand, itemCount, items, onCreate, onSelect, selectedIdx, workspace],
  );

  useEffect(() => {
    const element = listRef.current?.children[selectedIdx] as HTMLElement | undefined;
    element?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx]);

  const itemBg = (selected: boolean) =>
    selected ? theme.hoverBg : "transparent";
  const itemBorder = (selected: boolean) =>
    selected ? theme.accent : theme.border;

  return (
    <OverlayBackdrop palette={palette} label="Expose" onClose={onClose}>
      <OverlayPanel
        palette={palette}
        style={{
          width: "min(90vw, 900px)",
          maxWidth: 900,
          backgroundColor: theme.panelBg,
          fontFamily: font,
        }}
      >
        <header
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
            marginBottom: 12,
          }}
        >
          <input
            ref={searchRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder="Search terminals, or >command"
            style={{
              ...ui.input,
              backgroundColor: theme.inputBg,
              color: "inherit",
            }}
          />
          {isCommand && (
            <button
              style={{
                background: "none",
                border: "none",
                cursor: "pointer",
                fontFamily: "inherit",
                opacity: 1,
                backgroundColor: theme.accent,
                color: "#fff",
                padding: "4px 10px",
                fontSize: 13,
              }}
              onClick={() => onCreate(commandText || undefined)}
            >
              Run
            </button>
          )}
          <button style={chrome.closeButton} onClick={onClose}>
            Esc
          </button>
        </header>
        {!isCommand && (
          <ul
            ref={listRef}
            style={searching
              ? {
                  display: "grid",
                  gridTemplateColumns: "minmax(0, 1fr)",
                  gap: 8,
                  listStyle: "none",
                  padding: 0,
                  margin: 0,
                }
              : {
                  display: "grid",
                  gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))",
                  gap: 12,
                  listStyle: "none",
                  padding: 0,
                  margin: 0,
                }}
          >
            {items.map((item, index) => (
              <li
                key={item.sessionId}
                style={searching
                  ? {
                      display: "flex",
                      alignItems: "center",
                      gap: 8,
                      padding: "8px 12px",
                      border: "1px solid",
                      cursor: "pointer",
                      listStyle: "none",
                      borderColor: itemBorder(index === selectedIdx),
                      backgroundColor: itemBg(index === selectedIdx),
                    }
                  : {
                      border: "2px solid",
                      overflow: "hidden",
                      cursor: "pointer",
                      borderColor: itemBorder(index === selectedIdx),
                      backgroundColor: itemBg(index === selectedIdx),
                    }}
                onClick={() => onSelect(item.sessionId)}
                onMouseEnter={() => setSelectedIdx(index)}
              >
                {searching ? (
                  <>
                    <figure style={{ margin: 0, width: 120, height: 68, flexShrink: 0, overflow: "hidden" }}>
                      <BlitTerminal
                        sessionId={item.sessionId}
                        readOnly
                        style={{ width: "100%", height: "100%", pointerEvents: "none" }}
                      />
                    </figure>
                    <div style={{ flex: 1, minWidth: 0 }}>
                      <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                        <span
                          style={{
                            flex: 1,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                            fontSize: 13,
                          }}
                        >
                          {item.title}
                        </span>
                        {showConnectionBadge && (
                          <mark style={ui.badge}>{item.connectionId}</mark>
                        )}
                        {item.source != null && (
                          <mark style={ui.badge}>{SOURCE_LABEL[item.source] ?? "Match"}</mark>
                        )}
                        {item.exited && (
                          <mark style={{ ...ui.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>
                            Exited
                          </mark>
                        )}
                        {item.sessionId === focusedSessionId && (
                          <mark style={ui.badge}>Lead</mark>
                        )}
                      </div>
                      {item.context && (
                        <div
                          style={{
                            fontSize: 11,
                            opacity: 0.6,
                            marginTop: 2,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap",
                          }}
                        >
                          {item.context}
                        </div>
                      )}
                    </div>
                    <button
                      style={{
                        background: "none",
                        border: "none",
                        color: "inherit",
                        cursor: "pointer",
                        opacity: 0.4,
                        fontSize: 14,
                        padding: "0 4px",
                        fontFamily: "inherit",
                      }}
                      title="Close (Ctrl+W)"
                      onClick={(e) => {
                        e.stopPropagation();
                        void workspace.closeSession(item.sessionId);
                      }}
                    >
                      x
                    </button>
                  </>
                ) : (
                  <>
                    <header
                      style={{
                        display: "flex",
                        justifyContent: "space-between",
                        alignItems: "center",
                        padding: "6px 10px",
                        fontSize: 12,
                        opacity: 0.8,
                        gap: 6,
                      }}
                    >
                      <span
                        style={{
                          flex: 1,
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap",
                          fontSize: 13,
                        }}
                      >
                        {item.title}
                      </span>
                      {showConnectionBadge && (
                        <mark style={ui.badge}>{item.connectionId}</mark>
                      )}
                      {item.exited && (
                        <mark style={{ ...ui.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>
                          Exited
                        </mark>
                      )}
                      {item.sessionId === focusedSessionId && (
                        <mark style={ui.badge}>Lead</mark>
                      )}
                      <button
                        style={{
                          background: "none",
                          border: "none",
                          color: "inherit",
                          cursor: "pointer",
                          opacity: 0.4,
                          fontSize: 14,
                          padding: "0 4px",
                          fontFamily: "inherit",
                        }}
                        title="Close (Ctrl+W)"
                        onClick={(e) => {
                          e.stopPropagation();
                          void workspace.closeSession(item.sessionId);
                        }}
                      >
                        x
                      </button>
                    </header>
                    <figure style={{ margin: 0, overflow: "hidden" }}>
                      <BlitTerminal
                        sessionId={item.sessionId}
                        readOnly
                        style={{ width: "100%", height: "100%", pointerEvents: "none" }}
                      />
                    </figure>
                  </>
                )}
              </li>
            ))}
            <li
              style={searching
                ? {
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    padding: "8px 12px",
                    border: "1px solid",
                    cursor: "pointer",
                    listStyle: "none",
                    borderColor: itemBorder(selectedIdx === items.length),
                    backgroundColor: itemBg(selectedIdx === items.length),
                  }
                : {
                    border: "2px solid",
                    overflow: "hidden",
                    cursor: "pointer",
                    display: "flex",
                    alignItems: "center",
                    justifyContent: "center",
                    minHeight: 120,
                    borderColor: itemBorder(selectedIdx === items.length),
                    backgroundColor: itemBg(selectedIdx === items.length),
                  }}
              onClick={() => onCreate()}
              onMouseEnter={() => setSelectedIdx(items.length)}
            >
              <span style={{ fontSize: searching ? 16 : 32, opacity: 0.5 }}>+</span>
            </li>
          </ul>
        )}
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
