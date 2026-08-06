/**
 * PR-7/8/9 — BlitEditor (docs/ide-plan.md): a CodeMirror 6 editor tile.
 *
 * Watches a single-file content sync of the file itself
 * (docs/design/fs-watch.md § Single-file sync): one mirror entry keyed `""`,
 * hashed for CAS, no sibling mirroring. An old server refuses the `SINGLE`
 * flag, and the editor falls back to a content sync of the parent directory
 * (see the `syncMode` discriminator). Edits happen in CM6 (palette-themed via
 * {@link cmTheme}, syntax by extension); saves use CAS (`ifHash`), ship
 * deltas against the last-known disk bytes (docs/design/fs-write.md) — and
 * surface conflict (Reload / Overwrite) and read-only host. Self-echo is
 * suppressed via `lastWrittenHash`; an external change (its upsert's changed
 * hash) refetches a clean buffer and banners a dirty one.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  onMount,
  Show,
} from "solid-js";
import type {
  BlitWorkspace,
  ConnectionId,
  FsSyncHandle,
  FsNode,
  FsRecord,
  TerminalPalette,
} from "@blit-sh/core";
import type { LspFileDiags, LspHandle } from "@blit-sh/core";
import {
  FsConflictError,
  FS_ENTRY_NO_CONTENT,
  LSP_COMPLETION_DEPRECATED,
  LSP_COMPLETION_PRESELECT,
  LSP_COMPLETION_SNIPPET,
  LSP_MARKUP_MARKDOWN,
  LSP_SEVERITY_ERROR,
  LSP_SEVERITY_WARNING,
  LSP_STATUS_OK,
  LSP_STATUS_WARMING,
  lspStatusText,
} from "@blit-sh/core";
import type { LspQueryRecord, LspQueryResult } from "@blit-sh/core";
import { createBlitWorkspaceState } from "@blit-sh/solid";
import {
  lintGutter,
  lintKeymap,
  setDiagnostics,
  type Diagnostic,
} from "@codemirror/lint";
import {
  EditorState,
  Compartment,
  StateEffect,
  StateField,
} from "@codemirror/state";
import {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  highlightTrailingWhitespace,
  crosshairCursor,
  rectangularSelection,
  scrollPastEnd,
  drawSelection,
  dropCursor,
  hoverTooltip,
  showTooltip,
  type Tooltip,
} from "@codemirror/view";
import {
  acceptCompletion,
  snippet,
  nextSnippetField,
  prevSnippetField,
  autocompletion,
  closeBrackets,
  closeBracketsKeymap,
  type Completion,
  type CompletionContext,
  type CompletionResult,
} from "@codemirror/autocomplete";
import {
  highlightSelectionMatches,
  search,
  searchKeymap,
} from "@codemirror/search";
import {
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
} from "@codemirror/commands";
import {
  bidiIsolates,
  bracketMatching,
  indentUnit,
  indentOnInput,
  foldGutter,
  foldKeymap,
} from "@codemirror/language";
import { editorAssignment } from "@blit-sh/core/bsp";
import type { Theme } from "../theme";
import { ui } from "../theme";
import { scrollbarStyle } from "../theme";
import { cmTheme, cmPhrases } from "./cm-theme";
import { detectIndent, lspSnippetToCm } from "./snippet";
import { blitSearchPanel } from "./cmSearchPanel";
import { loadLangForFile } from "./languages";
import { lspWirePath } from "./paths";
import { isConnReady, connGeneration } from "./reactive";
import { consumeReveal, setReveal, revealVersion } from "./reveal";
import { symbolKindTag } from "./symbolKinds";
import { lineWrap, toggleLineWrap } from "./editorPrefs";
import {
  rememberEditorPosition,
  recallEditorPosition,
} from "./editorPositions";
import {
  parkBuffer,
  flushParkedBuffer,
  clearParkedBuffer,
  recallParkedBuffer,
} from "./serverState";
import {
  registerActiveEditor,
  clearActiveEditor,
  type EditorController,
  type EditorBanner,
} from "./activeEditor";
import {
  applyRenameToFile,
  resolveEdits,
  type LspEdit,
  type RenameFileOutcome,
} from "./lspRename";

const decoder = new TextDecoder();
const encoder = new TextEncoder();

// Signature-help tooltip plumbing, shared across editors (a StateField
// definition instantiates per EditorState): the effect swaps the tooltip
// in and out, and doc edits re-anchor it through the change map.
const setSigTooltip = StateEffect.define<Tooltip | null>();
const sigTooltipField = StateField.define<Tooltip | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) if (e.is(setSigTooltip)) value = e.value;
    if (value && tr.docChanged)
      value = { ...value, pos: tr.changes.mapPos(value.pos) };
    return value;
  },
  provide: (f) => showTooltip.from(f),
});

// LSP CompletionItemKind → the CM completion icon vocabulary.
const CM_COMPLETION_TYPES: Record<number, string> = {
  1: "text",
  2: "method",
  3: "function",
  4: "function",
  5: "property",
  6: "variable",
  7: "class",
  8: "interface",
  9: "namespace",
  10: "property",
  11: "constant",
  12: "constant",
  13: "enum",
  14: "keyword",
  15: "text",
  16: "constant",
  17: "text",
  18: "text",
  19: "namespace",
  20: "constant",
  21: "constant",
  22: "class",
  23: "interface",
  24: "keyword",
  25: "type",
};

/** Characters an identifier is made of, for the rename prefill. */
const WORD_CHAR = /[\p{L}\p{N}_$]/u;

/**
 * Render an LSP `MARKUP` record into a hover tooltip body.
 *
 * Language servers emit a narrow slice of markdown — a fenced signature,
 * `---`, then prose with inline code — so this handles exactly that rather
 * than pulling in a markdown renderer. Everything is built as text nodes,
 * never innerHTML: hover content is server data.
 */
function renderMarkup(text: string, markdown: boolean): HTMLElement {
  const dom = document.createElement("div");
  dom.className = "blit-hover";
  if (!markdown) {
    const pre = document.createElement("pre");
    pre.textContent = text;
    dom.append(pre);
    return dom;
  }
  // Odd indices are the insides of ``` fences; even indices are prose.
  text.split("```").forEach((part, i) => {
    if (i % 2 === 1) {
      const pre = document.createElement("pre");
      // Drop the info string ("rust", "ts") that opens a fence.
      pre.textContent = part.replace(/^[^\n]*\n/, "").replace(/\n+$/, "");
      if (pre.textContent) dom.append(pre);
      return;
    }
    for (const line of part.split("\n")) {
      if (/^\s*-{3,}\s*$/.test(line)) {
        dom.append(document.createElement("hr"));
        continue;
      }
      if (!line.trim()) continue;
      const p = document.createElement("p");
      // Odd indices are inline `code` spans; bold/italic markers just go.
      line.split("`").forEach((chunk, j) => {
        if (j % 2 === 1) {
          const c = document.createElement("code");
          c.textContent = chunk;
          p.append(c);
        } else {
          p.append(chunk.replace(/\*\*|__|(?<!\w)[*_](?!\s)/g, ""));
        }
      });
      dom.append(p);
    }
  });
  if (!dom.childNodes.length) {
    const pre = document.createElement("pre");
    pre.textContent = text;
    dom.append(pre);
  }
  return dom;
}

function basename(p: string): string {
  const s = p.replace(/\/+$/, "");
  const i = s.lastIndexOf("/");
  return i === -1 ? s : s.slice(i + 1);
}
function dirname(p: string): string {
  const s = p.replace(/\/+$/, "");
  const i = s.lastIndexOf("/");
  return i <= 0 ? "/" : s.slice(0, i);
}

export function BlitEditor(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  path: string;
  theme: Theme;
  palette: TerminalPalette;
  fontFamily: string;
  fontSize: number;
  /** Switch this tile to another view of the same file (editor ⇄ diffs). */
  onOpenTile?: (assignment: string) => void;
  /** Read-only preview (the background dock): no editing, no LSP, no buffer
   *  parking, no status-bar registration — an always-on view, terminal-style. */
  preview?: boolean;
}) {
  let host!: HTMLDivElement;
  let view: EditorView | null = null;
  let applying = false; // programmatic doc updates must not mark dirty
  let lastHash = 0n; // server content hash (CAS base + self-echo compare)
  // The on-disk bytes `lastHash` names — the delta base for the next save
  // (docs/design/fs-write.md `content_kind` 2). Kept in lockstep with
  // lastHash so the delta always applies against the exact bytes on disk.
  let lastDiskBytes: Uint8Array | null = null;

  const fileName = basename(props.path);
  const parentDir = dirname(props.path);
  // How this file is mirrored. "single": a sync of the file itself
  // (docs/design/fs-watch.md § Single-file sync), whose one entry is keyed
  // `""`. "dir": the fallback for servers that predate the SINGLE flag — a
  // content sync of the parent directory, keyed by file name. Starts
  // optimistic; an old server's open rejection flips it (see the sync
  // effect), re-running every opener with the other source.
  const [syncMode, setSyncMode] = createSignal<"single" | "dir">("single");
  // This file's mirror key under the current mode — the path used for
  // live.get / fetch / writeFile / record matching.
  const fileKey = () => (syncMode() === "single" ? "" : fileName);
  const themeComp = new Compartment();
  // Language grammar loads on demand (see loadLangForFile); until it resolves
  // the compartment holds nothing, so the editor opens immediately unstyled and
  // gains highlighting a tick later.
  const langComp = new Compartment();
  // Soft wrap is a workspace-wide preference (see ./editorPrefs), so it
  // lives in a compartment and every open editor reconfigures together.
  const wrapComp = new Compartment();
  // Indentation is detected from the document (see ./snippet detectIndent),
  // so it can only be set once content has arrived — hence a compartment
  // rather than a static extension.
  const indentComp = new Compartment();

  // Reactive: is this tile's connection actually connected? A tile restored
  // from the hash on reload can mount before the transport finishes
  // connecting, in which case syncFs/openLsp throw. Gate opens on this and
  // re-run once it flips to connected.
  const wsState = createBlitWorkspaceState(props.workspace);
  // Memo so it only notifies when readiness actually flips — a plain accessor
  // would re-run on *every* workspace snapshot (which changes constantly),
  // re-opening the sync repeatedly. Gate on the FS capability so a tile on a
  // session-less root connection (stuck "authenticating") still loads.
  const connConnected = createMemo(() =>
    isConnReady(wsState(), props.connectionId, "supportsFsSync"),
  );
  // Bumps on every connection reset — read in the sync effect so a server
  // re-establish (which resets syncs while the transport stays up) re-opens.
  const connGen = createMemo(() =>
    connGeneration(wsState(), props.connectionId),
  );
  // Retry after a transient close/reject: a re-establish resets fs syncs right
  // AFTER re-emitting the snapshot, so a sync opened during that emit is
  // immediately closed — and connGen already bumped, so it won't re-run on its
  // own. Bumping this re-runs the effect a microtask later, after the reset.
  // Bounded (reset on a successful load) so a reset storm can't spin.
  const [fsRetry, setFsRetry] = createSignal(0);
  let fsRetries = 0;
  const retryFs = () => {
    if (fsRetries < 20) {
      fsRetries++;
      setFsRetry((n) => n + 1);
    }
  };
  // Same retry discipline for the LSP attachment — without it, a re-establish
  // leaves the editor with a dead (or null) LspHandle, so go-to-definition /
  // find-references / squiggles all silently stop working after a refresh.
  const [lspRetry, setLspRetry] = createSignal(0);
  let lspRetries = 0;
  const retryLsp = () => {
    if (lspRetries < 20) {
      lspRetries++;
      setLspRetry((n) => n + 1);
    }
  };
  const isTransientConn = (e: unknown): boolean =>
    /re-established|shutting down|transport is|not connected/i.test(
      e instanceof Error ? e.message : String(e),
    );

  const [handle, setHandle] = createSignal<FsSyncHandle | null>(null);
  const [status, setStatus] = createSignal<
    "loading" | "ready" | "readonly" | "error"
  >("loading");
  const [error, setError] = createSignal<string | null>(null);
  const [dirty, setDirty] = createSignal(false);
  const [conflict, setConflict] = createSignal(false);
  const [externalChanged, setExternalChanged] = createSignal(false);
  // The file went away underneath us (rm, a branch switch, a rename). The
  // buffer stays open and editable; Save recreates the file. Cleared when
  // it comes back.
  const [gone, setGone] = createSignal<"deleted" | "renamed" | null>(null);

  const [lspHandle, setLspHandle] = createSignal<LspHandle | null>(null);
  const [lspVersion, setLspVersion] = createSignal(0);

  const lspRel = () => lspWirePath(lspHandle()?.root ?? null, props.path);

  // ── LSP navigation: go-to-definition (F12 / ⌘-click), find-references
  //    (⇧F12). Positions are 0-based line + UTF-8 byte column, like diagnostics.
  type Loc = { path: string; line: number; col: number };
  const [refs, setRefs] = createSignal<Loc[] | null>(null);
  const [lspMsg, setLspMsg] = createSignal<string | null>(null);
  let lspMsgTimer: ReturnType<typeof setTimeout> | undefined;
  // `ms` buys longer for reports the user has to actually read (what a
  // rename touched, what it refused) than for a transient "not found".
  const flashLsp = (m: string, ms = 2500) => {
    setLspMsg(m);
    clearTimeout(lspMsgTimer);
    lspMsgTimer = setTimeout(() => setLspMsg(null), ms);
  };
  onCleanup(() => clearTimeout(lspMsgTimer));

  // Cursor → LSP (line, byteCol).
  const cursorLsp = (): { line: number; col: number } | null => {
    if (!view) return null;
    const pos = view.state.selection.main.head;
    const line = view.state.doc.lineAt(pos);
    const byteCol = encoder.encode(line.text.slice(0, pos - line.from)).length;
    return { line: line.number - 1, col: byteCol };
  };
  // LSP (line, byteCol) → offset in THIS document.
  const lspToOffset = (line0: number, byteCol: number): number => {
    const doc = view!.state.doc;
    const lineNo = Math.min(Math.max(line0, 0) + 1, doc.lines);
    const line = doc.line(lineNo);
    const bytes = encoder.encode(line.text);
    const ch = decoder.decode(
      bytes.subarray(0, Math.min(byteCol, bytes.length)),
    ).length;
    return line.from + Math.min(ch, line.length);
  };
  // Resolve a location's workspace-relative path to absolute.
  const lspAbs = (recPath: string): string => {
    // Location paths are workspace-relative (same keys as diagnostics); an
    // absolute path is already resolved. Strip a trailing slash off the root so
    // the join never yields "root//path" (which would fail to open).
    if (recPath.startsWith("/")) return recPath;
    const root = (lspHandle()?.root ?? "").replace(/\/+$/, "");
    return root ? `${root}/${recPath.replace(/^\/+/, "")}` : recPath;
  };
  // Jump to a location: same file → move the cursor; else open an editor tile.
  const jumpTo = (loc: Loc) => {
    const abs = lspAbs(loc.path);
    if (abs === props.path && view) {
      const off = lspToOffset(loc.line, loc.col);
      view.dispatch({
        selection: { anchor: off },
        effects: EditorView.scrollIntoView(off, { y: "center" }),
      });
      view.focus();
      return;
    }
    if (!props.onOpenTile) {
      flashLsp("Definition is in another file");
      return;
    }
    setReveal(props.connectionId, abs, {
      text: "",
      line: loc.line + 1,
      col: loc.col,
    });
    props.onOpenTile(editorAssignment(props.connectionId, abs));
  };

  const locsOf = (records: { kind: string }[]): Loc[] =>
    records
      .filter((r): r is Loc & { kind: "location" } => r.kind === "location")
      .map((r) => ({ path: r.path, line: r.line, col: r.col }));

  async function goToDefinition() {
    const h = lspHandle();
    const at = cursorLsp();
    if (!h) {
      // Attach failed or never matched (no project marker for this file
      // type, or the server binary is missing on the host) — say so
      // instead of a silent F12.
      flashLsp("No language server attached");
      return;
    }
    if (!at) return;
    try {
      flushLspBuffer();
      const res = await h.definition(lspRel(), at.line, at.col);
      const locs = locsOf(res.records);
      if (locs.length) jumpTo(locs[0]);
      else flashLsp("No definition found");
    } catch {
      flashLsp("Go to definition failed");
    }
  }

  async function findReferences() {
    const h = lspHandle();
    const at = cursorLsp();
    if (!h) {
      flashLsp("No language server attached");
      return;
    }
    if (!at) return;
    try {
      flushLspBuffer();
      const res = await h.references(lspRel(), at.line, at.col, true);
      // Show the panel even when empty so "No references found." lands at the
      // bottom (where results appear), not as a transient header flash.
      setOutline(null);
      setRefs(locsOf(res.records));
    } catch {
      flashLsp("Find references failed");
    }
  }

  // ── Hover: type and docs under the pointer. The backend answers with one
  //    MARKUP record plus an optional LOCATION naming the range the answer
  //    covers, which becomes the tooltip's extent so it holds still while
  //    the pointer moves within the symbol.
  const hoverSource = hoverTooltip(async (v, pos) => {
    const h = lspHandle();
    if (!h) return null;
    const line = v.state.doc.lineAt(pos);
    if (!line.text.trim()) return null; // nothing to ask about
    const byteCol = encoder.encode(line.text.slice(0, pos - line.from)).length;
    flushLspBuffer();
    let res: LspQueryResult;
    try {
      res = await h.hover(lspRel(), line.number - 1, byteCol);
    } catch {
      return null;
    }
    if (res.status !== LSP_STATUS_OK) return null;
    const markup = res.records.find(
      (r): r is Extract<LspQueryRecord, { kind: "markup" }> =>
        r.kind === "markup",
    );
    if (!markup?.text.trim()) return null;
    const loc = res.records.find(
      (r): r is Extract<LspQueryRecord, { kind: "location" }> =>
        r.kind === "location",
    );
    // Anchor on the reported range when there is one; the tooltip then
    // survives pointer movement across the whole symbol.
    const from = loc ? lspToOffset(loc.line, loc.col) : pos;
    const to = loc ? Math.max(from, lspToOffset(loc.endLine, loc.endCol)) : pos;
    return {
      pos: Math.min(from, pos),
      end: Math.max(to, pos),
      above: true,
      create: () => ({
        dom: renderMarkup(markup.text, markup.format === LSP_MARKUP_MARKDOWN),
      }),
    };
  });

  // ── Rename (F2): the backend returns an edit plan and never touches the
  //    filesystem, so applying it is this client's job (see ./lspRename).
  //    This file goes through CodeMirror so the rename lands in the undo
  //    history; every other file is rewritten under CAS.
  const [renaming, setRenaming] = createSignal<{
    at: { line: number; col: number };
    oldName: string;
    top: number;
    left: number;
  } | null>(null);
  const [renameBusy, setRenameBusy] = createSignal(false);
  let renameInput: HTMLInputElement | undefined;

  function startRename() {
    if (!view || props.preview) return;
    if (!lspHandle()) {
      flashLsp("No language server attached");
      return;
    }
    if (status() === "readonly") {
      flashLsp("Read-only host");
      return;
    }
    const at = cursorLsp();
    if (!at) return;
    const pos = view.state.selection.main.head;
    const line = view.state.doc.lineAt(pos);
    // The identifier under the cursor prefills the box, so the common
    // edit is "change a few characters", not "retype the name".
    let s = pos - line.from;
    let e = s;
    while (s > 0 && WORD_CHAR.test(line.text[s - 1])) s--;
    while (e < line.text.length && WORD_CHAR.test(line.text[e])) e++;
    const coords = view.coordsAtPos(line.from + s) ?? view.coordsAtPos(pos);
    const box = host.getBoundingClientRect();
    setRefs(null);
    setOutline(null);
    setRenaming({
      at,
      oldName: line.text.slice(s, e),
      top: coords ? coords.bottom - box.top + 2 : 4,
      left: coords ? Math.max(0, coords.left - box.left) : 4,
    });
  }

  const cancelRename = () => {
    setRenaming(null);
    view?.focus();
  };

  // Apply the plan's edits for THIS file through CM: one transaction, so a
  // single undo reverses the local half of the rename. No `applying` flag —
  // this is a real edit that should mark the buffer dirty and park it.
  function applyEditsHere(list: LspEdit[]): number {
    if (!view) return 0;
    const resolved = resolveEdits(view.state.doc.toString(), list);
    if (!resolved.length) return 0;
    view.dispatch({
      // resolveEdits returns last-first; CM wants them in document order.
      changes: [...resolved]
        .reverse()
        .map((e) => ({ from: e.from, to: e.to, insert: e.insert })),
    });
    return resolved.length;
  }

  async function commitRename(newName: string) {
    const r = renaming();
    const h = lspHandle();
    if (!r || !h || !view) return;
    const name = newName.trim();
    if (!name || name === r.oldName) {
      cancelRename();
      return;
    }
    setRenameBusy(true);
    try {
      flushLspBuffer();
      const res = await h.rename(lspRel(), r.at.line, r.at.col, name);
      if (res.status !== LSP_STATUS_OK) {
        flashLsp(
          `Rename failed: ${res.detail || lspStatusText(res.status)}`,
          5000,
        );
        return;
      }
      const edits = res.records.filter((x): x is LspEdit => x.kind === "edit");
      if (!edits.length) {
        flashLsp("Nothing to rename here");
        return;
      }
      // Group by file; the plan's paths are workspace-relative like every
      // other LSP path.
      const byPath = new Map<string, LspEdit[]>();
      for (const e of edits) {
        const list = byPath.get(e.path);
        if (list) list.push(e);
        else byPath.set(e.path, [e]);
      }
      let here = 0;
      const others: RenameFileOutcome[] = [];
      for (const [p, list] of byPath) {
        if (lspAbs(p) === props.path) {
          // No hash check for this file: the backend answered against our
          // live buffer overlay, not disk, so the plan's hash is the
          // overlay's and comparing it to the disk hash would always fail.
          here = applyEditsHere(list);
        } else {
          others.push(
            await applyRenameToFile(
              props.workspace,
              props.connectionId,
              lspAbs(p),
              p,
              list,
            ),
          );
        }
      }
      if (here) await save();
      const applied = here + others.reduce((n, o) => n + o.edits, 0);
      const touched = (here ? 1 : 0) + others.filter((o) => o.edits).length;
      const failed = others.filter((o) => o.error);
      const where = `${applied} occurrence${applied === 1 ? "" : "s"} in ${touched} file${touched === 1 ? "" : "s"}`;
      if (failed.length) {
        flashLsp(
          `Renamed ${where}; ${failed.length} file${failed.length === 1 ? "" : "s"} refused — ${failed[0].path}: ${failed[0].error}`,
          8000,
        );
      } else if (res.incomplete) {
        // LSP_RESP_INCOMPLETE: the server dropped whole-file create /
        // rename / delete steps it could not project into edit records.
        flashLsp(
          `Renamed ${where}; the plan's file create/rename/delete steps were dropped`,
          8000,
        );
      } else if (res.truncated) {
        flashLsp(`Renamed ${where} — plan truncated, re-run to finish`, 8000);
      } else {
        flashLsp(`Renamed ${where}`, 4000);
      }
    } catch (e) {
      flashLsp(
        `Rename failed: ${e instanceof Error ? e.message : String(e)}`,
        5000,
      );
    } finally {
      setRenameBusy(false);
      setRenaming(null);
      view?.focus();
    }
  }

  // Focus and pre-select the box the moment it appears, so F2 is followed
  // straight by typing.
  createEffect(() => {
    if (!renaming()) return;
    queueMicrotask(() => {
      renameInput?.focus();
      renameInput?.select();
    });
  });

  // ── Outline (⌘⇧O): the document's symbols, filtered as you type.
  type Sym = {
    name: string;
    symKind: number;
    depth: number;
    line: number;
    col: number;
  };
  const [outline, setOutline] = createSignal<Sym[] | null>(null);
  const [outlineFilter, setOutlineFilter] = createSignal("");
  const [outlineSel, setOutlineSel] = createSignal(0);
  let outlineInput: HTMLInputElement | undefined;

  const outlineMatches = createMemo(() => {
    const all = outline();
    if (!all) return [];
    const q = outlineFilter().trim().toLowerCase();
    return q ? all.filter((s) => s.name.toLowerCase().includes(q)) : all;
  });

  async function showOutline() {
    if (props.preview) return;
    const h = lspHandle();
    if (!h) {
      flashLsp("No language server attached");
      return;
    }
    try {
      flushLspBuffer();
      const res = await h.documentSymbols(lspRel());
      if (res.status !== LSP_STATUS_OK) {
        flashLsp(
          res.status === LSP_STATUS_WARMING
            ? "Language server warming up…"
            : `No outline: ${res.detail || lspStatusText(res.status)}`,
        );
        return;
      }
      const syms = res.records
        .filter(
          (r): r is Extract<LspQueryRecord, { kind: "symbol" }> =>
            r.kind === "symbol",
        )
        .map((r) => ({
          name: r.name,
          symKind: r.symKind,
          depth: r.depth,
          line: r.line,
          col: r.col,
        }));
      if (!syms.length) {
        flashLsp("No symbols in this file");
        return;
      }
      setRefs(null);
      setOutlineFilter("");
      setOutlineSel(0);
      setOutline(syms);
      queueMicrotask(() => outlineInput?.focus());
    } catch {
      flashLsp("Outline failed");
    }
  }

  const closeOutline = () => {
    setOutline(null);
    view?.focus();
  };

  const jumpToSymbol = (s: Sym) => {
    closeOutline();
    jumpTo({ path: lspRel(), line: s.line, col: s.col });
  };

  // Arrow keys move the highlight, Enter jumps, Escape closes — the input
  // keeps focus throughout so filtering never needs a second click.
  const onOutlineKey = (e: KeyboardEvent) => {
    const list = outlineMatches();
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (!list.length) return;
      const d = e.key === "ArrowDown" ? 1 : -1;
      setOutlineSel((i) => (i + d + list.length) % list.length);
    } else if (e.key === "Enter") {
      e.preventDefault();
      const s = list[outlineSel()];
      if (s) jumpToSymbol(s);
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeOutline();
    }
  };

  // ── Live LSP (docs/design/lsp.md "LSP_BUFFER"): while this editor holds
  //    the file, language servers see its buffer, not disk. Debounced on
  //    content changes; flushed before every query, so answers and
  //    diagnostics track keystrokes rather than saves.
  // Real content has loaded at least once — the overlay gate. Distinct
  // from status(): after a re-establish the fs sync flips "loading" and
  // a dirty buffer never returns to "ready" (the dirty guard in
  // loadNode), yet the buffer is exactly what LSP should see.
  const [contentReal, setContentReal] = createSignal(false);
  // Well under the server's BLIT_LSP_BUFFER_MAX (8 MiB) and the 16 MiB
  // frame limit: an oversized buffer must degrade to a release (saved-
  // state intelligence), never to an oversized frame that would kill
  // the whole connection.
  const LSP_BUFFER_CLIENT_MAX = 4 * 1024 * 1024;
  let lspBufTimer: ReturnType<typeof setTimeout> | undefined;
  let lspBufPending = false;
  let lspOverlayReleased = false;
  const sendLspBuffer = () => {
    lspBufPending = false;
    const h = lspHandle();
    if (!h || !view) return;
    const bytes = encoder.encode(view.state.doc.toString());
    if (bytes.length > LSP_BUFFER_CLIENT_MAX) {
      if (!lspOverlayReleased) {
        lspOverlayReleased = true;
        h.releaseBuffer(lspRel());
      }
      return;
    }
    lspOverlayReleased = false;
    h.buffer(lspRel(), bytes);
  };
  const scheduleLspBuffer = () => {
    lspBufPending = true;
    clearTimeout(lspBufTimer);
    lspBufTimer = setTimeout(sendLspBuffer, 150);
  };
  const flushLspBuffer = () => {
    if (!lspBufPending) return;
    clearTimeout(lspBufTimer);
    sendLspBuffer();
  };
  onCleanup(() => clearTimeout(lspBufTimer));
  // A fresh attachment (first open, or re-opened after a re-establish) gets
  // the current buffer once the content is real; afterwards edits stream
  // from the update listener. Closing the attachment (editor teardown,
  // disconnect) releases the overlay server-side — no explicit release.
  createEffect(() => {
    const h = lspHandle();
    const real = contentReal();
    if (!h || props.preview) return;
    // A fresh attachment knows nothing: reset the released latch so an
    // oversized buffer re-announces its release to the new lsp_id.
    lspOverlayReleased = false;
    if (real) scheduleLspBuffer();
  });

  const lspCompletions = async (
    ctx: CompletionContext,
  ): Promise<CompletionResult | null> => {
    const h = lspHandle();
    if (!h) {
      // An explicit Ctrl+Space deserves an answer even when nothing can
      // answer it; auto-activation stays silent.
      if (ctx.explicit) flashLsp("No language server attached");
      return null;
    }
    const word = ctx.matchBefore(/[\w$]+/);
    if (!ctx.explicit && !word) {
      // Not mid-word: only member/path punctuation warrants a query.
      const before = ctx.state.sliceDoc(Math.max(0, ctx.pos - 1), ctx.pos);
      if (!/[.:>/]/.test(before)) return null;
    }
    flushLspBuffer(); // transport order: the query answers against these bytes
    const line = ctx.state.doc.lineAt(ctx.pos);
    const byteCol = encoder.encode(
      line.text.slice(0, ctx.pos - line.from),
    ).length;
    let res: LspQueryResult;
    try {
      res = await h.completion(lspRel(), line.number - 1, byteCol);
    } catch {
      if (ctx.explicit) flashLsp("Completion failed");
      return null;
    }
    if (ctx.aborted) return null;
    if (res.status !== LSP_STATUS_OK) {
      if (ctx.explicit) {
        // NOT_FOUND means no attached backend covers this file's
        // language (discovery is marker-driven: a .ts file outside any
        // package.json/tsconfig project gets no tsserver). Name what IS
        // attached so the reason is visible, not a guess.
        const ids = [...h.state.servers.values()].map((s) => s.id);
        flashLsp(
          res.status === LSP_STATUS_WARMING
            ? "Language server warming up…"
            : ids.length
              ? `No completion for this file type (attached here: ${ids.join(", ")})`
              : "No language server for this workspace",
        );
      }
      return null;
    }
    let from = word?.from ?? ctx.pos;
    const options: Completion[] = [];
    for (const r of res.records) {
      if (r.kind !== "completion") continue;
      if (
        options.length === 0 &&
        !(r.line === 0 && r.col === 0 && r.endLine === 0 && r.endCol === 0)
      ) {
        // The first item's replace range names the word start (the zero
        // range means "client picks its own boundary").
        const off = lspToOffset(r.line, r.col);
        if (off <= ctx.pos) from = off;
      }
      // A snippet keeps its tab stops instead of being flattened: Tab
      // walks the placeholders, which is the whole reason a server sends
      // `println!($1)` rather than `println!()`.
      const isSnippet = (r.flags & LSP_COMPLETION_SNIPPET) !== 0;
      const insert = r.insert === "" ? r.label : r.insert;
      options.push({
        label: r.label,
        type: CM_COMPLETION_TYPES[r.itemKind],
        detail: r.detail || undefined,
        apply: isSnippet
          ? snippet(lspSnippetToCm(insert))
          : insert === r.label
            ? undefined
            : insert,
        boost:
          r.flags & LSP_COMPLETION_PRESELECT
            ? 1
            : r.flags & LSP_COMPLETION_DEPRECATED
              ? -1
              : 0,
      });
    }
    if (options.length === 0) {
      if (ctx.explicit) flashLsp("No completions");
      return null;
    }
    // An incomplete list must re-query as typing continues (no validFor);
    // a complete one filters client-side while the word grows.
    return { from, options, validFor: res.incomplete ? undefined : /^[\w$]*$/ };
  };

  // Signature help: queried on "(" and ",", dismissed on ")", Escape,
  // blur, or leaving the line. The active signature arrives first with
  // the active parameter's byte range inside its label.
  let sigLine = 0;
  // Every dismissal bumps the generation, so a response still in flight
  // when the user typed ")" / Escape / blurred cannot resurrect the
  // tooltip after it was (or would have been) dismissed.
  let sigGen = 0;
  const closeSignature = () => {
    sigGen++;
    if (view && view.state.field(sigTooltipField, false) != null)
      view.dispatch({ effects: setSigTooltip.of(null) });
  };
  const querySignature = async () => {
    const h = lspHandle();
    const at = cursorLsp();
    if (!h || !view || !at) return;
    flushLspBuffer();
    const gen = ++sigGen;
    const anchor = view.state.selection.main.head;
    let res: LspQueryResult;
    try {
      res = await h.signatureHelp(lspRel(), at.line, at.col);
    } catch {
      return;
    }
    if (!view || gen !== sigGen) return;
    const sigs = res.records.filter(
      (r): r is Extract<LspQueryRecord, { kind: "signature" }> =>
        r.kind === "signature",
    );
    if (res.status !== LSP_STATUS_OK || sigs.length === 0) {
      closeSignature();
      return;
    }
    const sig = sigs[0];
    const pos = Math.min(anchor, view.state.doc.length);
    sigLine = view.state.doc.lineAt(pos).number;
    view.dispatch({
      effects: setSigTooltip.of({
        pos,
        above: true,
        create: () => {
          const dom = document.createElement("div");
          dom.className = "blit-signature";
          const label = document.createElement("div");
          if (sig.paramEnd > sig.paramStart) {
            // paramStart/End are UTF-8 byte offsets into the label.
            const bytes = encoder.encode(sig.label);
            label.append(decoder.decode(bytes.subarray(0, sig.paramStart)));
            const active = document.createElement("b");
            active.textContent = decoder.decode(
              bytes.subarray(sig.paramStart, sig.paramEnd),
            );
            label.append(active);
            label.append(decoder.decode(bytes.subarray(sig.paramEnd)));
          } else {
            label.textContent = sig.label;
          }
          if (sigs.length > 1) {
            const more = document.createElement("span");
            more.className = "blit-signature-count";
            more.textContent = ` +${sigs.length - 1}`;
            label.append(more);
          }
          dom.append(label);
          const docLine = sig.doc.split("\n", 1)[0];
          if (docLine) {
            const d = document.createElement("div");
            d.className = "blit-signature-doc";
            d.textContent = docLine;
            dom.append(d);
          }
          return { dom };
        },
      }),
    });
  };

  /** Re-derive indentation from the document and reconfigure. */
  function applyIndent(text: string) {
    if (!view) return;
    const { unit, tabSize } = detectIndent(text);
    view.dispatch({
      effects: indentComp.reconfigure([
        indentUnit.of(unit),
        EditorState.tabSize.of(tabSize),
      ]),
    });
  }

  function setDoc(text: string) {
    if (!view) return;
    // Identical content (our own save echoing back through fs-sync, or a
    // re-load of unchanged bytes) must not touch the document: a full replace
    // would blink the editor and collapse the cursor to offset 0.
    if (view.state.doc.toString() === text) {
      setDirty(false);
      setExternalChanged(false);
      return;
    }
    applying = true;
    // Preserve the cursor across a genuine reload by clamping the current
    // selection into the new document, rather than letting the whole-doc
    // replace reset it to the start.
    const len = text.length;
    const sel = view.state.selection.main;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: text },
      selection: {
        anchor: Math.min(sel.anchor, len),
        head: Math.min(sel.head, len),
      },
    });
    applying = false;
    // A different file (or a wholesale external rewrite) can have different
    // conventions from the last one this view showed.
    applyIndent(text);
    setDirty(false);
    setExternalChanged(false);
  }

  // A "reveal this line" request from the commit view, a diff, or the
  // Problems list, applied after the first load. The line's text relocates it
  // in the live file (line numbers drift); the line number is a nearest-match
  // tie-breaker. Consumed here for the tile this request just opened, and
  // again from the effect below for every later request landing on an
  // already-mounted tile.
  let revealIntent = consumeReveal(props.connectionId, props.path);

  function applyReveal(): boolean {
    if (!revealIntent || !view) return false;
    const r = revealIntent;
    revealIntent = null; // once
    const doc = view.state.doc;
    let bestLine = Math.min(Math.max(r.line, 1), doc.lines);
    const target = r.text.trim();
    if (target) {
      let bestDist = Infinity;
      for (let ln = 1; ln <= doc.lines; ln++) {
        if (doc.line(ln).text.trim() === target) {
          const dist = Math.abs(ln - r.line);
          if (dist < bestDist) {
            bestDist = dist;
            bestLine = ln;
          }
        }
      }
    }
    const line = doc.line(Math.min(Math.max(bestLine, 1), doc.lines));
    // Land on the byte column whenever one was given. Text relocation finds
    // the right *line*; a column is still meaningful inside it — more so,
    // in fact, since a line matched by its exact text has the same byte
    // offsets it had at the source. Ignoring `col` here was what made a
    // search hit land at the start of the line instead of on the match.
    let anchor = line.from;
    if (r.col != null) {
      const bytes = encoder.encode(line.text);
      const ch = decoder.decode(
        bytes.subarray(0, Math.min(r.col, bytes.length)),
      ).length;
      anchor = line.from + Math.min(ch, line.length);
    }
    view.dispatch({
      selection: { anchor },
      effects: EditorView.scrollIntoView(anchor, { y: "center" }),
    });
    view.focus();
    return true;
  }

  // Later reveal requests for a file whose tile is already open: the editor
  // is already mounted, so nothing would consume them. Gated on the buffer
  // being real — consuming against a placeholder document would land the
  // cursor on the wrong line and drop the intent.
  createEffect(() => {
    revealVersion();
    if (!contentReal()) return;
    const next = consumeReveal(props.connectionId, props.path);
    if (!next) return;
    revealIntent = next;
    applyReveal();
  });

  // Restore the cursor + scroll saved when this file's editor was last torn
  // down (navigation re-creates the tile). Only meaningful on the first load;
  // an explicit reveal (LSP jump / diff click) takes precedence.
  let firstLoad = true;
  function restoreSavedPosition() {
    if (!view) return;
    const p = recallEditorPosition(props.connectionId, props.path);
    if (!p) return;
    const len = view.state.doc.length;
    view.dispatch({
      selection: {
        anchor: Math.min(p.anchor, len),
        head: Math.min(p.head, len),
      },
    });
    const top = p.top;
    requestAnimationFrame(() => {
      if (view) view.scrollDOM.scrollTop = top;
    });
  }

  // Guards overlapping loads: each fetch is async, so two rapid external
  // changes could otherwise resolve out of order and leave the older
  // content (and CAS base) in place.
  let loadGen = 0;

  async function loadNode(h: FsSyncHandle, node: FsNode, force = false) {
    // A re-opened sync (re-establish, transient retry) must never replace a
    // dirty buffer silently — that would discard unsaved edits with no
    // confirmation. Banner instead; Reload / Overwrite decide. Explicit
    // reloads (Discard, the conflict banner's Reload) pass force.
    if (!force && !firstLoad && dirty()) {
      if (node.hash !== lastHash) setExternalChanged(true);
      return;
    }
    const gen = ++loadGen;
    let bytes = node.content;
    if (!bytes && node.entryFlags & FS_ENTRY_NO_CONTENT) {
      try {
        bytes = await h.fetch(fileKey());
      } catch (e) {
        if (gen !== loadGen) return;
        // Past `inlineMax` this fetch is the only content source. Refusing
        // to render beats opening an empty buffer whose CAS base is the
        // real file's hash (a save would truncate the file). The next
        // upsert (or a reconnect re-open) retries the load.
        setError(e instanceof Error ? e.message : String(e));
        setStatus("error");
        return;
      }
      if (gen !== loadGen) return; // a newer load superseded this one
    }
    lastHash = node.hash;
    lastDiskBytes = bytes ?? new Uint8Array();
    fsRetries = 0; // a successful load clears the retry budget
    setDoc(bytes ? decoder.decode(bytes) : "");
    setContentReal(true); // the overlay gate: the buffer now means something
    // The buffer now mirrors disk: any pending conflict or external-change
    // banner is resolved, and there is nothing dirty left to save.
    setDirty(false);
    setConflict(false);
    setExternalChanged(false);
    if (status() !== "readonly") setStatus("ready");
    const revealed = applyReveal();
    if (firstLoad) {
      firstLoad = false;
      if (!revealed) restoreSavedPosition();
      // A parked dirty buffer (docs/design/kv.md): restore it when its disk
      // base still matches; when the disk moved on, leave disk content and
      // say so — never silently resurrect a stale buffer.
      void recallParkedBuffer(
        props.workspace,
        props.connectionId,
        props.path,
      ).then((parked) => {
        if (!parked || !view || dirty()) return;
        if (parked.base === lastHash) {
          applying = true;
          view.dispatch({
            changes: {
              from: 0,
              to: view.state.doc.length,
              insert: decoder.decode(parked.content),
            },
          });
          applying = false;
          setDirty(true);
        } else {
          flashLsp(
            "Unsaved buffer parked for this file no longer matches disk",
          );
        }
      });
    }
  }

  function onFsRecord(r: FsRecord) {
    const h = handle();
    if (!h) return;
    const key = fileKey();
    // A single-file sync stays open across delete/rename-away — the file's
    // absence is state, not failure (docs/design/fs-watch.md § Single-file
    // sync) — so a recreate flows as an ordinary upsert of `""` and the
    // load below recovers from the error status.
    // Losing the file is not losing the buffer: keep the document on
    // screen and banner it, rather than replacing the pane with an error
    // and taking unsaved work out of reach. The CAS base drops to 0 so an
    // explicit Save recreates the file create-exclusively — which also
    // means it refuses if something else got there first.
    if (r.kind === "move" && r.from === key) {
      lastHash = 0n;
      lastDiskBytes = null;
      setGone("renamed");
      return;
    }
    if (r.kind === "delete" && r.path === key) {
      lastHash = 0n;
      lastDiskBytes = null;
      setGone("deleted");
      return;
    }
    if (r.kind === "move" || r.kind === "delete") return;
    if (r.path !== key) return;
    const node = h.live.get(key);
    if (!node) return;
    // Back on disk (recreated, or the rename reversed).
    setGone(null);
    if (h.lastWrittenHash(key) === node.hash) {
      lastHash = node.hash; // our own write echoing back — ignore
      return;
    }
    if (node.hash === lastHash) return; // unchanged
    if (!dirty()) void loadNode(h, node);
    else setExternalChanged(true);
  }

  async function save() {
    const h = handle();
    if (!h || !view) return;
    const bytes = encoder.encode(view.state.doc.toString());
    // Beat the 150 ms overlay debounce: a save issued right after a
    // keystroke would otherwise land on disk while the server still
    // holds the previous buffer, and the didSave that the write triggers
    // would diagnose the older text.
    flushLspBuffer();
    try {
      const res = await h.writeFile(fileKey(), bytes, {
        ifHash: lastHash,
        // The disk bytes `ifHash` names: core ships a delta when smaller
        // (docs/design/fs-write.md `content_kind` 2), transparently sending
        // full bytes to servers without delta writes. Meaningless without a
        // CAS base (ifHash 0 = create-exclusive), hence the gate.
        deltaBase: lastHash !== 0n ? (lastDiskBytes ?? undefined) : undefined,
      });
      lastHash = res.hash;
      lastDiskBytes = bytes;
      setDirty(false);
      setConflict(false);
      setExternalChanged(false);
      setGone(null); // the write put it back on disk
      clearParkedBuffer(props.workspace, props.connectionId, props.path);
    } catch (e) {
      if (e instanceof FsConflictError) {
        setConflict(true);
      } else if (e instanceof Error && /permission/i.test(e.message)) {
        setStatus("readonly");
      } else {
        setError(e instanceof Error ? e.message : String(e));
      }
    }
  }

  async function overwrite() {
    const h = handle();
    if (!h || !view) return;
    const bytes = encoder.encode(view.state.doc.toString());
    flushLspBuffer();
    try {
      const res = await h.writeFile(fileKey(), bytes, { force: true });
      lastHash = res.hash;
      lastDiskBytes = bytes;
      setDirty(false);
      setConflict(false);
      setExternalChanged(false);
      setGone(null); // the write put it back on disk
      clearParkedBuffer(props.workspace, props.connectionId, props.path);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  function reload() {
    const h = handle();
    const node = h?.live.get(fileKey());
    clearParkedBuffer(props.workspace, props.connectionId, props.path);
    if (h && node) void loadNode(h, node, true);
  }

  // Autosave on focus change / tab hide / teardown. save() is CAS-guarded, so a
  // stale write can't clobber an external change — it just surfaces the conflict
  // banner. Status-bar action buttons suppress the editor blur (preventDefault
  // on mousedown), so clicking Discard/Save doesn't autosave out from under them.
  const autosave = () => {
    if (props.preview) return;
    // Never autosave a file that was deleted or renamed away: recreating
    // it because focus moved would undo a deliberate `rm` or branch
    // switch. Save stays available as an explicit act (see the banner);
    // the park below still keeps the buffer safe either way.
    if (dirty() && status() !== "readonly" && !gone()) void save();
    // Flush any pending buffer park too: crash insurance for the case the
    // disk save above refuses (conflict) or never lands (transport down).
    if (dirty())
      flushParkedBuffer(
        props.workspace,
        props.connectionId,
        props.path,
        bufBytes,
        bufBase,
      );
  };
  // Parked-buffer plumbing (docs/design/kv.md § First consumer): the KV
  // envelope carries the buffer bytes + the disk hash they diverged from.
  const bufBytes = () => encoder.encode(view?.state.doc.toString() ?? "");
  const bufBase = () => lastHash;

  onMount(() => {
    view = new EditorView({
      parent: host,
      state: EditorState.create({
        doc: "",
        extensions: [
          lineNumbers(),
          highlightActiveLine(),
          highlightActiveLineGutter(),
          foldGutter(),
          drawSelection(),
          dropCursor(),
          history(),
          bracketMatching(),
          indentOnInput(),
          wrapComp.of(lineWrap() ? EditorView.lineWrapping : []),
          // Control characters, zero-width joiners and the bidi overrides
          // render as visible placeholders instead of nothing. In a tool
          // whose whole job is showing you what is really in a file, an
          // invisible character that changes what code means is the one
          // thing the editor must not hide.
          highlightSpecialChars(),
          // Isolate right-to-left runs so a string or comment containing
          // Arabic or Hebrew can't visually reorder the code around it.
          bidiIsolates(),
          search({ top: true, createPanel: blitSearchPanel }),
          highlightSelectionMatches(),
          EditorState.allowMultipleSelections.of(true),
          // ⌘-click is go-to-definition here (see the mousedown handler
          // below), so CM's own default for adding a cursor — ⌘-click on
          // a Mac — could never fire. Alt-click adds cursors instead,
          // which leaves Alt+Shift-drag free for column selection rather
          // than having the two gestures fight over Alt.
          EditorView.clickAddsSelectionRange.of((e) => e.altKey && !e.shiftKey),
          rectangularSelection({
            eventFilter: (e) => e.altKey && e.shiftKey && e.button === 0,
          }),
          crosshairCursor({ key: "Alt" }),
          keymap.of([
            {
              key: "Mod-s",
              run: () => (void save(), true),
              preventDefault: true,
            },
            {
              key: "F12",
              run: () => (void goToDefinition(), true),
              preventDefault: true,
            },
            {
              key: "Shift-F12",
              run: () => (void findReferences(), true),
              preventDefault: true,
            },
            {
              key: "F2",
              run: () => (startRename(), true),
              preventDefault: true,
            },
            {
              key: "Mod-Shift-o",
              run: () => (void showOutline(), true),
              preventDefault: true,
            },
            {
              // Alt-Z, as in most editors.
              key: "Alt-z",
              run: () => (toggleLineWrap(), true),
              preventDefault: true,
            },
            {
              // Close signature help first; completion's own Escape (a
              // higher-precedence binding) already ran if a popup was open.
              key: "Escape",
              run: () => {
                if (view?.state.field(sigTooltipField, false) != null) {
                  closeSignature();
                  return true;
                }
                return false;
              },
            },
            // Tab accepts an open completion (Enter already does, via
            // the completion keymap); with no popup it falls through to
            // indentWithTab.
            { key: "Tab", run: acceptCompletion },
            // Between the two on purpose: CodeMirror's own snippetKeymap
            // binds Tab, but `indentWithTab` always returns true, so
            // without this the placeholders of a just-accepted snippet
            // were unreachable — Tab indented instead of advancing.
            {
              key: "Tab",
              run: nextSnippetField,
              shift: prevSnippetField,
            },
            indentWithTab,
            // Ahead of defaultKeymap: closeBrackets needs Backspace to
            // delete a pair, and search needs Mod-d before anything else
            // claims it.
            ...closeBracketsKeymap,
            ...searchKeymap,
            ...defaultKeymap,
            ...historyKeymap,
            ...foldKeymap,
          ]),
          // ⌘/Ctrl-click a symbol → go to its definition.
          EditorView.domEventHandlers({
            focus: () => {
              if (!props.preview) registerActiveEditor(controller);
              return false;
            },
            blur: () => {
              autosave();
              closeSignature();
              return false;
            },
            mousedown: (e, v) => {
              if (!(e.metaKey || e.ctrlKey) || e.button !== 0) return false;
              const pos = v.posAtCoords({ x: e.clientX, y: e.clientY });
              if (pos == null) return false;
              v.dispatch({ selection: { anchor: pos } });
              e.preventDefault();
              void goToDefinition();
              return true;
            },
          }),
          themeComp.of(
            cmTheme(
              props.theme,
              props.palette,
              props.fontFamily,
              props.fontSize,
            ),
          ),
          langComp.of([]),
          ...(props.preview
            ? [EditorState.readOnly.of(true), EditorView.editable.of(false)]
            : [
                autocompletion({ override: [lspCompletions] }),
                closeBrackets(),
                sigTooltipField,
                hoverSource,
                // Diagnostic markers in the gutter, so a broken file reads
                // as broken without opening the Problems panel.
                lintGutter(),
                // CodeMirror's own panel/prompt strings, localized.
                cmPhrases(),
                // Empty until the document lands; applyIndent fills it in.
                indentComp.of([]),
                // F8 / Shift-F8 step through this file's diagnostics, and
                // Mod-Shift-M lists them. The gutter already showed where
                // the errors were; without this there was no way to reach
                // one from the keyboard — you had to aim at a marker.
                keymap.of(lintKeymap),
                highlightTrailingWhitespace(),
                scrollPastEnd(),
              ]),
          EditorView.updateListener.of((u) => {
            if (u.docChanged && !applying) {
              setDirty(true);
              if (!props.preview)
                parkBuffer(
                  props.workspace,
                  props.connectionId,
                  props.path,
                  bufBytes,
                  bufBase,
                );
            }
            if (u.docChanged && !props.preview) {
              // Every content change — user edits and programmatic
              // reloads alike — streams to the LSP buffer overlay.
              scheduleLspBuffer();
              if (u.transactions.some((t) => t.isUserEvent("input.type"))) {
                let typed = "";
                u.changes.iterChanges(
                  (_fa, _ta, _fb, _tb, ins) => (typed = ins.toString()),
                );
                if (typed === "(" || typed === ",") void querySignature();
                else if (typed === ")") closeSignature();
              }
            }
            if (
              u.selectionSet &&
              !u.docChanged &&
              u.state.field(sigTooltipField, false) != null &&
              u.state.doc.lineAt(u.state.selection.main.head).number !== sigLine
            ) {
              closeSignature();
            }
          }),
        ],
      }),
    });
    // Load the language grammar on demand and slot it into the compartment.
    void loadLangForFile(fileName).then((lang) => {
      if (lang && view) view.dispatch({ effects: langComp.reconfigure(lang) });
    });
  });

  // A CM blur only fires when focus moves within the page; switching tab/app or
  // hiding the window leaves the editor "focused", so autosave on those too.
  onMount(() => {
    const onHide = () => {
      if (document.visibilityState === "hidden") autosave();
    };
    window.addEventListener("blur", autosave);
    document.addEventListener("visibilitychange", onHide);
    onCleanup(() => {
      window.removeEventListener("blur", autosave);
      document.removeEventListener("visibilitychange", onHide);
    });
  });

  // Follow the shared soft-wrap preference, so toggling it in one editor
  // reaches every open tile rather than just the focused one.
  createEffect(() => {
    const on = lineWrap();
    view?.dispatch({
      effects: wrapComp.reconfigure(on ? EditorView.lineWrapping : []),
    });
  });

  // Re-theme on palette / font change.
  createEffect(() => {
    view?.dispatch({
      effects: themeComp.reconfigure(
        cmTheme(props.theme, props.palette, props.fontFamily, props.fontSize),
      ),
    });
  });

  // Open this file's sync. Preferred: a single-file content sync of the file
  // itself (docs/design/fs-watch.md § Single-file sync) — one hashed entry,
  // no sibling mirroring. Fallback ("dir"): a content sync of the parent
  // directory, for servers that predate the SINGLE flag and refuse the open.
  // Content (not metadata-only) in both modes: metadata-only syncs never
  // hash (FS_FILE fetch replies carry no hash either), so a fetch-based
  // editor has no valid CAS base and every save of an existing file
  // conflicts — `ifHash: 0` means create-exclusive (docs/design/fs-write.md).
  createEffect(() => {
    const connectionId = props.connectionId;
    const mode = syncMode(); // flipping to "dir" re-opens via this read
    connGen(); // re-open after a connection reset
    fsRetry(); // re-attempt after a transient (reset-clobbered) open
    if (!connConnected()) {
      // Wait for the connection; this effect re-runs when it connects.
      setStatus("loading");
      setError(null);
      return;
    }
    if (!props.path) {
      // The opener could not resolve an absolute path — its session had no
      // synced root (session.ts `abs`). The tile's path is fixed at creation,
      // so this cannot recover; say what is wrong rather than blaming the file
      // or spinning on "loading" forever. Openers avoid creating such a tile.
      setStatus("error");
      setError("No path — the file tree had no synced root when this opened");
      return;
    }
    let disposed = false;
    let opened: FsSyncHandle | null = null;
    let limitTimer: ReturnType<typeof setTimeout> | null = null;
    // The file's initial load is resolved once — either it loads, or it's
    // confirmed missing after the snapshot is coherent (onSync).
    let initialResolved = false;
    const tryInitialLoad = () => {
      if (disposed || initialResolved || !opened) return;
      const node = opened.live.get(fileKey());
      if (node) {
        initialResolved = true;
        void loadNode(opened, node);
      }
    };
    const shared = {
      content: true,
      inlineMax: 8 * 1024 * 1024,
      onRecord: onFsRecord,
      onSync: () => {
        // The snapshot is now coherent. syncFs resolves the moment the
        // server *accepts* the sync — before any entries land — so the
        // file only reliably appears here. Only once coherent is a
        // still-absent file genuinely "not found".
        if (disposed || initialResolved) return;
        tryInitialLoad();
        if (!initialResolved) {
          initialResolved = true;
          setStatus("error");
          setError("File not found");
        }
      },
      onClosed: () => {
        // A sync closes on a connection reset (server re-establish), or —
        // single mode — when the parent directory vanishes. Recover: show
        // loading, not a dead-end error, and retry — connGen already
        // bumped for a reset, so it won't re-run on its own. (A vanished
        // parent makes the retried open reject "not found" below.)
        if (!disposed) {
          setStatus("loading");
          setError(null);
          retryFs();
        }
      },
    };
    props.workspace
      .syncFs(
        connectionId,
        mode === "single" ? props.path : parentDir,
        mode === "single"
          ? { ...shared, single: true }
          : { ...shared, recursive: false },
      )
      .then((h) => {
        if (disposed) {
          h.stop();
          return;
        }
        opened = h;
        setHandle(h);
        // `live` may still be empty here (entries stream in via onSync). Load if
        // the file is already present; otherwise wait for onSync to decide.
        tryInitialLoad();
      })
      .catch((e: unknown) => {
        if (disposed) return;
        // A transient failure right after a reset (server mid-handshake) — stay
        // in loading; the connGen bump will retry. Otherwise surface it.
        const msg = e instanceof Error ? e.message : String(e);
        if (
          /re-established|shutting down|transport is|not connected/i.test(msg)
        ) {
          setStatus("loading");
          setError(null);
          retryFs();
        } else if (/resource limit/i.test(msg)) {
          // The per-connection sync cap — transient in practice (slots free
          // as idle sessions expire and dock cards close). Show the error
          // but keep re-attempting instead of dead-ending the editor.
          setStatus("error");
          setError(msg);
          limitTimer = setTimeout(() => retryFs(), 3000);
        } else if (
          mode === "single" &&
          !/not found|permission denied/i.test(msg)
        ) {
          // An old server refusing the SINGLE flag — missing-file and
          // permission are real answers about the file, anything else is
          // the flag itself being rejected. Fall back to the parent-dir
          // content sync (this effect re-runs on the mode flip).
          setSyncMode("dir");
        } else {
          setStatus("error");
          // Match dir mode's phrasing for a file the single open never found.
          setError(
            mode === "single" && /not found/i.test(msg)
              ? "File not found"
              : msg,
          );
        }
      });
    onCleanup(() => {
      disposed = true;
      if (limitTimer) clearTimeout(limitTimer);
      opened?.stop();
    });
  });

  // Open an LSP attachment for diagnostics + navigation (best-effort; a project
  // with no language server just stays silent). Gated on connGen + a retry so a
  // re-establish re-attaches (the old attachment is reset server-side).
  createEffect(() => {
    if (props.preview) return; // previews stay LSP-free
    const connectionId = props.connectionId;
    connGen(); // re-open after a connection reset
    lspRetry(); // re-attempt after a transient (reset-clobbered) open
    if (!connConnected()) return; // re-runs when the connection connects
    let disposed = false;
    let opened: LspHandle | null = null;
    let unsub: () => void = () => {};
    props.workspace
      .openLsp(connectionId, parentDir, { diagnostics: true })
      .then((h) => {
        if (disposed) {
          h.close();
          return;
        }
        lspRetries = 0;
        opened = h;
        unsub = h.subscribe(() => setLspVersion((v) => v + 1));
        setLspHandle(h);
      })
      .catch((e: unknown) => {
        // Transient (the open raced a re-establish reset) — retry; otherwise
        // (no language server) stay silent.
        if (!disposed && isTransientConn(e)) retryLsp();
      });
    onCleanup(() => {
      disposed = true;
      unsub();
      opened?.close();
      setLspHandle(null);
    });
  });

  // Push diagnostics into CM6 as squiggles, transcoding UTF-8 byte columns to
  // document offsets (LSP positions are 0-based line + byte col).
  // The mirror replaces a file's LspFileDiags entry only when that file's set
  // changes, so its identity fingerprints this file across pushes: an
  // unchanged set skips the CM dispatch (CM maps the stored ranges through
  // doc edits itself).
  let lastDiagsHandle: LspHandle | null = null;
  let lastDiags: LspFileDiags | undefined;
  createEffect(() => {
    lspVersion();
    const h = lspHandle();
    if (!view || !h) return;
    const fileDiags = h.diags.files.get(lspRel());
    if (h === lastDiagsHandle && fileDiags === lastDiags) return;
    lastDiagsHandle = h;
    lastDiags = fileDiags;
    const doc = view.state.doc;
    const toOffset = (line0: number, byteCol: number) => {
      const lineNo = Math.min(Math.max(line0, 0) + 1, doc.lines);
      const line = doc.line(lineNo);
      const bytes = encoder.encode(line.text);
      const ch = decoder.decode(
        bytes.subarray(0, Math.min(byteCol, bytes.length)),
      ).length;
      return line.from + Math.min(ch, line.length);
    };
    const diags: Diagnostic[] = (fileDiags?.diags ?? []).map((d) => {
      const from = toOffset(d.line, d.col);
      const to = Math.max(from, toOffset(d.endLine, d.endCol));
      return {
        from,
        to,
        severity:
          d.severity === LSP_SEVERITY_ERROR
            ? "error"
            : d.severity === LSP_SEVERITY_WARNING
              ? "warning"
              : "info",
        message: d.source ? `${d.msg} (${d.source})` : d.msg,
      };
    });
    view.dispatch(setDiagnostics(view.state, diags));
  });

  onCleanup(() => {
    // Save on teardown (navigation away / tile close) before the fs handle is
    // torn down — the write request is in flight regardless of the unmount.
    autosave();
    if (view) {
      const sel = view.state.selection.main;
      rememberEditorPosition(props.connectionId, props.path, {
        anchor: sel.anchor,
        head: sel.head,
        top: view.scrollDOM.scrollTop,
      });
    }
    view?.destroy();
  });

  const banner = (): EditorBanner => {
    if (status() === "readonly")
      return { text: "Read-only host", tone: "warn" };
    // Ahead of the conflict banners: once the file is gone, "changed on
    // disk" is no longer the useful thing to say about it.
    const g = gone();
    if (g)
      return {
        text: `${g === "renamed" ? "Renamed away" : "Deleted"} on disk — Save recreates it.`,
        tone: "err",
      };
    if (conflict())
      return { text: "Save conflict — the file changed on disk.", tone: "err" };
    if (externalChanged())
      return { text: "Changed on disk while editing.", tone: "warn" };
    return null;
  };

  // The editor's chrome (filename + actions) lives in the global StatusBar, not
  // a per-tile header. Publish this editor as the active one when it gains
  // focus; clear it on unmount so the bar never reads a disposed signal.
  const controller: EditorController = {
    kind: "editor",
    connectionId: props.connectionId,
    path: props.path,
    dirty,
    banner,
    lspMsg,
    lspAvailable: () => lspHandle() != null,
    readOnly: () => status() === "readonly",
    conflicted: () => conflict() || externalChanged(),
    save: () => void save(),
    discard: reload,
    reload,
    overwrite: () => void overwrite(),
    goToDefinition: () => void goToDefinition(),
    findReferences: () => void findReferences(),
    renameSymbol: startRename,
    showOutline: () => void showOutline(),
    onOpenTile: props.onOpenTile,
  };
  // Register on mount as well as on focus: switching a tile from a diff to
  // its editor via the status-bar switcher replaces the tile without moving
  // keyboard focus, and mount-registration keeps the switcher in the bar
  // instead of dropping back to the terminal identity (diffs do the same).
  onMount(() => {
    if (!props.preview) registerActiveEditor(controller);
  });
  onCleanup(() => clearActiveEditor(controller));

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        "flex-direction": "column",
        background: props.theme.bg,
        overflow: "hidden",
      }}
    >
      <Show
        when={status() !== "error"}
        fallback={
          <div style={{ padding: "10px", color: props.theme.errorText }}>
            {error()}
          </div>
        }
      >
        {/* The CM host stays mounted; a loading overlay covers it until the
            first content load so the pane is never just an empty editor. */}
        <div style={{ flex: "1 1 0", "min-height": 0, position: "relative" }}>
          <div
            ref={host}
            style={{ width: "100%", height: "100%", overflow: "hidden" }}
          />
          <Show when={status() === "loading"}>
            <div
              style={{
                position: "absolute",
                inset: 0,
                display: "flex",
                "align-items": "center",
                "justify-content": "center",
                gap: "8px",
                background: props.theme.bg,
                color: props.theme.dimFg,
                "font-family": props.fontFamily,
                "font-size": `${Math.round(props.fontSize * 0.9)}px`,
              }}
            >
              <span
                style={{
                  display: "inline-block",
                  animation: "blit-spin 0.9s linear infinite",
                }}
              >
                ◐
              </span>
              Loading…
              <style>
                {"@keyframes blit-spin{to{transform:rotate(360deg)}}"}
              </style>
            </div>
          </Show>
          {/* Rename box, anchored under the symbol it will rename. */}
          <Show when={renaming()}>
            {(r) => (
              <div
                style={{
                  position: "absolute",
                  top: `${r().top}px`,
                  left: `${r().left}px`,
                  "max-width": "calc(100% - 8px)",
                  display: "flex",
                  "align-items": "center",
                  gap: "6px",
                  padding: "3px 6px",
                  background: props.theme.solidPanelBg,
                  border: `1px solid ${props.theme.border}`,
                  "border-radius": "3px",
                  "font-family": props.fontFamily,
                  "font-size": `${Math.round(props.fontSize * 0.85)}px`,
                  "box-shadow": "0 2px 8px rgba(0,0,0,0.35)",
                  "z-index": 5,
                }}
              >
                <span style={{ color: props.theme.dimFg }}>Rename to</span>
                <input
                  ref={renameInput}
                  value={r().oldName}
                  disabled={renameBusy()}
                  spellcheck={false}
                  autocapitalize="off"
                  onKeyDown={(e) => {
                    // Stop CM from seeing these: Escape would close a
                    // tooltip and Enter would insert a newline behind the
                    // box.
                    e.stopPropagation();
                    if (e.key === "Enter") {
                      e.preventDefault();
                      void commitRename(e.currentTarget.value);
                    } else if (e.key === "Escape") {
                      e.preventDefault();
                      cancelRename();
                    }
                  }}
                  onBlur={() => {
                    if (!renameBusy()) cancelRename();
                  }}
                  style={{
                    "min-width": "12ch",
                    background: props.theme.bg,
                    color: props.theme.fg,
                    border: `1px solid ${props.theme.subtleBorder}`,
                    "border-radius": "2px",
                    padding: "1px 4px",
                    "font-family": props.fontFamily,
                    "font-size": "inherit",
                  }}
                />
                <span style={{ color: props.theme.dimFg }}>
                  {renameBusy() ? "applying…" : "↵ apply · esc cancel"}
                </span>
              </div>
            )}
          </Show>
          {/* Document outline — type to filter, ↑↓ to move, ↵ to jump. */}
          <Show when={outline()}>
            <div
              style={{
                position: "absolute",
                left: 0,
                right: 0,
                bottom: 0,
                "max-height": "45%",
                display: "flex",
                "flex-direction": "column",
                background: props.theme.panelBg,
                "border-top": `1px solid ${props.theme.subtleBorder}`,
                "font-family": props.fontFamily,
                "font-size": `${Math.round(props.fontSize * 0.85)}px`,
              }}
            >
              <div
                style={{
                  display: "flex",
                  "align-items": "center",
                  gap: "8px",
                  padding: `${Math.round(props.fontSize * 0.3)}px 10px`,
                  "border-bottom": `1px solid ${props.theme.subtleBorder}`,
                  color: props.theme.dimFg,
                }}
              >
                <b style={{ color: props.theme.fg, "flex-shrink": 0 }}>
                  Outline
                </b>
                <input
                  ref={outlineInput}
                  value={outlineFilter()}
                  placeholder="filter symbols…"
                  spellcheck={false}
                  autocapitalize="off"
                  onInput={(e) => {
                    setOutlineFilter(e.currentTarget.value);
                    setOutlineSel(0);
                  }}
                  onKeyDown={onOutlineKey}
                  style={{
                    flex: 1,
                    "min-width": 0,
                    background: props.theme.bg,
                    color: props.theme.fg,
                    border: `1px solid ${props.theme.subtleBorder}`,
                    "border-radius": "2px",
                    padding: "1px 4px",
                    "font-family": props.fontFamily,
                    "font-size": "inherit",
                  }}
                />
                <span style={{ "flex-shrink": 0 }}>
                  {outlineMatches().length}
                </span>
                <button style={ui.btn} onClick={closeOutline}>
                  ✕
                </button>
              </div>
              <div
                style={{
                  "overflow-y": "auto",
                  ...scrollbarStyle(props.theme),
                }}
              >
                <For each={outlineMatches()}>
                  {(s, i) => (
                    <div
                      style={{
                        display: "flex",
                        gap: "8px",
                        padding: "2px 10px",
                        cursor: "pointer",
                        "white-space": "nowrap",
                        overflow: "hidden",
                        color: props.theme.fg,
                        background:
                          i() === outlineSel()
                            ? props.theme.selectedBg
                            : "transparent",
                      }}
                      title={`${s.name} — line ${s.line + 1}`}
                      onMouseEnter={() => setOutlineSel(i())}
                      onMouseDown={(e) => e.preventDefault()}
                      onClick={() => jumpToSymbol(s)}
                    >
                      <span
                        style={{
                          color: props.theme.dimFg,
                          "min-width": "5ch",
                          "flex-shrink": 0,
                        }}
                      >
                        {symbolKindTag(s.symKind)}
                      </span>
                      <span
                        style={{
                          overflow: "hidden",
                          "text-overflow": "ellipsis",
                        }}
                      >
                        {/* Nesting depth reads as indentation, matching
                            the pre-order the backend emits. Hard spaces:
                            HTML collapses a leading run of ordinary ones. */}
                        {"\u00a0".repeat(s.depth * 2)}
                        {s.name}
                      </span>
                      <span
                        style={{
                          "margin-left": "auto",
                          color: props.theme.dimFg,
                          "flex-shrink": 0,
                        }}
                      >
                        {s.line + 1}
                      </span>
                    </div>
                  )}
                </For>
              </div>
            </div>
          </Show>
          {/* Find-references results — click a row to jump. */}
          <Show when={refs()}>
            {(list) => (
              <div
                style={{
                  position: "absolute",
                  left: 0,
                  right: 0,
                  bottom: 0,
                  "max-height": "45%",
                  display: "flex",
                  "flex-direction": "column",
                  background: props.theme.panelBg,
                  "border-top": `1px solid ${props.theme.subtleBorder}`,
                  "font-family": props.fontFamily,
                  "font-size": `${Math.round(props.fontSize * 0.85)}px`,
                }}
              >
                <div
                  style={{
                    display: "flex",
                    "align-items": "center",
                    gap: "8px",
                    padding: `${Math.round(props.fontSize * 0.3)}px 10px`,
                    "border-bottom": `1px solid ${props.theme.subtleBorder}`,
                    color: props.theme.dimFg,
                  }}
                >
                  <b style={{ color: props.theme.fg }}>
                    {list().length === 0
                      ? "No references found."
                      : `${list().length} reference${list().length === 1 ? "" : "s"}`}
                  </b>
                  <button
                    style={{ ...ui.btn, "margin-left": "auto" }}
                    onClick={() => setRefs(null)}
                  >
                    ✕
                  </button>
                </div>
                <div
                  style={{
                    "overflow-y": "auto",
                    ...scrollbarStyle(props.theme),
                  }}
                >
                  <For each={list()}>
                    {(loc) => (
                      <div
                        style={{
                          padding: `2px 10px`,
                          cursor: "pointer",
                          "white-space": "nowrap",
                          overflow: "hidden",
                          "text-overflow": "ellipsis",
                          color: props.theme.fg,
                        }}
                        title={`${loc.path}:${loc.line + 1}`}
                        onClick={() => jumpTo(loc)}
                      >
                        <span style={{ color: props.theme.dimFg }}>
                          {loc.path}
                        </span>
                        <span style={{ color: props.theme.accent }}>
                          :{loc.line + 1}
                        </span>
                      </div>
                    )}
                  </For>
                </div>
              </div>
            )}
          </Show>
        </div>
      </Show>
    </div>
  );
}
