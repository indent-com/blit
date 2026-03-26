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
import { styles } from "./styles";

const SOURCE_LABEL: Record<number, string> = {
  [SEARCH_SOURCE_TITLE]: "Title",
  [SEARCH_SOURCE_VISIBLE]: "Terminal",
  [SEARCH_SOURCE_SCROLLBACK]: "Backlog",
};

export function ExposeOverlay({
  sessions,
  onSelect,
  onClose,
  onCreate,
  searchResultsCbRef,
}: {
  sessions: UseBlitSessionsReturn;
  onSelect: (id: number) => void;
  onClose: () => void;
  onCreate: (command?: string) => void;
  searchResultsCbRef: React.RefObject<((reqId: number, results: SearchResult[]) => void) | null>;
}) {
  // Show active first, then exited at the end
  const visible = [
    ...sessions.sessions.filter((s) => s.state === "active"),
    ...sessions.sessions.filter((s) => s.state === "exited"),
  ];
  const { palette, fontFamily: font } = useBlitContext();
  const dark = palette?.dark ?? true;
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
    <div role="dialog" aria-label="Expose" style={styles.overlay} onClick={onClose}>
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
                          ptyId={it.ptyId}
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
                          {it.exited && (
                            <mark style={{ ...styles.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>Exited</mark>
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
                        {it.exited && (
                          <mark style={{ ...styles.badge, backgroundColor: "rgba(255,100,100,0.3)" }}>Exited</mark>
                        )}
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
