import {
  useState,
  useCallback,
  useEffect,
  useRef,
} from "react";
import {
  BlitTerminal,
  useBlitContext,
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
} from "blit-react";
import type { UseBlitSessionsReturn, SearchResult } from "blit-react";
import { themeFor, layout, ui } from "./theme";

const SOURCE_LABEL: Record<number, string> = {
  [SEARCH_SOURCE_TITLE]: "Title",
  [SEARCH_SOURCE_VISIBLE]: "Terminal",
  [SEARCH_SOURCE_SCROLLBACK]: "Backlog",
};

export function ExposeOverlay({
  sessions,
  lru,
  onSelect,
  onClose,
  onCreate,
  searchResultsCbRef,
}: {
  sessions: UseBlitSessionsReturn;
  lru: number[];
  onSelect: (id: number) => void;
  onClose: () => void;
  onCreate: (command?: string) => void;
  searchResultsCbRef: React.RefObject<((reqId: number, results: SearchResult[]) => void) | null>;
}) {
  // Sort by LRU: most recently focused first, then any not in LRU.
  const notClosed = sessions.sessions.filter((s) => s.state !== "closed");
  const lruIndex = new Map(lru.map((id, i) => [id, i]));
  const visible = [...notClosed].sort((a, b) => {
    const ai = lruIndex.get(a.ptyId) ?? Infinity;
    const bi = lruIndex.get(b.ptyId) ?? Infinity;
    return ai - bi;
  });
  const { palette, fontFamily: font } = useBlitContext();
  const dark = palette?.dark ?? true;
  const theme = themeFor(dark);
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

  const sessionsByPtyId = new Map(visible.map((s) => [s.ptyId, s]));

  const items: { ptyId: number; title: string; exited: boolean; context?: string; source?: number }[] = searching && searchResults
    ? searchResults
        .filter((r) => sessionsByPtyId.has(r.ptyId))
        .map((r) => ({
          ptyId: r.ptyId,
          title: sessionsByPtyId.get(r.ptyId)!.title ?? `PTY ${r.ptyId}`,
          exited: sessionsByPtyId.get(r.ptyId)!.state === "exited",
          context: r.context,
          source: r.primarySource,
        }))
    : visible.map((s) => ({
        ptyId: s.ptyId,
        title: s.title ?? `PTY ${s.ptyId}`,
        exited: s.state === "exited",
      }));

  const itemCount = isCommand ? 0 : items.length + 1;
  // Default to the most recent PTY that isn't the currently focused one,
  // so Cmd+K Enter switches back — like Alt-Tab.
  const defaultIdx = items.findIndex((it) => it.ptyId !== sessions.focusedPtyId);
  const [selectedIdx, setSelectedIdx] = useState(defaultIdx >= 0 ? defaultIdx : 0);
  const prevQueryRef = useRef(query);

  useEffect(() => {
    if (prevQueryRef.current !== query) {
      prevQueryRef.current = query;
      setSelectedIdx(0);
    }
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
    selected ? theme.hoverBg : "transparent";
  const itemBorder = (selected: boolean) =>
    selected ? theme.accent : theme.border;

  return (
    <div role="dialog" aria-label="Expose" style={layout.overlay} onClick={onClose}>
      <nav
        style={{
          width: "90%",
          maxWidth: 900,
          maxHeight: "80vh",
          padding: 16,
          overflow: "auto",
          backgroundColor: theme.panelBg,
          color: theme.fg,
          fontFamily: font,
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <header style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          marginBottom: 12,
        }}>
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
                borderRadius: 4,
                fontSize: 13,
              }}
              onClick={() => onCreate(commandText || undefined)}
            >
              Run
            </button>
          )}
          <button style={{
            background: "none",
            border: "none",
            color: "inherit",
            cursor: "pointer",
            fontFamily: "inherit",
            opacity: 0.5,
            fontSize: 12,
          }} onClick={onClose}>
            Esc
          </button>
        </header>
        {!isCommand && (
        <ul ref={listRef} style={searching ? {
          display: "grid",
          gridTemplateColumns: "minmax(0, 1fr)",
          gap: 8,
          listStyle: "none",
          padding: 0,
          margin: 0,
        } : {
          display: "grid",
          gridTemplateColumns: "repeat(auto-fill, minmax(260px, 1fr))",
          gap: 12,
          listStyle: "none",
          padding: 0,
          margin: 0,
        }}>
              {items.map((it, i) => (
                <li
                  key={it.ptyId}
                  style={searching ? {
                    display: "flex",
                    alignItems: "center",
                    gap: 8,
                    padding: "8px 12px",
                    border: "1px solid",
                    cursor: "pointer",
                    listStyle: "none",
                    borderColor: itemBorder(i === selectedIdx),
                    backgroundColor: itemBg(i === selectedIdx),
                  } : {
                    border: "2px solid",
                    overflow: "hidden",
                    cursor: "pointer",
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
                          ptyId={it.ptyId}
                          readOnly
                          style={{ width: "100%", height: "100%", pointerEvents: "none" }}
                        />
                      </figure>
                      <div style={{ flex: 1, minWidth: 0 }}>
                        <div style={{ display: "flex", alignItems: "center", gap: 6 }}>
                          <span style={{
                            flex: 1,
                            overflow: "hidden",
                            textOverflow: "ellipsis",
                            whiteSpace: "nowrap" as const,
                            fontSize: 13,
                          }}>{it.title}</span>
                          {it.source != null && (
                            <mark style={ui.badge}>{SOURCE_LABEL[it.source] ?? "Match"}</mark>
                          )}
                          {it.exited && (
                            <mark style={{ ...ui.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>Exited</mark>
                          )}
                          {it.ptyId === sessions.focusedPtyId && (
                            <mark style={ui.badge}>Lead</mark>
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
                          sessions.closePty(it.ptyId);
                        }}
                      >
                        x
                      </button>
                    </>
                  ) : (
                    <>
                      <header style={{
                        display: "flex",
                        justifyContent: "space-between",
                        alignItems: "center",
                        padding: "6px 10px",
                        fontSize: 12,
                        opacity: 0.8,
                      }}>
                        <span style={{
                          flex: 1,
                          overflow: "hidden",
                          textOverflow: "ellipsis",
                          whiteSpace: "nowrap" as const,
                          fontSize: 13,
                        }}>{it.title}</span>
                        {it.exited && (
                          <mark style={{ ...ui.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>Exited</mark>
                        )}
                        {it.ptyId === sessions.focusedPtyId && (
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
                            sessions.closePty(it.ptyId);
                          }}
                        >
                          x
                        </button>
                      </header>
                      <figure style={{ margin: 0, overflow: "hidden" }}>
                        <BlitTerminal
                          ptyId={it.ptyId}
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
                  display: "flex",
                  alignItems: "center",
                  gap: 8,
                  padding: "8px 12px",
                  border: "1px solid",
                  cursor: "pointer",
                  listStyle: "none",
                  borderColor: itemBorder(selectedIdx === items.length),
                  backgroundColor: itemBg(selectedIdx === items.length),
                } : {
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
      </nav>
    </div>
  );
}
