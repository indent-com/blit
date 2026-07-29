/**
 * SearchPanel — project-wide content search (docs/design/fs-grep.md).
 *
 * The `@` file picker scores a shipped index locally, so it can answer per
 * keystroke for free. Content search cannot: every query is a real walk on
 * the server. So this debounces, cancels the previous query on each new
 * one, and says plainly when a result set was clipped by a budget.
 *
 * Ignore rules apply by default; the ⊘ toggle widens the search to
 * gitignored files, which then rank last and render dimmed. On a repo with
 * build output that toggle is the difference between milliseconds and
 * seconds, which is why it is off unless asked for.
 *
 * One row per line, however many matches land on it, each match
 * individually highlighted and individually clickable — a line matching
 * twice should not appear twice, but clicking the second hit should still
 * go to the second hit.
 */

import {
  createEffect,
  untrack,
  createMemo,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import type { FsGrepFile, TerminalPalette } from "@blit-sh/core";
import { editorAssignment } from "@blit-sh/core/bsp";
import type { JSX } from "solid-js";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import type { IdeSession } from "./session";
import { setReveal } from "./reveal";
import { startTileDrag } from "./tileDrag";
import {
  searchQuery,
  setSearchQuery,
  searchCaseSensitive,
  setSearchCaseSensitive,
  searchRegex,
  setSearchRegex,
  searchNoIgnore,
  setSearchNoIgnore,
  searchWord,
  setSearchWord,
  searchFiles,
  setSearchFiles,
  searchTruncated,
  setSearchTruncated,
  searchError,
  setSearchError,
  searchBusy,
  setSearchBusy,
  searchKey,
  setSearchKey,
  searchKeyFor,
  setSearchInputFocused,
} from "./searchStore";
import { langForFile } from "./languages";
import { buildDiffHighlighter, lineColors } from "./diff-highlight";

/** One rendered line. Results are flattened to a single homogeneous list so
 *  the fixed-row-height windowing below stays a slice of one array. */
type Row =
  | { kind: "file"; file: FsGrepFile }
  | {
      kind: "match";
      file: FsGrepFile;
      /** 0-based line this row shows. One row per line, however many
       *  matches land on it. */
      line: number;
      text: string;
      /** Every match touching this line, as UTF-8 byte offsets into
       *  `text`, each carrying where clicking it should land. A match
       *  spanning newlines contributes one span to each line it covers,
       *  all pointing at the match's start. */
      spans: {
        start: number;
        end: number;
        jumpLine: number;
        jumpCol: number;
      }[];
    };

const OVERSCAN = 10;

export function SearchPanel(props: {
  session: IdeSession | null;
  theme: Theme;
  palette: TerminalPalette;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onOpenTile?: (assignment: string) => void;
  /** Dismiss the pane. */
  onClose?: () => void;
  /** Bumped every time the shortcut is pressed; focuses the input
   *  even when the pane was already open. */
  focusNonce?: number;
}) {
  // All state lives in ./searchStore, at module scope: the pane unmounts
  // when dismissed, and the query and results should survive that.
  // Ignore rules apply by default — on a repo with build output that is
  // the difference between milliseconds and seconds — and `noIgnore`
  // widens the search to ignored files, which then rank last.
  const query = searchQuery;
  const setQuery = setSearchQuery;
  const caseSensitive = searchCaseSensitive;
  const setCaseSensitive = setSearchCaseSensitive;
  const regex = searchRegex;
  const setRegex = setSearchRegex;
  const noIgnore = searchNoIgnore;
  const setNoIgnore = setSearchNoIgnore;
  const word = searchWord;
  const setWord = setSearchWord;
  const files = searchFiles;
  const setFiles = setSearchFiles;
  const truncated = searchTruncated;
  const setTruncated = setSearchTruncated;
  const error = searchError;
  const setError = setSearchError;
  const searching = searchBusy;
  const setSearching = setSearchBusy;

  let inputEl: HTMLInputElement | undefined;

  // Same highlighter the diff and commit views use, so a hit reads
  // like the file it came from. `langForFile` is reactive: a grammar
  // that loads after the results land re-colours them in place.
  const highlighter = createMemo(() =>
    buildDiffHighlighter(props.theme, props.palette),
  );
  const encoder = new TextEncoder();
  const decoder = new TextDecoder();

  // The match highlight, same construction the diff uses for intraline
  // changes. `selectedBg` was too faint to find at a glance in a wall of
  // results — the point of a hit list is that the hit is obvious.
  const matchBg = () =>
    `color-mix(in srgb, ${props.theme.warning} 34%, transparent)`;

  // The pane unmounts when dismissed; the shared flag must not be
  // left reading "focused" after it is gone.
  onCleanup(() => setSearchInputFocused(false));

  // A memo over the root *string*, not the session object. IdeSessions are
  // keyed per anchored file (`p <conn> <path>`), so focusing a different
  // pane hands us a different session even inside one project — and an
  // effect that read the session would re-run and re-issue the whole walk
  // every time you switched panes. The root is what actually scopes a
  // search, and a memo over a string only notifies when it really changes.
  const root = createMemo(() => props.session?.root() ?? null);

  createEffect(() => {
    const q = query().trim();
    const r = root();
    // Re-run on toggle changes too.
    const opts = {
      caseSensitive: caseSensitive(),
      regex: regex(),
      noIgnore: noIgnore(),
      word: word(),
    };
    // Read untracked: which session object carries out the walk is an
    // implementation detail, and subscribing to it is the bug above.
    const session = untrack(() => props.session);
    if (!q || !r || !session) {
      setFiles([]);
      setTruncated(false);
      setError(null);
      setSearching(false);
      setSearchKey(null);
      return;
    }
    // Reopening the pane re-runs this effect. If nothing that defines the
    // search changed, the results on screen are still the answer — don't
    // pay for the walk again.
    const key = searchKeyFor(r, q, opts);
    if (key === searchKey()) {
      setSearching(false);
      return;
    }
    let cancelled = false;
    setSearching(true);
    // Every keystroke is a server walk, so debounce; Solid disposes the
    // previous run on the next keystroke, which is the cancellation.
    const timer = setTimeout(() => {
      session
        .grep(q, opts)
        .then((res) => {
          if (cancelled) return;
          setFiles(res.files);
          setTruncated(res.truncated);
          setError(null);
          setSearching(false);
          setSearchKey(key);
        })
        .catch((e: unknown) => {
          if (cancelled) return;
          setFiles([]);
          setTruncated(false);
          // The server's wording, so an uncompilable regex explains itself.
          setError(e instanceof Error ? e.message : String(e));
          setSearching(false);
          setSearchKey(null);
        });
    }, 200);
    onCleanup(() => {
      cancelled = true;
      clearTimeout(timer);
    });
  });

  // Focus + select on every invoke, so the shortcut lands in the field
  // and a second press replaces the previous query rather than
  // appending to it.
  createEffect(() => {
    props.focusNonce;
    queueMicrotask(() => {
      inputEl?.focus();
      inputEl?.select();
    });
  });

  const rows = createMemo<Row[]>(() => {
    const out: Row[] = [];
    for (const file of files()) {
      out.push({ kind: "file", file });
      // One row per line, not per match: two hits on one line are one row
      // with two highlights, and a match spanning newlines contributes a
      // span to each line it covers. Insertion order is the server's,
      // which is already by position.
      const byLine = new Map<number, Extract<Row, { kind: "match" }>>();
      for (const m of file.matches) {
        const lines = m.text.split("\n");
        for (let i = 0; i < lines.length; i++) {
          const lineNo = m.line + i;
          let row = byLine.get(lineNo);
          if (!row) {
            row = {
              kind: "match",
              file,
              line: lineNo,
              text: lines[i],
              spans: [],
            };
            byLine.set(lineNo, row);
            out.push(row);
          }
          row.spans.push({
            start: i === 0 ? m.col : 0,
            end:
              i === lines.length - 1
                ? m.endCol
                : encoder.encode(lines[i]).length,
            jumpLine: m.line,
            jumpCol: m.col,
          });
        }
      }
    }
    return out;
  });

  // Keyboard selection: an index into `rows()`, always on a match row —
  // file headers are labels, not destinations. Reset whenever the result
  // set changes so a new search starts at its first hit.
  const [selected, setSelected] = createSignal(-1);
  const firstMatch = () => rows().findIndex((r) => r.kind === "match");
  createEffect(() => {
    rows();
    setSelected(firstMatch());
  });

  const moveSelection = (dir: 1 | -1) => {
    const all = rows();
    if (!all.length) return;
    let i = selected();
    if (i < 0) i = dir === 1 ? -1 : 0;
    for (let n = 0; n < all.length; n++) {
      i = (i + dir + all.length) % all.length;
      if (all[i].kind === "match") break;
    }
    setSelected(i);
    // The list is windowed, so the row may not be rendered — scroll by
    // arithmetic rather than looking for an element that isn't there.
    const top = headerH() + i * rowH();
    const view = viewH();
    if (top < scrollEl.scrollTop) scrollEl.scrollTop = top;
    else if (top + rowH() > scrollEl.scrollTop + view)
      scrollEl.scrollTop = top + rowH() - view;
  };

  /** Open the keyboard selection at its first match. */
  const openSelected = () => {
    const row = rows()[selected()];
    if (row?.kind !== "match") return;
    const first = row.spans[0];
    openAt(row, first?.jumpLine ?? row.line, first?.jumpCol ?? 0);
  };

  const matchCount = () => files().reduce((n, f) => n + f.matches.length, 0);

  // Absolute path for a hit. Grep paths are relative to the searched root.
  const abs = (rel: string): string | null => {
    const r = root();
    if (!r) return null;
    return rel.startsWith("/")
      ? rel
      : `${r.replace(/\/+$/, "")}/${rel.replace(/^\/+/, "")}`;
  };

  /** Arm the reveal, then hand back the assignment — same order as the
   *  Problems panel, so click and drag land on the same place. Relocation
   *  keys on the line's text when the jump target is this very line;
   *  for a multi-line match's later rows it falls back to the number. */
  const assignmentFor = (
    row: Extract<Row, { kind: "match" }>,
    jumpLine: number,
    jumpCol: number,
  ): string | null => {
    const s = props.session;
    const a = abs(row.file.path);
    if (!s || !a) return null;
    setReveal(s.connectionId, a, {
      text: jumpLine === row.line ? row.text : "",
      line: jumpLine + 1, // grep is 0-based, reveal is 1-based
      col: jumpCol,
    });
    return editorAssignment(s.connectionId, a);
  };

  const openAt = (
    row: Extract<Row, { kind: "match" }>,
    jumpLine: number,
    jumpCol: number,
  ) => {
    const a = assignmentFor(row, jumpLine, jumpCol);
    if (a) props.onOpenTile?.(a);
  };

  /**
   * Render one result line: syntax colours from the shared highlighter,
   * every match on the line highlighted, and each highlight individually
   * clickable so clicking the second hit goes to the second hit.
   *
   * Hand-rolled rather than `renderHunkText` (which the diff uses) because
   * that renders spans without letting a caller attach a handler to one —
   * and per-span handlers are the whole point here.
   */
  const renderLine = (row: Extract<Row, { kind: "match" }>) => {
    const bytes = encoder.encode(row.text);
    const colors = lineColors(
      row.text,
      langForFile(row.file.path),
      highlighter(),
    );
    // Byte offsets → char offsets, the conversion renderHunkText also does.
    const toChar = (b: number) =>
      decoder.decode(bytes.subarray(0, Math.min(Math.max(b, 0), bytes.length)))
        .length;
    // Which match (if any) owns each character.
    const owner = new Array<number>(row.text.length).fill(-1);
    row.spans.forEach((sp, i) => {
      for (let c = toChar(sp.start); c < toChar(sp.end); c++) owner[c] = i;
    });

    const parts: JSX.Element[] = [];
    let i = 0;
    while (i < row.text.length) {
      const color = colors[i] ?? null;
      const own = owner[i];
      let j = i + 1;
      while (
        j < row.text.length &&
        (colors[j] ?? null) === color &&
        owner[j] === own
      )
        j++;
      const seg = row.text.slice(i, j);
      if (own < 0) {
        parts.push(<span style={{ color: color ?? undefined }}>{seg}</span>);
      } else {
        const sp = row.spans[own];
        parts.push(
          <span
            onClick={(e) => {
              // Beat the row's own handler, which targets the first match.
              e.stopPropagation();
              openAt(row, sp.jumpLine, sp.jumpCol);
            }}
            style={{
              color: color ?? undefined,
              background: matchBg(),
              "border-radius": "2px",
              cursor: props.onOpenTile ? "pointer" : "default",
            }}
          >
            {seg}
          </span>,
        );
      }
      i = j;
    }
    return parts;
  };

  // -- windowing: fixed-height rows between spacers --------------------
  let scrollEl!: HTMLDivElement;
  let headerEl: HTMLDivElement | undefined;
  let resizeObs: ResizeObserver | undefined;
  const [scrollTop, setScrollTop] = createSignal(0);
  const [viewH, setViewH] = createSignal(0);
  const [headerH, setHeaderH] = createSignal(0);
  const rowH = () => Math.round(props.fontSize * 1.35);
  const headerRef = (el: HTMLDivElement) => {
    headerEl = el;
    resizeObs?.observe(el);
  };
  onMount(() => {
    resizeObs = new ResizeObserver(() => {
      setViewH(scrollEl.clientHeight);
      if (headerEl) setHeaderH(headerEl.offsetHeight);
    });
    resizeObs.observe(scrollEl);
    if (headerEl) resizeObs.observe(headerEl);
    setViewH(scrollEl.clientHeight);
    if (headerEl) setHeaderH(headerEl.offsetHeight);
    onCleanup(() => resizeObs?.disconnect());
  });
  const window_ = createMemo(() => {
    const total = rows().length;
    const h = rowH();
    const top = Math.max(0, scrollTop() - headerH());
    const start = Math.min(total, Math.max(0, Math.floor(top / h) - OVERSCAN));
    const end = Math.min(
      total,
      Math.max(start, Math.ceil((top + viewH()) / h) + OVERSCAN),
    );
    return { start, end, total };
  });

  const toggleStyle = (on: boolean) => ({
    background: on ? props.theme.hoverBg : "transparent",
    border: "none",
    color: on ? props.theme.fg : props.theme.dimFg,
    cursor: "pointer",
    padding: "2px 6px",
    "font-family": props.fontFamily,
    "font-size": `${props.scale.sm}px`,
    "border-radius": "2px",
  });

  return (
    <div
      ref={scrollEl}
      onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
      style={{
        // `auto` basis, not 0: the pane above sizes itself to this content
        // until it hits its cap, and a 0 basis would collapse it instead.
        // `0 1 auto` rather than `1 1 auto` so an empty result list does
        // not stretch to fill space the pane did not need.
        flex: "0 1 auto",
        "min-height": 0,
        "overflow-y": "auto",
        ...scrollbarStyle(props.theme),
      }}
    >
      <div
        ref={headerRef}
        style={{
          padding: `${props.scale.tightGap}px ${props.scale.tightGap}px 0`,
        }}
      >
        <div style={{ display: "flex", gap: `${props.scale.tightGap}px` }}>
          <input
            ref={inputEl}
            value={query()}
            placeholder="search file contents…"
            spellcheck={false}
            autocapitalize="off"
            onInput={(e) => setQuery(e.currentTarget.value)}
            onFocus={() => setSearchInputFocused(true)}
            onBlur={() => setSearchInputFocused(false)}
            onKeyDown={(e) => {
              // Without this every keystroke also reaches the global
              // shortcut handler.
              e.stopPropagation();
              // Escape releases focus but leaves the pane up: the results
              // are usually the thing you wanted to keep looking at while
              // you go type somewhere else. Dismissing is Ctrl+Shift+F or
              // the ✕.
              if (e.key === "Escape") {
                e.currentTarget.blur();
              } else if (e.key === "ArrowDown") {
                e.preventDefault();
                moveSelection(1);
              } else if (e.key === "ArrowUp") {
                e.preventDefault();
                moveSelection(-1);
              } else if (e.key === "Enter") {
                e.preventDefault();
                openSelected();
              }
            }}
            style={{
              flex: 1,
              "min-width": 0,
              "box-sizing": "border-box",
              background: "transparent",
              color: props.theme.fg,
              border: `1px solid ${props.theme.subtleBorder}`,
              "border-radius": "2px",
              outline: "none",
              padding: "1px 4px",
              "font-family": props.fontFamily,
              "font-size": `${props.scale.md}px`,
            }}
          />
          <button
            title="Match case"
            onClick={() => setCaseSensitive((v) => !v)}
            style={toggleStyle(caseSensitive())}
          >
            Aa
          </button>
          <button
            title="Regular expression"
            onClick={() => setRegex((v) => !v)}
            style={toggleStyle(regex())}
          >
            .*
          </button>
          <button
            title="Whole words only"
            onClick={() => setWord((v) => !v)}
            style={toggleStyle(word())}
          >
            ab|
          </button>
          <button
            title="Include gitignored files (slower; they rank last)"
            onClick={() => setNoIgnore((v) => !v)}
            style={toggleStyle(noIgnore())}
          >
            ⊘
          </button>
          <Show when={props.onClose}>
            <button
              title="Close search (Esc)"
              onClick={() => props.onClose?.()}
              style={toggleStyle(false)}
            >
              ✕
            </button>
          </Show>
        </div>
        <Show when={error()}>
          <div
            style={{
              "font-size": `${props.scale.sm}px`,
              color: props.theme.errorText,
              padding: "2px 2px 0",
            }}
          >
            {error()}
          </div>
        </Show>
        <Show when={!error() && query().trim()}>
          <div
            style={{
              "font-size": `${props.scale.sm}px`,
              color: props.theme.dimFg,
              padding: "2px 2px 0",
            }}
          >
            {searching()
              ? "Searching…"
              : rows().length === 0
                ? "No matches."
                : `${matchCount()} in ${files().length} file${files().length === 1 ? "" : "s"}${
                    truncated() ? " — clipped by a budget" : ""
                  }`}
          </div>
        </Show>
        <Show when={!root()}>
          <div
            style={{
              "font-size": `${props.scale.sm}px`,
              color: props.theme.dimFg,
              padding: "2px 2px 0",
            }}
          >
            No root selected.
          </div>
        </Show>
      </div>

      <div style={{ height: `${window_().start * rowH()}px` }} />
      <For each={rows().slice(window_().start, window_().end)}>
        {(row, i) =>
          row.kind === "file" ? (
            <div
              style={{
                display: "flex",
                "align-items": "center",
                gap: `${props.scale.tightGap}px`,
                height: `${rowH()}px`,
                padding: `0 ${props.scale.panelPadding}px`,
                "font-family": props.fontFamily,
                "font-size": `${props.scale.md}px`,
                // Gitignored hits are real results, just lower-ranked.
                color: row.file.ignored ? props.theme.dimFg : props.theme.fg,
                "white-space": "nowrap",
                overflow: "hidden",
              }}
              title={
                row.file.ignored
                  ? `${row.file.path} (gitignored)`
                  : row.file.path
              }
            >
              <span style={{ overflow: "hidden", "text-overflow": "ellipsis" }}>
                {row.file.path}
              </span>
              <span
                style={{
                  "margin-left": "auto",
                  color: props.theme.dimFg,
                  "font-size": `${props.scale.xs}px`,
                  "flex-shrink": 0,
                }}
              >
                {row.file.matches.length}
              </span>
            </div>
          ) : (
            <div
              draggable={true}
              onDragStart={(e) => {
                // Dragging the row opens the file at its first match.
                const first = row.spans[0];
                const a = assignmentFor(
                  row,
                  first?.jumpLine ?? row.line,
                  first?.jumpCol ?? 0,
                );
                if (a) startTileDrag(e, a);
              }}
              onMouseEnter={() => setSelected(window_().start + i())}
              onClick={() => {
                // Clicking the row's whitespace targets the first match on
                // it; clicking a highlight targets that one (see
                // renderLine, which stops propagation).
                const first = row.spans[0];
                openAt(row, first?.jumpLine ?? row.line, first?.jumpCol ?? 0);
              }}
              style={{
                display: "flex",
                // Centred, not baseline: at line-height 1 a baseline in a
                // 1.35x row pins the text to the top of it.
                "align-items": "center",
                gap: `${props.scale.tightGap}px`,
                height: `${rowH()}px`,
                padding: `0 ${props.scale.panelPadding}px 0 ${props.scale.panelPadding + 6}px`,
                "font-family": props.fontFamily,
                "font-size": `${props.scale.md}px`,
                cursor: props.onOpenTile ? "pointer" : "default",
                background:
                  window_().start + i() === selected()
                    ? props.theme.selectedBg
                    : undefined,
                "white-space": "nowrap",
                overflow: "hidden",
              }}
              title={`${row.file.path}:${row.line + 1}`}
            >
              <span
                style={{
                  color: props.theme.dimFg,
                  "font-variant-numeric": "tabular-nums",
                  "flex-shrink": 0,
                }}
              >
                {row.line + 1}
              </span>
              <span
                style={{
                  color: props.theme.fg,
                  overflow: "hidden",
                  "text-overflow": "ellipsis",
                  "white-space": "pre",
                }}
              >
                {renderLine(row)}
              </span>
            </div>
          )
        }
      </For>
      <div
        style={{
          height: `${(window_().total - window_().end) * rowH()}px`,
        }}
      />
    </div>
  );
}
