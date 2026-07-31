/**
 * BlitCommit — a read-only commit tile: full message + author, then the
 * commit's patch (parent×commit) across all files. Clicking a diff line opens
 * that file in the live editor and asks it to relocate the line (by text, via
 * the reveal intent) — a best-effort "jump from this historical line to where
 * it is now", since the LSP/editor only ever see the live working tree.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
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
  gitOidFromHex,
  gitOidHex,
  measureCell,
  GIT_ENDPOINT_COMMIT,
  GIT_ENDPOINT_EMPTY,
  GIT_LOG_FULL_MESSAGE,
  GIT_OID_NONE,
  gitCommitRecords,
} from "@blit-sh/core";
import { commitAssignment, editorAssignment } from "@blit-sh/core/bsp";
import { createBlitWorkspaceState } from "@blit-sh/solid";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import {
  useOwnedHandle,
  isConnReady,
  connGeneration,
  isTransientConnError,
} from "./reactive";
import { collectRefPills, RefPills } from "./refPills";
import { acquireRepo } from "./repoRegistry";
import {
  commitCacheKey,
  getCachedCommit,
  putCachedCommit,
  type CommitInfo,
  type FileDiff,
} from "./commitCache";
import { langForFile } from "./languages";
import { CommitMarkdown } from "./CommitMarkdown";
import { buildDiffHighlighter, lineColors } from "./diff-highlight";
import { renderHunkText } from "./diff-render";
import { lineWrap } from "./editorPrefs";
import { setReveal } from "./reveal";
import {
  registerActiveEditor,
  clearActiveEditor,
  type CommitController,
} from "./activeEditor";

const dec = new TextDecoder();

export function BlitCommit(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  oid: string;
  repoPath: string;
  theme: Theme;
  palette: TerminalPalette;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onOpenTile: (assignment: string) => void;
  /** Read-only preview (the background dock): no status-bar registration,
   *  so a thumbnail never takes the bar from the focused tile. */
  preview?: boolean;
}) {
  // Syntax highlighting for the patch, matched to the editor/diff scheme.
  const highlighter = createMemo(() =>
    buildDiffHighlighter(props.theme, props.palette),
  );
  // Gate the repo open on the connection being connected, so a commit tile
  // restored on reload waits for the transport instead of erroring.
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
        ? acquireRepo(props.workspace, props.connectionId, props.repoPath)
        : null;
    },
    (h) => h.close(),
  );

  // Refs pointing at this commit, live from the shared repo handle's
  // pushed GIT_STATE (the registry opens with watch, so branch moves and
  // new tags update the pills without a refetch).
  const [refVer, setRefVer] = createSignal(0);
  createEffect(() => {
    const h = repo.handle();
    if (!h) return;
    const unsub = h.subscribe(() => setRefVer((v) => v + 1));
    onCleanup(unsub);
  });
  const refPills = createMemo(() => {
    refVer();
    const h = repo.handle();
    if (!h) return [];
    return collectRefPills(h.state, h.oidFormat).get(props.oid) ?? [];
  });

  // Default to the unified (single-column) view; toggle to side-by-side,
  // exactly as a diff tile does — a commit is a patch across files and
  // wants the same choice.
  const [viewMode, setViewMode] = createSignal<"unified" | "split">("unified");

  const [commit, setCommit] = createSignal<CommitInfo | null>(null);
  // The tile's chrome (oid, subject, unified/split toggle) lives in the
  // StatusBar, like the diff's. A commit view has no focusable input, so
  // it registers on mount and on click rather than on focus.
  const controller: CommitController = {
    kind: "commit",
    connectionId: props.connectionId,
    repoPath: props.repoPath,
    get short() {
      return commit()?.short ?? props.oid.slice(0, 8);
    },
    get subject() {
      return subject();
    },
    viewMode,
    toggleViewMode: () =>
      setViewMode((m) => (m === "unified" ? "split" : "unified")),
  };
  if (!props.preview) {
    onMount(() => registerActiveEditor(controller));
    onCleanup(() => clearActiveEditor(controller));
  }

  // `null` = the patch has not arrived yet — distinct from an empty
  // commit, so the viewer shows "Loading…" instead of "No file changes."
  // while the fetch is in flight.
  const [files, setFiles] = createSignal<FileDiff[] | null>(null);
  const [error, setError] = createSignal<string | null>(null);

  // A file's rows beyond this start collapsed and render only on demand, so
  // one huge generated file doesn't stall the whole commit view.
  const COLLAPSE_ROWS = 400;
  const [expandedFiles, setExpandedFiles] = createSignal<Set<number>>(
    new Set(),
  );
  const toggleFile = (idx: number) =>
    setExpandedFiles((cur) => {
      const next = new Set(cur);
      if (next.has(idx)) next.delete(idx);
      else next.add(idx);
      return next;
    });

  createEffect(() => {
    const h = repo.handle();
    if (!h) return;
    const oid = gitOidFromHex(props.oid);
    if (!oid) {
      setError("Invalid commit id");
      return;
    }
    // A commit's message and patch are immutable, so a tile that has already
    // loaded them renders from the cache. This is what makes moving a commit
    // to the dock and back free: the view is rebuilt, but nothing is asked of
    // the server a second time.
    const key = commitCacheKey(props.connectionId, props.repoPath, props.oid);
    const cached = getCachedCommit(key);
    if (cached) {
      setCommit(cached.commit);
      setFiles(cached.files);
      setError(null);
      return;
    }
    setFiles(null); // a re-acquired handle refetches; show loading again
    setExpandedFiles(new Set<number>());
    h.log({ tips: [oid], limit: 1, flags: GIT_LOG_FULL_MESSAGE })
      .then(async (page) => {
        const rec = [...gitCommitRecords(page.records)].find(
          (r) => r.kind === "commit",
        );
        if (!rec || rec.kind !== "commit") {
          setError("Commit not found");
          return;
        }
        const info: CommitInfo = {
          short: props.oid.slice(0, 10),
          message: rec.message,
          author: rec.authorName,
          email: rec.authorEmail,
          time: rec.authorTime,
          committer: rec.committerName,
          committerEmail: rec.committerEmail,
          committerTime: rec.committerTime,
          parents: rec.parents.map((p) => gitOidHex(p, h.oidFormat)),
        };
        setCommit(info);
        const parent = rec.parents[0];
        const oldEp = parent
          ? { kind: GIT_ENDPOINT_COMMIT, oid: parent }
          : { kind: GIT_ENDPOINT_EMPTY, oid: GIT_OID_NONE };
        const res = await h.patch(oldEp, { kind: GIT_ENDPOINT_COMMIT, oid });
        const out: FileDiff[] = [];
        let cur: FileDiff | null = null;
        for (const r of res.records) {
          if (r.kind === "file") {
            cur = { newPath: r.newPath, oldPath: r.oldPath, rows: [] };
            out.push(cur);
          } else if (r.kind !== "base" && cur) {
            cur.rows.push(r);
          }
        }
        setFiles(out);
        putCachedCommit(key, { commit: info, files: out });
      })
      .catch((e: unknown) => {
        // A reset mid-request re-acquires the repo and refetches; showing
        // the transport's own message would just flash a dead end.
        if (isTransientConnError(e)) return;
        setError(e instanceof Error ? e.message : String(e));
      });
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

  // A patch row is exactly one terminal cell tall, measured the way the
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
    width: "40px",
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

  /** Replace this tile with another commit's view (a parent, usually). */
  function openCommit(oidHex: string) {
    props.onOpenTile(
      commitAssignment(props.connectionId, oidHex, props.repoPath),
    );
  }

  // Open a changed file in the live editor, relocating to the clicked line's
  // current position (matched by text; falls back to the line number).
  function openAt(file: FileDiff, r: GitPatchRecord) {
    if (r.kind !== "row") return;
    const abs = `${props.repoPath}/${file.newPath}`;
    const line = r.newLine || r.oldLine;
    const bytes = r.newLine ? r.newText : r.oldText;
    setReveal(props.connectionId, abs, {
      text: dec.decode(bytes).replace(/[\r\n]+$/, ""),
      line: line || 1,
    });
    props.onOpenTile(editorAssignment(props.connectionId, abs));
  }

  const subject = () => commit()?.message.split("\n")[0] ?? "";
  const body = () => {
    const m = commit()?.message ?? "";
    const nl = m.indexOf("\n");
    return nl === -1 ? "" : m.slice(nl + 1).replace(/^\n+/, "");
  };

  return (
    <div
      // Programmatically focusable so pane-cycling onto a commit tile moves
      // DOM focus out of the previously focused terminal.
      tabindex={-1}
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        "flex-direction": "column",
        background: props.theme.bg,
        color: props.theme.fg,
        "font-family": props.fontFamily,
        // Code renders at the configured font size, same as the editor and
        // the terminal — a patch is the same text, not chrome.
        "font-size": `${props.fontSize}px`,
        outline: "none",
      }}
      onPointerDown={() => {
        if (!props.preview) registerActiveEditor(controller);
      }}
      onFocusIn={() => {
        if (!props.preview) registerActiveEditor(controller);
      }}
    >
      <Show
        when={!repo.error() && !error()}
        fallback={
          <div style={{ padding: "10px", color: props.theme.errorText }}>
            {repo.error() ?? error()}
          </div>
        }
      >
        {/* One scroller for the tile: the commit header scrolls away into the
            patch, and the per-file headers stick against this scrollport. */}
        <div
          style={{
            flex: "1 1 0",
            "min-height": 0,
            overflow: "auto",
            ...scrollbarStyle(props.theme),
          }}
        >
          {/* Commit header */}
          <Show when={commit()}>
            {(c) => (
              <div
                style={{
                  // The scroller runs at line-height 1 for the patch rows;
                  // prose needs its own leading back.
                  "line-height": "normal",
                  padding: `${props.scale.panelPadding}px`,
                  "border-bottom": `1px solid ${props.theme.subtleBorder}`,
                  background: props.theme.panelBg,
                }}
              >
                <div
                  style={{
                    "font-size": `${props.scale.md}px`,
                    color: props.theme.fg,
                  }}
                >
                  <CommitMarkdown theme={props.theme} variant="subject">
                    {subject()}
                  </CommitMarkdown>
                </div>
                <div
                  style={{
                    display: "flex",
                    // Wrap *between* the items. Without this the row is one
                    // unwrapped flex line and every item shrinks to its
                    // min-content width, so a narrow pane turns the header
                    // into a rank of one-word-wide columns each wrapping
                    // inside itself. The atoms below then refuse to break at
                    // all — an oid or a timestamp split across two lines is
                    // not a shorter oid, it is an unreadable one.
                    "flex-wrap": "wrap",
                    gap: `${props.scale.tightGap}px`,
                    "align-items": "baseline",
                    "margin-top": "2px",
                    // Metadata reads as secondary through colour, not by
                    // shrinking it — the commit header is prose the user
                    // asked for at the configured size.
                    "font-size": `${props.scale.md}px`,
                    color: props.theme.dimFg,
                  }}
                >
                  <span
                    style={{
                      color: props.theme.warning,
                      "white-space": "nowrap",
                    }}
                  >
                    {c().short}
                  </span>
                  <RefPills
                    pills={refPills()}
                    theme={props.theme}
                    scale={props.scale}
                    max={6}
                    wrap
                  />
                  {/* A name is prose and may wrap; it just may not be
                      squeezed to one word per line, which `min-width: 0`
                      here would reintroduce. */}
                  <span>{c().author}</span>
                  <span style={{ "white-space": "nowrap" }}>
                    {new Date(Number(c().time) * 1000).toLocaleString()}
                  </span>
                  <Show
                    when={
                      c().committer !== c().author ||
                      c().committerEmail !== c().email ||
                      c().committerTime !== c().time
                    }
                  >
                    <span style={{ opacity: 0.75 }}>
                      {"· committed"}
                      {c().committer !== c().author
                        ? ` by ${c().committer}`
                        : ""}{" "}
                      <span style={{ "white-space": "nowrap" }}>
                        {new Date(
                          Number(c().committerTime) * 1000,
                        ).toLocaleString()}
                      </span>
                    </span>
                  </Show>
                </div>
                {/* Parents — the way to walk history backwards from here.
                    A merge has several; a root commit has none, and says
                    so rather than rendering an empty row. */}
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${props.scale.tightGap}px`,
                    "align-items": "baseline",
                    "margin-top": "2px",
                    "font-size": `${props.scale.md}px`,
                    color: props.theme.dimFg,
                  }}
                >
                  <span>
                    {c().parents.length === 0
                      ? "root commit"
                      : c().parents.length === 1
                        ? "parent"
                        : `parents (merge of ${c().parents.length})`}
                  </span>
                  <For each={c().parents}>
                    {(p, i) => (
                      <span
                        role="button"
                        tabindex={0}
                        title={`Open ${p}`}
                        style={{
                          color: props.theme.accent,
                          cursor: "pointer",
                          "text-decoration": "underline",
                          "text-underline-offset": "2px",
                          // `^2 a1b2c3d4e5` is one token: broken across two
                          // lines it reads as two different parents.
                          "white-space": "nowrap",
                        }}
                        onClick={() => openCommit(p)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            openCommit(p);
                          }
                        }}
                      >
                        {/* Numbered on a merge, so "which side" is
                            answerable without reading the oids. */}
                        {c().parents.length > 1 ? `^${i() + 1} ` : ""}
                        {p.slice(0, 10)}
                      </span>
                    )}
                  </For>
                </div>
                <Show when={body()}>
                  <div
                    style={{
                      "font-size": `${props.scale.md}px`,
                      color: props.theme.fg,
                    }}
                  >
                    <CommitMarkdown theme={props.theme} variant="body">
                      {body()}
                    </CommitMarkdown>
                  </div>
                </Show>
              </div>
            )}
          </Show>

          {/* Patch */}
          <For each={files() ?? []}>
            {(file, fileIdx) => {
              const collapsible = file.rows.length > COLLAPSE_ROWS;
              const collapsed = () =>
                collapsible && !expandedFiles().has(fileIdx());
              return (
                <>
                  <div
                    onClick={
                      collapsible ? () => toggleFile(fileIdx()) : undefined
                    }
                    style={{
                      position: "sticky",
                      top: 0,
                      padding: `${props.scale.controlY}px ${props.scale.panelPadding}px`,
                      background: props.theme.panelBg,
                      "border-top": `1px solid ${props.theme.subtleBorder}`,
                      "border-bottom": `1px solid ${props.theme.subtleBorder}`,
                      color: props.theme.fg,
                      "font-weight": 600,
                      "white-space": "nowrap",
                      overflow: "hidden",
                      "text-overflow": "ellipsis",
                      cursor: collapsible ? "pointer" : undefined,
                      "user-select": collapsible ? "none" : undefined,
                    }}
                    title={file.newPath}
                  >
                    <Show when={collapsible}>
                      <span
                        style={{
                          color: props.theme.dimFg,
                          "margin-right": `${props.scale.tightGap}px`,
                        }}
                      >
                        {collapsed() ? "▸" : "▾"}
                      </span>
                    </Show>
                    {file.oldPath && file.oldPath !== file.newPath
                      ? `${file.oldPath} → ${file.newPath}`
                      : file.newPath}
                    <Show when={collapsed()}>
                      <span
                        style={{
                          color: props.theme.dimFg,
                          "font-weight": 400,
                          "font-size": `${props.scale.xs}px`,
                          "margin-left": `${props.scale.tightGap}px`,
                        }}
                      >
                        {file.rows.length} rows
                      </span>
                    </Show>
                  </div>
                  <For each={collapsed() ? [] : file.rows}>
                    {(r) => {
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
                      if (r.kind !== "row") return null;
                      const added = r.oldLine === 0;
                      const deleted = r.newLine === 0;
                      const lang = langForFile(file.newPath || file.oldPath);
                      if (viewMode() === "split") {
                        // Old on the left, new on the right — the aligned
                        // record already pairs them, so nothing has to be
                        // re-matched here.
                        const oldStr = dec.decode(r.oldText);
                        const newStr = dec.decode(r.newText);
                        const oldColors = lineColors(
                          oldStr,
                          lang,
                          highlighter(),
                        );
                        // A context row is the same text twice: parse once.
                        const newColors =
                          newStr === oldStr
                            ? oldColors
                            : lineColors(newStr, lang, highlighter());
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
                                cursor: "pointer",
                                background: deleted ? delBg() : "transparent",
                              }}
                              onClick={() => openAt(file, r)}
                              title="Open in editor at this line"
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
                                cursor: "pointer",
                                background: added ? addBg() : "transparent",
                              }}
                              onClick={() => openAt(file, r)}
                              title="Open in editor at this line"
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
                      }
                      const text = added
                        ? r.newText
                        : deleted
                          ? r.oldText
                          : r.newText.length
                            ? r.newText
                            : r.oldText;
                      const spans = added
                        ? r.newSpans
                        : deleted
                          ? r.oldSpans
                          : [];
                      const changedBg = added
                        ? addSpan()
                        : deleted
                          ? delSpan()
                          : "transparent";
                      const colors = lineColors(
                        dec.decode(text),
                        lang,
                        highlighter(),
                      );
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
                              cursor: "pointer",
                              background: deleted ? delBg() : "transparent",
                            }}
                            onClick={() => openAt(file, r)}
                            title="Open in editor at this line"
                          >
                            {r.oldLine || ""}
                          </div>
                          <div
                            style={{
                              ...numCol,
                              cursor: "pointer",
                              background: added ? addBg() : "transparent",
                              "border-right": `1px solid ${props.theme.subtleBorder}`,
                            }}
                            onClick={() => openAt(file, r)}
                            title="Open in editor at this line"
                          >
                            {r.newLine || ""}
                          </div>
                          <div
                            style={{
                              ...textCol(),
                              background: added
                                ? addBg()
                                : deleted
                                  ? delBg()
                                  : "transparent",
                            }}
                          >
                            {renderHunkText(text, spans, changedBg, colors)}
                          </div>
                        </div>
                      );
                    }}
                  </For>
                </>
              );
            }}
          </For>
          <Show when={files() === null}>
            <div style={{ padding: "10px", color: props.theme.dimFg }}>
              Loading…
            </div>
          </Show>
          <Show when={commit() && files()?.length === 0}>
            <div style={{ padding: "10px", color: props.theme.dimFg }}>
              No file changes.
            </div>
          </Show>
        </div>
      </Show>
    </div>
  );
}
