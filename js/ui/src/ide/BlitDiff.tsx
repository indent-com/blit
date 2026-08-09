/**
 * PR-6 — BlitDiff (docs/ide-plan.md): a read-only git diff tile.
 *
 * Renders `GitRepoHandle.patch()` aligned rows side-by-side with intraline
 * change spans — no diff parser. v1 shows INDEX×WORKTREE (unstaged) for one
 * file, discovering the repo from the file's absolute path (self-contained,
 * so it survives layout churn) and relativizing against the returned
 * `workdir`.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
  untrack,
  type JSX,
} from "solid-js";
import type {
  BlitWorkspace,
  ConnectionId,
  GitRepoHandle,
  GitPatchRecord,
  TerminalPalette,
} from "@blit-sh/core";
import {
  measureCell,
  GIT_ENDPOINT_INDEX,
  GIT_ENDPOINT_WORKTREE,
  GIT_ENDPOINT_COMMIT,
  GIT_ENDPOINT_EMPTY,
  GIT_HEAD_UNBORN,
  GIT_OID_NONE,
  GIT_DIFF_UNTRACKED,
} from "@blit-sh/core";
import type { DiffSide } from "@blit-sh/core/bsp";
import { editorAssignment } from "@blit-sh/core/bsp";
import { createBlitWorkspaceState } from "@blit-sh/solid";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import {
  useOwnedHandle,
  isConnReady,
  connGeneration,
  isTransientConnError,
} from "./reactive";
import {
  registerActiveEditor,
  clearActiveEditor,
  setActiveEditorFocused,
  type DiffController,
} from "./activeEditor";
import { acquireRepo } from "./repoRegistry";
import { langForFile } from "./languages";
import { buildDiffHighlighter, lineColors } from "./diff-highlight";
import { renderHunkText } from "./diff-render";
import { lineWrap } from "./editorPrefs";
import { setReveal } from "./reveal";

const dec = new TextDecoder();

function basename(p: string): string {
  const s = p.replace(/\/+$/, "");
  const i = s.lastIndexOf("/");
  return i === -1 ? s : s.slice(i + 1);
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function sameSpans(
  a: Array<[number, number]>,
  b: Array<[number, number]>,
): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++)
    if (a[i][0] !== b[i][0] || a[i][1] !== b[i][1]) return false;
  return true;
}

/** Structural equality of two patch records — the per-row key for keeping
 *  DOM across refetches. */
function sameRecord(x: GitPatchRecord, y: GitPatchRecord): boolean {
  if (x.kind !== y.kind) return false;
  if (x.kind === "row" && y.kind === "row") {
    if (x.oldLine !== y.oldLine || x.newLine !== y.newLine) return false;
    if (!bytesEqual(x.oldText, y.oldText) || !bytesEqual(x.newText, y.newText))
      return false;
    if (
      !sameSpans(x.oldSpans, y.oldSpans) ||
      !sameSpans(x.newSpans, y.newSpans)
    )
      return false;
  } else if (x.kind === "gap" && y.kind === "gap") {
    if (x.oldLine !== y.oldLine || x.newLine !== y.newLine) return false;
  } else if (x.kind === "file" && y.kind === "file") {
    if (
      x.flags !== y.flags ||
      x.oldPath !== y.oldPath ||
      x.newPath !== y.newPath
    )
      return false;
  } else if (x.kind === "base" && y.kind === "base") {
    if (!bytesEqual(x.oid, y.oid)) return false;
  }
  return true;
}

/** The shared repo notifier fans a worktree change out to *every* diff tile,
 *  so an edit in one pane re-runs `patch()` in all of them; without this
 *  guard each unrelated re-fetch would replace the records array and rebuild
 *  the DOM (a visible flash + scroll jump). An identical result keeps the
 *  previous array outright; a changed one reuses the unchanged records of
 *  the common prefix/suffix, so `<For>` keeps their rows' DOM and only the
 *  edited middle rebuilds. */
function reuseRecords(
  prev: GitPatchRecord[],
  next: GitPatchRecord[],
): GitPatchRecord[] {
  const n = Math.min(prev.length, next.length);
  let head = 0;
  while (head < n && sameRecord(prev[head], next[head])) head++;
  if (head === prev.length && prev.length === next.length) return prev;
  const out = next.slice();
  for (let i = 0; i < head; i++) out[i] = prev[i];
  let tail = 0;
  while (
    tail < n - head &&
    sameRecord(prev[prev.length - 1 - tail], next[next.length - 1 - tail])
  ) {
    out[next.length - 1 - tail] = prev[prev.length - 1 - tail];
    tail++;
  }
  return out;
}

/** One line of the unified (single-column) view. */
type UnifiedEntry =
  | { kind: "gap" }
  | {
      kind: "line";
      sign: " " | "-" | "+";
      oldLine: number;
      newLine: number;
      text: Uint8Array;
      spans: Array<[number, number]>;
    };

export function BlitDiff(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  path: string;
  /** Which side to show; defaults to "unstaged" (INDEX×WORKTREE). "staged" =
   *  HEAD×INDEX; "untracked" = INDEX×WORKTREE with the untracked walk so a new
   *  file shows fully added instead of "No changes". */
  side?: DiffSide;
  theme: Theme;
  palette: TerminalPalette;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  /** Switch this tile to another view of the same file (diffs ⇄ editor). */
  onOpenTile?: (assignment: string) => void;
  /** Read-only dock preview: no status-bar registration. */
  preview?: boolean;
  /** Whether this tile's workspace pane is focused. */
  focused?: boolean;
}) {
  // Syntax highlighting: the language's Lezer parser drives per-line coloring,
  // recolored on theme/palette change.
  const lang = createMemo(() => langForFile(basename(props.path)));
  const highlighter = createMemo(() =>
    buildDiffHighlighter(props.theme, props.palette),
  );
  const cellColors = (bytes: Uint8Array): (string | null)[] =>
    lineColors(dec.decode(bytes), lang(), highlighter());
  // Gate the repo open on the connection actually being connected, so a diff
  // tile restored on reload waits for the transport instead of erroring.
  const wsState = createBlitWorkspaceState(props.workspace);
  // Memo so it only notifies when readiness flips (a plain accessor re-runs on
  // every snapshot, re-opening the repo). Gate on the git capability so a tile
  // on a session-less root connection (stuck "authenticating") still opens.
  const connConnected = createMemo(() =>
    isConnReady(wsState(), props.connectionId, "supportsGit"),
  );
  // Re-acquire the repo after a connection reset (the old handle is dead).
  const connGen = createMemo(() =>
    connGeneration(wsState(), props.connectionId),
  );
  const repo = useOwnedHandle<GitRepoHandle>(
    () => {
      connGen();
      return connConnected()
        ? acquireRepo(props.workspace, props.connectionId, props.path)
        : null;
    },
    (h) => h.close(),
  );

  const [records, setRecords] = createSignal<GitPatchRecord[]>([]);
  const [diffError, setDiffError] = createSignal<string | null>(null);
  // Default to the unified (single-column) view; toggle to side-by-side.
  const [viewMode, setViewMode] = createSignal<"unified" | "split">("unified");

  // The diff's chrome (filename, view switcher, unified/split toggle) lives
  // in the StatusBar, like the editor's. BSP keeps background tiles mounted,
  // so its pane's focus state owns registration.
  const controller: DiffController = {
    kind: "diff",
    connectionId: props.connectionId,
    path: props.path,
    side: props.side ?? "unstaged",
    sideLabel:
      props.side === "staged"
        ? "HEAD × INDEX"
        : props.side === "worktree"
          ? "HEAD × WORKTREE"
          : props.side === "untracked"
            ? "new file"
            : "INDEX × WORKTREE",
    viewMode,
    toggleViewMode: () =>
      setViewMode((m) => (m === "unified" ? "split" : "unified")),
    onOpenTile: props.onOpenTile,
  };
  createEffect(() => {
    setActiveEditorFocused(
      controller,
      !props.preview && props.focused !== false,
    );
  });
  onCleanup(() => clearActiveEditor(controller));

  // Flatten the aligned records into unified lines: a modified row becomes a
  // "-" (old) line followed by a "+" (new) line; pure add/delete/context map
  // one-to-one. Entries are cached per record object — reuseRecords keeps
  // unchanged records' identities, so their unified entries (and rows' DOM)
  // survive a refetch too.
  const unifiedCache = new WeakMap<GitPatchRecord, UnifiedEntry[]>();
  const unifiedFor = (r: GitPatchRecord): UnifiedEntry[] => {
    if (r.kind === "gap") return [{ kind: "gap" }];
    if (r.kind !== "row") return [];
    if (r.oldLine === 0) {
      return [
        {
          kind: "line",
          sign: "+",
          oldLine: 0,
          newLine: r.newLine,
          text: r.newText,
          spans: r.newSpans,
        },
      ];
    }
    if (r.newLine === 0) {
      return [
        {
          kind: "line",
          sign: "-",
          oldLine: r.oldLine,
          newLine: 0,
          text: r.oldText,
          spans: r.oldSpans,
        },
      ];
    }
    if (
      r.oldSpans.length > 0 ||
      r.newSpans.length > 0 ||
      !bytesEqual(r.oldText, r.newText)
    ) {
      return [
        {
          kind: "line",
          sign: "-",
          oldLine: r.oldLine,
          newLine: 0,
          text: r.oldText,
          spans: r.oldSpans,
        },
        {
          kind: "line",
          sign: "+",
          oldLine: 0,
          newLine: r.newLine,
          text: r.newText,
          spans: r.newSpans,
        },
      ];
    }
    return [
      {
        kind: "line",
        sign: " ",
        oldLine: r.oldLine,
        newLine: r.newLine,
        text: r.newText,
        spans: [],
      },
    ];
  };
  const unifiedRows = createMemo<UnifiedEntry[]>(() => {
    const out: UnifiedEntry[] = [];
    for (const r of records()) {
      if (r.kind === "file" || r.kind === "base") continue;
      let entries = unifiedCache.get(r);
      if (!entries) {
        entries = unifiedFor(r);
        unifiedCache.set(r, entries);
      }
      out.push(...entries);
    }
    return out;
  });

  const rel = () => {
    const h = repo.handle();
    const wd = h?.workdir ?? "";
    if (wd && props.path.startsWith(wd)) {
      return props.path.slice(wd.length).replace(/^\/+/, "");
    }
    return props.path;
  };

  // Click a diff line to open the file in the editor at that line. The line's
  // text relocates it in the live buffer (numbers drift across the diff), with
  // the line number as a nearest-match tie-breaker — same intent as the commit
  // view. Prefer the new-side line; fall back to the old side for deletions.
  const jumpTo = (line: number, text: Uint8Array) => {
    if (!props.onOpenTile) return;
    setReveal(props.connectionId, props.path, {
      text: dec.decode(text).replace(/[\r\n]+$/, ""),
      line: line || 1,
    });
    props.onOpenTile(editorAssignment(props.connectionId, props.path));
  };
  // Click-to-open lives only on the line-number gutter so the diff text stays
  // selectable without navigating away.
  const numCursor = props.onOpenTile ? "pointer" : "default";
  const numTitle = props.onOpenTile ? "Open in editor at this line" : "";
  const numJump = (line: number, text: Uint8Array) =>
    props.onOpenTile ? () => jumpTo(line, text) : undefined;

  const runPatch = (h: GitRepoHandle) => {
    // Endpoints by side:
    //  - staged:    HEAD × INDEX     (git diff --cached)
    //  - worktree:  HEAD × WORKTREE  (all changes since HEAD)
    //  - unstaged:  INDEX × WORKTREE
    //  - untracked: INDEX × WORKTREE + the untracked walk — a new file isn't in
    //    the index, so without GIT_DIFF_UNTRACKED the diff is empty; the flag
    //    pulls in its worktree content so it shows fully added.
    const head = h.state.head;
    const born = head != null && (head.flags & GIT_HEAD_UNBORN) === 0;
    const headEp = born
      ? { kind: GIT_ENDPOINT_COMMIT, oid: head!.oid }
      : { kind: GIT_ENDPOINT_EMPTY, oid: GIT_OID_NONE };
    let oldEp;
    let newEp;
    let flags = 0;
    if (props.side === "staged") {
      oldEp = headEp;
      newEp = { kind: GIT_ENDPOINT_INDEX, oid: GIT_OID_NONE };
    } else if (props.side === "worktree") {
      oldEp = headEp;
      newEp = { kind: GIT_ENDPOINT_WORKTREE, oid: GIT_OID_NONE };
      flags |= GIT_DIFF_UNTRACKED;
    } else {
      oldEp = { kind: GIT_ENDPOINT_INDEX, oid: GIT_OID_NONE };
      newEp = { kind: GIT_ENDPOINT_WORKTREE, oid: GIT_OID_NONE };
      if (props.side === "untracked") flags |= GIT_DIFF_UNTRACKED;
    }
    h.patch(oldEp, newEp, { path: rel(), flags })
      .then((res) => {
        setDiffError(null);
        // Only swap in new records when the diff actually changed, so an
        // unrelated worktree change elsewhere doesn't rebuild this pane.
        setRecords((prev) => reuseRecords(prev, res.records));
      })
      .catch((e: unknown) => {
        // A reset mid-request re-acquires the repo and refetches; showing
        // the transport's own message would just flash a dead end.
        if (isTransientConnError(e)) return;
        setDiffError(e instanceof Error ? e.message : String(e));
      });
  };

  // Refetch pacing: the shared repo notifier fans every worktree settle out
  // to every diff tile, so re-running patch() is debounced (trailing) and
  // deferred while the tile is off-screen or the document hidden — one
  // refetch runs on becoming visible instead.
  const REFETCH_DEBOUNCE_MS = 200;
  let refetchTimer: ReturnType<typeof setTimeout> | null = null;
  let refetchPending = false;
  let fetchedHandle: GitRepoHandle | null = null;
  let rootEl!: HTMLDivElement;
  const [tileVisible, setTileVisible] = createSignal(true);
  const [docHidden, setDocHidden] = createSignal(document.hidden);
  onMount(() => {
    const io = new IntersectionObserver((entries) => {
      for (const entry of entries) setTileVisible(entry.isIntersecting);
    });
    io.observe(rootEl);
    const onVisibility = () => setDocHidden(document.hidden);
    document.addEventListener("visibilitychange", onVisibility);
    onCleanup(() => {
      io.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
    });
  });
  onCleanup(() => {
    if (refetchTimer) clearTimeout(refetchTimer);
  });
  const canFetch = () => tileVisible() && !docHidden();

  createEffect(() => {
    repo.version(); // re-run when the worktree / index / HEAD changes
    const h = repo.handle();
    if (refetchTimer) {
      clearTimeout(refetchTimer);
      refetchTimer = null;
    }
    if (!h) {
      fetchedHandle = null;
      refetchPending = false;
      setRecords([]);
      return;
    }
    if (h !== fetchedHandle) {
      // A freshly (re)acquired handle fetches immediately — first paint
      // must not wait on visibility (embedded panes can be permanently
      // hidden documents); only refetches defer while unseen.
      fetchedHandle = h;
      refetchPending = false;
      runPatch(h);
      return;
    }
    if (!untrack(canFetch)) {
      refetchPending = true;
      return;
    }
    refetchPending = false;
    refetchTimer = setTimeout(() => {
      refetchTimer = null;
      if (canFetch()) runPatch(h);
      else refetchPending = true;
    }, REFETCH_DEBOUNCE_MS);
  });

  // A refetch deferred while unseen runs once the tile is visible again.
  createEffect(() => {
    if (!canFetch() || !refetchPending) return;
    const h = untrack(() => repo.handle());
    if (!h) return;
    refetchPending = false;
    runPatch(h);
  });

  // Row tints and the change-span tints on top of them are OPAQUE, mixed
  // against what they sit on rather than left translucent. An inline
  // background covers the font's content box, which is taller than a
  // line-height-1 row, so vertically adjacent spans overlap — with
  // translucent colours that overlap paints twice and draws a dark band
  // across every row boundary. Composited to the same values up front, the
  // overlap is invisible and one edit reads as one block.
  const over = (color: string, pct: number, base: string) =>
    `color-mix(in srgb, ${color} ${pct}%, ${base})`;
  const addBg = () => over(props.theme.success, 14, props.theme.bg);
  const delBg = () => over(props.theme.error, 14, props.theme.bg);
  const addSpan = () => over(props.theme.success, 32, addBg());
  const delSpan = () => over(props.theme.error, 32, delBg());

  // A diff row is exactly one terminal cell tall, measured the way the
  // terminal measures it: ascent + descent, snapped to device pixels
  // (js/core/src/measure.ts). Rounding the font size instead made the row
  // shorter than the glyphs it holds, and everything followed from that —
  // spacing that did not match a terminal beside it, change-span
  // backgrounds (which cover the font's content box, not the line box)
  // overlapping their neighbours, and the next row's background painting
  // over the tail of a descender, so the bottom of a `g` went missing.
  const cell = createMemo(() => measureCell(props.fontFamily, props.fontSize));
  const rowH = () => `${cell().h}px`;
  const numCol: JSX.CSSProperties = {
    width: "38px",
    "flex-shrink": 0,
    "text-align": "right",
    padding: "0 6px",
    color: props.theme.dimFg,
    "font-variant-numeric": "tabular-nums",
    "user-select": "none",
  };
  // Follows the editor's shared soft-wrap preference (./editorPrefs), so
  // one toggle covers every code pane. Wrapped rows grow past one line —
  // hence min-height rather than height on the rows, and `break-word` so
  // an unbroken minified line still wraps instead of forcing a scrollbar.
  const textCol = (): JSX.CSSProperties => ({
    flex: 1,
    "min-width": 0,
    padding: "0 6px",
    "white-space": lineWrap() ? "pre-wrap" : "pre",
    "overflow-wrap": lineWrap() ? "break-word" : "normal",
    overflow: lineWrap() ? "visible" : "hidden",
  });

  return (
    <div
      ref={rootEl}
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        "flex-direction": "column",
        background: props.theme.bg,
        color: props.theme.fg,
        "font-family": props.fontFamily,
        // Code renders at the configured font size, same as the editor and
        // the terminal — a diff is the same text, not chrome.
        "font-size": `${props.fontSize}px`,
        outline: "none",
      }}
      tabindex={-1}
      onPointerDown={() => {
        if (!props.preview) registerActiveEditor(controller);
      }}
      onFocusIn={() => {
        if (!props.preview) registerActiveEditor(controller);
      }}
    >
      <div
        style={{
          flex: "1 1 0",
          "min-height": 0,
          overflow: "auto",
          ...scrollbarStyle(props.theme),
        }}
      >
        <Show
          when={!repo.error() && !diffError()}
          fallback={
            <div
              style={{
                padding: `${props.scale.panelPadding}px`,
                color: props.theme.errorText,
              }}
            >
              {repo.error() ?? diffError()}
            </div>
          }
        >
          <Show
            when={records().length > 0}
            fallback={
              <div
                style={{
                  padding: `${props.scale.panelPadding}px`,
                  color: props.theme.dimFg,
                }}
              >
                {repo.handle() ? "No changes." : "Opening…"}
              </div>
            }
          >
            <Show
              when={viewMode() === "split"}
              fallback={
                <For each={unifiedRows()}>
                  {(e) => {
                    if (e.kind === "gap") {
                      return (
                        <div
                          style={{
                            padding: `0 ${props.scale.panelPadding}px`,
                            color: props.theme.dimFg,
                            background: props.theme.hoverBg,
                            "font-size": `${props.scale.xs}px`,
                          }}
                        >
                          {"⋯"}
                        </div>
                      );
                    }
                    const bg =
                      e.sign === "+"
                        ? addBg()
                        : e.sign === "-"
                          ? delBg()
                          : "transparent";
                    const spanBg =
                      e.sign === "+"
                        ? addSpan()
                        : e.sign === "-"
                          ? delSpan()
                          : "transparent";
                    const colors = cellColors(e.text);
                    return (
                      <div
                        style={{
                          display: "flex",
                          "min-height": rowH(),
                          "line-height": rowH(),
                          background: bg,
                        }}
                      >
                        <div
                          style={{ ...numCol, cursor: numCursor }}
                          onClick={numJump(e.newLine || e.oldLine, e.text)}
                          title={numTitle}
                        >
                          {e.oldLine || ""}
                        </div>
                        <div
                          style={{ ...numCol, cursor: numCursor }}
                          onClick={numJump(e.newLine || e.oldLine, e.text)}
                          title={numTitle}
                        >
                          {e.newLine || ""}
                        </div>
                        <div
                          style={{
                            width: "14px",
                            "flex-shrink": 0,
                            "text-align": "center",
                            "user-select": "none",
                            color:
                              e.sign === "+"
                                ? props.theme.success
                                : e.sign === "-"
                                  ? props.theme.error
                                  : props.theme.dimFg,
                          }}
                        >
                          {e.sign.trim()}
                        </div>
                        <div style={{ ...textCol(), "padding-left": "2px" }}>
                          {renderHunkText(e.text, e.spans, spanBg, colors)}
                        </div>
                      </div>
                    );
                  }}
                </For>
              }
            >
              <For each={records()}>
                {(r) => {
                  if (r.kind === "file") return null;
                  if (r.kind === "base") return null;
                  // A truncated patch ends with a cursor naming where it
                  // stopped; it is a resume point, not a row to draw.
                  if (r.kind === "cursor") return null;
                  if (r.kind === "gap") {
                    return (
                      <div
                        style={{
                          padding: `0 ${props.scale.panelPadding}px`,
                          color: props.theme.dimFg,
                          background: props.theme.hoverBg,
                          "font-size": `${props.scale.xs}px`,
                        }}
                      >
                        {"⋯"}
                      </div>
                    );
                  }
                  const added = r.oldLine === 0;
                  const deleted = r.newLine === 0;
                  const oldColors = cellColors(r.oldText);
                  // An unchanged context row shows identical text on both
                  // sides — parse it once, not twice.
                  const newColors =
                    r.oldSpans.length === 0 &&
                    r.newSpans.length === 0 &&
                    bytesEqual(r.oldText, r.newText)
                      ? oldColors
                      : cellColors(r.newText);
                  const jumpText = r.newLine ? r.newText : r.oldText;
                  const jumpLine = r.newLine || r.oldLine;
                  return (
                    <div
                      style={{
                        display: "flex",
                        "min-height": rowH(),
                        "line-height": rowH(),
                      }}
                    >
                      <div
                        style={{
                          ...numCol,
                          cursor: numCursor,
                          background: deleted ? delBg() : "transparent",
                        }}
                        onClick={numJump(jumpLine, jumpText)}
                        title={numTitle}
                      >
                        {r.oldLine || ""}
                      </div>
                      <div
                        style={{
                          ...textCol(),
                          background: deleted ? delBg() : "transparent",
                          "border-right": `1px solid ${props.theme.subtleBorder}`,
                        }}
                      >
                        {renderHunkText(
                          r.oldText,
                          r.oldSpans,
                          delSpan(),
                          oldColors,
                        )}
                      </div>
                      <div
                        style={{
                          ...numCol,
                          cursor: numCursor,
                          background: added ? addBg() : "transparent",
                        }}
                        onClick={numJump(jumpLine, jumpText)}
                        title={numTitle}
                      >
                        {r.newLine || ""}
                      </div>
                      <div
                        style={{
                          ...textCol(),
                          background: added ? addBg() : "transparent",
                        }}
                      >
                        {renderHunkText(
                          r.newText,
                          r.newSpans,
                          addSpan(),
                          newColors,
                        )}
                      </div>
                    </div>
                  );
                }}
              </For>
            </Show>
          </Show>
        </Show>
      </div>
    </div>
  );
}
