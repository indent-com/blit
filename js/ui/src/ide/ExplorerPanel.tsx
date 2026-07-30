/**
 * ExplorerPanel — a pure view over {@link IdeSession}'s file tree.
 *
 * All lifecycle (the lazy per-directory syncs, expand/collapse state, the
 * flattened row list) lives in the session, so this component only renders
 * `session.tree()` and forwards clicks to `session.toggleDir` /
 * `onOpenTile(session.fileAssignment(...))`.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  Show,
  untrack,
  type JSX,
} from "solid-js";
import {
  FS_ENTRY_FILE,
  FS_ENTRY_DIR,
  FS_ENTRY_SYMLINK,
  FS_ENTRY_UNREADABLE,
  FS_ENTRY_UNSTABLE,
  GIT_STATUS_ENTRY_CONFLICTED,
  GIT_HEAD_DETACHED,
  GIT_HEAD_UNBORN,
  GIT_UPSTREAM_GONE,
  GIT_UPSTREAM_COUNTS_VALID,
  GIT_OP_MERGE,
  GIT_OP_REBASE,
  GIT_OP_CHERRY_PICK,
  GIT_OP_REVERT,
  GIT_OP_BISECT,
  gitOidHex,
} from "@blit-sh/core";
import { diffAssignment, type DiffSide } from "@blit-sh/core/bsp";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import type { IdeSession, IdeTreeRow } from "./session";
import { isDirLike } from "./session";
import {
  addFsMoveDrag,
  fsMovePayload,
  isFsMoveDrag,
  startTileDrag,
} from "./tileDrag";

function MenuItem(props: {
  label: string;
  color?: string;
  theme: Theme;
  scale: UIScale;
  onPick: () => void;
}) {
  const [hover, setHover] = createSignal(false);
  return (
    <div
      onMouseEnter={() => setHover(true)}
      onMouseLeave={() => setHover(false)}
      // mousedown, not click: the click-away listener runs on mousedown in
      // the capture phase, and acting here keeps the two from racing.
      onMouseDown={(e) => {
        e.preventDefault();
        e.stopPropagation();
        props.onPick();
      }}
      style={{
        padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px`,
        cursor: "pointer",
        "border-radius": "2px",
        background: hover() ? props.theme.hoverBg : "transparent",
        color: props.color ?? props.theme.fg,
        "user-select": "none",
        "white-space": "nowrap",
      }}
    >
      {props.label}
    </div>
  );
}

function humanSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} K`;
  return `${(n / (1024 * 1024)).toFixed(1)} M`;
}

function shortBranch(name: string): string {
  return name.replace(/^refs\/heads\//, "");
}

const A_CODE = "A".charCodeAt(0);
const Z_CODE = "Z".charCodeAt(0);
const Q_CODE = "?".charCodeAt(0);

export function ExplorerPanel(props: {
  session: IdeSession | null;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onOpenTile: (assignment: string) => void;
  /** Rel path of the file in the focused editor/diff pane: highlighted and
   *  scrolled into view. */
  activeFile?: string | null;
  /** Rel path of the current terminal cwd directory: highlighted and revealed
   *  as the terminal `cd`s around. */
  cwd?: string | null;
}) {
  const rows = () => props.session?.tree() ?? null;

  // ── File operations (docs/design/fs-write.md `FS_OP` via the session):
  //    right-click menu → inline create/rename rows in the tree, two-step
  //    delete, and drag-onto-a-directory move. The watcher streams every
  //    result back, so the tree needs no manual refresh.
  type EditState =
    | { kind: "create-file" | "create-dir"; parentRel: string }
    | { kind: "rename"; relPath: string };
  type MenuState = {
    x: number;
    y: number;
    /** The row the menu targets; null = the tree background (root scope). */
    row: IdeTreeRow | null;
    confirmDelete: boolean;
  };
  const [editing, setEditing] = createSignal<EditState | null>(null);
  const [menu, setMenu] = createSignal<MenuState | null>(null);
  const [opError, setOpError] = createSignal<string | null>(null);
  const [dropTarget, setDropTarget] = createSignal<string | null>(null);
  let opErrTimer: ReturnType<typeof setTimeout> | undefined;
  const flashOpError = (e: unknown) => {
    console.warn("fs op failed", e);
    setOpError(e instanceof Error ? e.message : String(e));
    clearTimeout(opErrTimer);
    opErrTimer = setTimeout(() => setOpError(null), 5000);
  };
  onCleanup(() => clearTimeout(opErrTimer));

  const parentOf = (rel: string): string => {
    const i = rel.lastIndexOf("/");
    return i === -1 ? "" : rel.slice(0, i);
  };
  const joinRel = (parent: string, name: string): string =>
    parent ? `${parent}/${name}` : name;

  const openMenu = (e: MouseEvent, row: IdeTreeRow | null) => {
    if (!props.session) return;
    e.preventDefault();
    e.stopPropagation();
    setMenu({ x: e.clientX, y: e.clientY, row, confirmDelete: false });
  };
  // Click-away / Escape close the menu.
  createEffect(() => {
    if (!menu()) return;
    const away = (ev: MouseEvent) => {
      const el = ev.target as HTMLElement | null;
      if (!el?.closest?.("[data-fs-menu]")) setMenu(null);
    };
    const key = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") setMenu(null);
    };
    window.addEventListener("mousedown", away, true);
    window.addEventListener("keydown", key, true);
    onCleanup(() => {
      window.removeEventListener("mousedown", away, true);
      window.removeEventListener("keydown", key, true);
    });
  });

  const startCreate = (kind: "create-file" | "create-dir") => {
    const m = menu();
    if (!m) return;
    // Inside a directory row, next to a file row, at the root otherwise.
    const parentRel = !m.row
      ? ""
      : m.row.type === FS_ENTRY_DIR
        ? m.row.relPath
        : parentOf(m.row.relPath);
    if (m.row?.type === FS_ENTRY_DIR && !m.row.expanded)
      props.session?.toggleDir(m.row.relPath);
    setEditing({ kind, parentRel });
    setMenu(null);
  };

  const startRename = () => {
    const row = menu()?.row;
    if (!row) return;
    setEditing({ kind: "rename", relPath: row.relPath });
    setMenu(null);
  };

  const handleDelete = () => {
    const m = menu();
    const s = props.session;
    if (!m?.row || !s) return;
    if (!m.confirmDelete) {
      setMenu({ ...m, confirmDelete: true });
      return;
    }
    const rel = m.row.relPath;
    setMenu(null);
    s.removePath(rel).catch(flashOpError);
  };

  const commitEdit = (value: string) => {
    const ed = editing();
    const s = props.session;
    setEditing(null);
    // The typed value may carry slashes: create/rename resolves it against
    // the parent (missing directories are created), so "sub/x.ts" both
    // nests and moves.
    const name = value.trim().replace(/^\/+|\/+$/g, "");
    if (!ed || !s || !name) return;
    if (ed.kind === "rename") {
      const to = joinRel(parentOf(ed.relPath), name);
      if (to !== ed.relPath) s.renamePath(ed.relPath, to).catch(flashOpError);
    } else if (ed.kind === "create-dir") {
      const rel = joinRel(ed.parentRel, name);
      s.createDir(rel)
        .then(() => s.expandTo(rel))
        .catch(flashOpError);
    } else {
      const rel = joinRel(ed.parentRel, name);
      s.createFile(rel)
        .then(() => {
          // "" if the root sync reset while the create was in flight.
          const a = s.fileAssignment(rel);
          if (a) props.onOpenTile(a);
        })
        .catch(flashOpError);
    }
  };

  // Drag a row onto a directory row (or the tree background = the root) to
  // move it there; same tree only.
  const acceptMove = (e: DragEvent, dirRel: string) => {
    if (!isFsMoveDrag(e)) return;
    e.preventDefault();
    e.stopPropagation();
    setDropTarget(dirRel);
  };
  const performMove = (e: DragEvent, dirRel: string) => {
    setDropTarget(null);
    const s = props.session;
    const p = fsMovePayload(e);
    if (!s || !p) return;
    e.preventDefault();
    e.stopPropagation();
    if (
      p.connectionId !== String(s.connectionId) ||
      p.root !== (s.root() ?? "")
    )
      return;
    const to = joinRel(dirRel, p.relPath.slice(p.relPath.lastIndexOf("/") + 1));
    // No-op moves and moves into the dragged directory's own subtree.
    if (
      to === p.relPath ||
      dirRel === p.relPath ||
      dirRel.startsWith(`${p.relPath}/`)
    )
      return;
    s.renamePath(p.relPath, to).catch(flashOpError);
  };

  // An inline name-entry row (create under a directory, rename in place).
  const editRow = (depth: number, initial: string): JSX.Element => (
    <div style={{ ...rowBase(depth), cursor: "default" }}>
      <span style={{ width: "10px", "flex-shrink": 0 }} />
      <input
        ref={(el) =>
          setTimeout(() => {
            el.focus();
            el.select();
          })
        }
        value={initial}
        spellcheck={false}
        style={{
          flex: "1 1 0",
          "min-width": 0,
          background: props.theme.hoverBg,
          color: props.theme.fg,
          border: `1px solid ${props.theme.accent}`,
          "border-radius": "2px",
          outline: "none",
          padding: "0 4px",
          "font-family": props.fontFamily,
          "font-size": `${props.scale.sm}px`,
        }}
        onKeyDown={(ev) => {
          ev.stopPropagation();
          if (ev.key === "Enter") commitEdit(ev.currentTarget.value);
          else if (ev.key === "Escape") setEditing(null);
        }}
        onBlur={() => setEditing(null)}
      />
    </div>
  );
  const isRenaming = (row: IdeTreeRow): boolean => {
    const ed = editing();
    return ed?.kind === "rename" && ed.relPath === row.relPath;
  };
  const createUnder = (row: IdeTreeRow): boolean => {
    const ed = editing();
    return (
      ed != null &&
      ed.kind !== "rename" &&
      row.type === FS_ENTRY_DIR &&
      ed.parentRel === row.relPath
    );
  };
  const createAtRoot = (): boolean => {
    const ed = editing();
    return ed != null && ed.kind !== "rename" && ed.parentRel === "";
  };

  // Follow the terminal cwd: expand its ancestors so a `cd` reveals the
  // directory. Deliberately does NOT scroll — the tree stays put. The focused
  // file is only *highlighted* (below), never scrolled/expanded to, so clicking
  // a change or jumping to a definition doesn't yank the tree around.
  createEffect(() => {
    const dir = props.cwd;
    if (dir) props.session?.expandTo(dir);
  });

  // Branch + upstream, lifted from the old Changes panel: the tree carries the
  // per-file status, so the only thing left worth its own line is the branch.
  const branch = createMemo(() => {
    const head = props.session?.gitState()?.head;
    if (!head) return null;
    if (head.flags & GIT_HEAD_UNBORN) return shortBranch(head.name) || "unborn";
    if (head.flags & GIT_HEAD_DETACHED)
      return gitOidHex(head.oid, props.session?.gitHandle()?.oidFormat).slice(
        0,
        8,
      );
    return shortBranch(head.name);
  });
  const upstream = createMemo(() => {
    const s = props.session?.gitState();
    const head = s?.head;
    if (!s || !head) return null;
    return (
      s.upstreams.get(head.name) ?? s.upstreams.get(shortBranch(head.name))
    );
  });
  // The in-progress operation, as a chip label: "merging",
  // "rebasing 3/7" (detail = step/total, docs/design/git.md OP record).
  const OP_LABELS: Record<number, string> = {
    [GIT_OP_MERGE]: "merging",
    [GIT_OP_REBASE]: "rebasing",
    [GIT_OP_CHERRY_PICK]: "cherry-picking",
    [GIT_OP_REVERT]: "reverting",
    [GIT_OP_BISECT]: "bisecting",
  };
  const opLabel = createMemo(() => {
    const op = props.session?.gitState()?.op;
    if (!op) return null;
    const label = OP_LABELS[op.op] ?? "in progress";
    return op.detail ? `${label} ${op.detail}` : label;
  });

  // The foldable "Changes" summary mirrors `git status --short`: a two-column
  // XY code per row (X = index/staged, Y = worktree/unstaged, space = clean in
  // that column), one row per porcelain entry. A tracked file deleted from the
  // index while an untracked file of the same name exists shows as TWO rows,
  // exactly like git — the staged deletion "D " and the untracked "??".
  type Change = {
    path: string;
    oldPath: string;
    x: string; // staged column (a letter, "?", or " ")
    y: string; // worktree column
    side: DiffSide;
  };
  const isLetterCode = (c: number) => c >= A_CODE && c <= Z_CODE;
  const col = (c: number): string =>
    isLetterCode(c) || c === Q_CODE ? String.fromCharCode(c) : " ";
  // The state mirror pushes on every settle, including ones that didn't
  // touch the status list (a ref moved, a stash landed). Both the Changes
  // list and the tree badges derive from status only, so they rebuild
  // behind this fingerprint and keep their identities otherwise — the same
  // pattern as the session's opRefTips.
  const statusKey = createMemo(() => {
    const gs = props.session?.gitState();
    if (!gs) return "";
    let out = "";
    for (const e of gs.status)
      out += `${e.staged} ${e.unstaged} ${e.flags} ${e.oldPath} ${e.path}\n`;
    return out;
  });
  // Reused per (path, side, fields) across rebuilds, so `<For>` keeps the
  // DOM of rows whose entry didn't change.
  let changeCache = new Map<string, Change>();
  const changes = createMemo<Change[]>(() => {
    statusKey();
    const gs = untrack(() => props.session?.gitState());
    if (!gs) {
      changeCache = new Map();
      return [];
    }
    const nextCache = new Map<string, Change>();
    const out: Change[] = [];
    const push = (c: Change) => {
      const key = `${c.path} ${c.side} ${c.x}${c.y} ${c.oldPath}`;
      const row = changeCache.get(key) ?? c;
      nextCache.set(key, row);
      out.push(row);
    };
    for (const e of gs.status) {
      if ((e.flags & GIT_STATUS_ENTRY_CONFLICTED) !== 0) {
        push({
          path: e.path,
          oldPath: e.oldPath,
          x: "U",
          y: "U",
          side: "unstaged",
        });
        continue;
      }
      // Deleted from the index while an untracked file of the same name exists:
      // git emits two rows (the index deletion + the untracked add).
      if (isLetterCode(e.staged) && e.unstaged === Q_CODE) {
        push({
          path: e.path,
          oldPath: e.oldPath,
          x: col(e.staged),
          y: " ",
          side: "staged",
        });
        push({
          path: e.path,
          oldPath: e.oldPath,
          x: "?",
          y: "?",
          side: "untracked",
        });
        continue;
      }
      const x = col(e.staged);
      const y = col(e.unstaged);
      if (x === " " && y === " ") continue;
      const side: DiffSide =
        e.unstaged === Q_CODE
          ? "untracked"
          : isLetterCode(e.unstaged)
            ? "unstaged"
            : "staged";
      push({ path: e.path, oldPath: e.oldPath, x, y, side });
    }
    changeCache = nextCache;
    return out;
  });
  const [changesOpen, setChangesOpen] = createSignal(true);

  // The diff-tile assignment for a change (workdir-relative path resolved
  // against the git workdir, which may differ from the fs root).
  function changeAssignment(c: Change): string | null {
    const s = props.session;
    if (!s) return null;
    const wd = s.gitHandle()?.workdir ?? "";
    const abs = wd ? `${wd}/${c.path}` : c.path;
    return diffAssignment(s.connectionId, abs, c.side);
  }
  function openChange(c: Change) {
    const a = changeAssignment(c);
    if (a) props.onOpenTile(a);
  }

  // Fold the git status into the tree: a per-file code, and a per-directory
  // roll-up of the status letters of everything beneath it. Keyed by absolute
  // path so it works even when the fs root and git workdir differ. Gated on
  // the status fingerprint (+ workdir): a push that didn't change the status
  // keeps the maps' identity, so no tree-row badge re-evaluates.
  const buildGitStatus = () => {
    const s = props.session;
    const gs = s?.gitState();
    const wd = s?.gitHandle()?.workdir ?? "";
    // Per file: the two porcelain columns {x: staged, y: worktree}.
    const fileMap = new Map<string, { x: string; y: string }>();
    const dirLetters = new Map<string, Set<string>>();
    if (!gs || !wd) return { fileMap, dirLetters };
    for (const e of gs.status) {
      const abs = `${wd}/${e.path}`;
      const conflicted = (e.flags & GIT_STATUS_ENTRY_CONFLICTED) !== 0;
      const x = conflicted ? "U" : col(e.staged);
      const y = conflicted ? "U" : col(e.unstaged);
      if (x === " " && y === " ") continue;
      fileMap.set(abs, { x, y });
      // Roll each non-blank letter up to every ancestor directory.
      const letters = `${x}${y}`.replace(/ /g, "");
      let dir = abs;
      for (;;) {
        const i = dir.lastIndexOf("/");
        if (i <= 0) break;
        dir = dir.slice(0, i);
        if (dir !== wd && !dir.startsWith(`${wd}/`)) break;
        let set = dirLetters.get(dir);
        if (!set) {
          set = new Set();
          dirLetters.set(dir, set);
        }
        for (const l of letters) set.add(l);
        if (dir === wd) break;
      }
    }
    return { fileMap, dirLetters };
  };
  const gitStatusKey = createMemo(() => {
    const wd = props.session?.gitHandle()?.workdir ?? "";
    return wd ? `${wd}\n${statusKey()}` : "";
  });
  const gitStatus = createMemo(() => {
    gitStatusKey();
    return untrack(buildGitStatus);
  });

  // Each status letter gets its own colour, so a directory summary like "AD?"
  // reads as add(green) delete(red) untracked(yellow) at a glance.
  const letterColor = (ch: string): string => {
    switch (ch) {
      case "A":
        return props.theme.success;
      case "D":
      case "U":
        return props.theme.error;
      case "?":
        return props.theme.warning;
      default:
        return props.theme.accent; // M, R, C, T, …
    }
  };

  type Badge = {
    /** Per-letter coloured segments. */
    segments: { ch: string; color: string }[];
    /** The raw letters, for the diff-side decision on click. */
    letters: string;
  };
  const badgeFor = (letters: string): Badge => ({
    letters,
    segments: [...letters].map((ch) => ({ ch, color: letterColor(ch) })),
  });

  // A blank porcelain column renders as a dim dot, not a space: with only
  // one column set (e.g. "M "), a space leaves the two columns visually
  // indistinguishable — staged "M " and worktree " M" would both read as a
  // lone M.
  const statusCell = (ch: string): { ch: string; color: string } =>
    ch === " "
      ? { ch: "·", color: props.theme.dimFg }
      : { ch, color: letterColor(ch) };

  const absOf = (row: IdeTreeRow): string => {
    const root = props.session?.root() ?? "";
    return root ? `${root}/${row.relPath}` : row.relPath;
  };

  // A collapsed directory's rolled-up status summary (hidden once expanded,
  // where the child rows carry their own flags).
  const dirSummary = (row: IdeTreeRow): Badge | null => {
    if (!isDirLike(row.type, row.flags) || row.expanded) return null;
    const set = gitStatus().dirLetters.get(absOf(row));
    if (!set || set.size === 0) return null;
    const order = ["U", "D", "A", "R", "C", "M", "?"];
    const letters = [...set]
      .sort((a, b) => order.indexOf(a) - order.indexOf(b))
      .join("");
    return badgeFor(letters);
  };

  // A file's two porcelain columns {x: staged, y: worktree}, or null if clean.
  const fileFlags = (row: IdeTreeRow): { x: string; y: string } | null => {
    if (isDirLike(row.type, row.flags)) return null;
    return gitStatus().fileMap.get(absOf(row)) ?? null;
  };

  const rowBase = (depth: number): JSX.CSSProperties => ({
    display: "flex",
    "align-items": "center",
    gap: `${props.scale.tightGap}px`,
    height: `${Math.round(props.fontSize * 1.5)}px`,
    padding: `0 ${props.scale.tightGap}px 0 ${props.scale.panelPadding + depth * 12}px`,
    "white-space": "nowrap",
    cursor: "pointer",
    "font-family": props.fontFamily,
    // Filenames are content, not chrome: they render at the configured
    // font size like the editor next to them. The badges hung off the row
    // (git status, size, change counts) stay smaller.
    "font-size": `${props.scale.md}px`,
    color: props.theme.fg,
  });

  // The branch line: ⎇ name, then ahead/behind (or "gone") on the right.
  const branchHeader = (): JSX.Element => (
    <Show when={branch()}>
      <div
        style={{
          "flex-shrink": 0,
          display: "flex",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px`,
          "font-family": props.fontFamily,
          "font-size": `${props.scale.sm}px`,
          "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        }}
      >
        <span style={{ color: props.theme.dimFg }}>{"⎇"}</span>
        <b
          style={{
            color: props.theme.fg,
            overflow: "hidden",
            "text-overflow": "ellipsis",
          }}
        >
          {branch()}
        </b>
        <Show when={opLabel()}>
          <span
            style={{
              "flex-shrink": 0,
              padding: "0 5px",
              "border-radius": "3px",
              "font-size": `${props.scale.xs}px`,
              color: props.theme.error,
              border: `1px solid color-mix(in srgb, ${props.theme.error} 45%, transparent)`,
              background: `color-mix(in srgb, ${props.theme.error} 14%, transparent)`,
              "font-variant-numeric": "tabular-nums",
            }}
          >
            {opLabel()}
          </span>
        </Show>
        <Show when={upstream()}>
          {(u) => (
            <span
              style={{
                "margin-left": "auto",
                display: "flex",
                gap: "6px",
                "flex-shrink": 0,
                "font-variant-numeric": "tabular-nums",
              }}
            >
              <Show
                when={u().flags & GIT_UPSTREAM_GONE}
                fallback={
                  <>
                    <span
                      style={{
                        color: props.theme.success,
                        opacity:
                          u().flags & GIT_UPSTREAM_COUNTS_VALID ? 1 : 0.5,
                      }}
                    >
                      {"↑"}
                      {u().ahead}
                    </span>
                    <span
                      style={{
                        color: props.theme.accent,
                        opacity:
                          u().flags & GIT_UPSTREAM_COUNTS_VALID ? 1 : 0.5,
                      }}
                    >
                      {"↓"}
                      {u().behind}
                    </span>
                  </>
                }
              >
                <span style={{ color: props.theme.warning }}>gone</span>
              </Show>
            </span>
          )}
        </Show>
      </div>
    </Show>
  );

  // A foldable summary of modified files at the top of the tree. It takes its
  // natural height and scrolls together with the file list below it (one shared
  // scroll), rather than owning a scroll of its own. The tree also carries this
  // status inline; this is the one-glance list + quick jump into each diff.
  const changesSection = (): JSX.Element => (
    <Show when={changes().length > 0}>
      <div
        style={{
          "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        }}
      >
        <div
          onClick={() => setChangesOpen((o) => !o)}
          style={{
            display: "flex",
            "align-items": "center",
            gap: `${props.scale.tightGap}px`,
            padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px`,
            cursor: "pointer",
            "font-size": `${props.scale.xs}px`,
            "text-transform": "uppercase",
            "letter-spacing": "0.6px",
            "font-weight": 700,
            color: props.theme.dimFg,
            "user-select": "none",
          }}
          title={changesOpen() ? "Collapse changes" : "Expand changes"}
        >
          <span style={{ width: "10px", "text-align": "center" }}>
            {changesOpen() ? "▾" : "▸"}
          </span>
          <span>Changes</span>
          <span style={{ "margin-left": "auto", "font-weight": 400 }}>
            {changes().length}
          </span>
        </div>
        <Show when={changesOpen()}>
          <div>
            <For each={changes()}>
              {(c) => (
                <div
                  style={{
                    display: "flex",
                    "align-items": "center",
                    gap: `${props.scale.tightGap}px`,
                    height: `${Math.round(props.fontSize * 1.5)}px`,
                    padding: `0 ${props.scale.panelPadding}px`,
                    "white-space": "nowrap",
                    cursor: "pointer",
                    "font-family": props.fontFamily,
                    "font-size": `${props.scale.sm}px`,
                    color: props.theme.fg,
                  }}
                  onClick={() => openChange(c)}
                  draggable={true}
                  onDragStart={(e) => {
                    const a = changeAssignment(c);
                    if (a) startTileDrag(e, a);
                  }}
                  title={c.oldPath ? `${c.oldPath} → ${c.path}` : c.path}
                >
                  <span
                    style={{
                      "flex-shrink": 0,
                      "font-weight": 700,
                      "font-size": `${props.scale.sm}px`,
                      "white-space": "pre",
                      "font-family": props.fontFamily,
                    }}
                  >
                    <span style={{ color: statusCell(c.x).color }}>
                      {statusCell(c.x).ch}
                    </span>
                    <span style={{ color: statusCell(c.y).color }}>
                      {statusCell(c.y).ch}
                    </span>
                  </span>
                  <span
                    style={{ overflow: "hidden", "text-overflow": "ellipsis" }}
                  >
                    {c.path}
                  </span>
                </div>
              )}
            </For>
          </div>
        </Show>
      </div>
    </Show>
  );

  return (
    <div
      style={{
        flex: "1 1 0",
        "min-height": 0,
        display: "flex",
        "flex-direction": "column",
      }}
    >
      {branchHeader()}
      <div
        style={{
          flex: "1 1 0",
          "min-height": 0,
          "overflow-y": "auto",
          "overflow-x": "hidden",
          ...scrollbarStyle(props.theme),
        }}
        onContextMenu={(e) => openMenu(e, null)}
        onDragOver={(e) => acceptMove(e, "")}
        onDrop={(e) => performMove(e, "")}
      >
        {changesSection()}
        <Show when={opError()}>
          <div
            style={{
              padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px`,
              "font-size": `${props.scale.sm}px`,
              "font-family": props.fontFamily,
              color: props.theme.errorText,
            }}
          >
            {opError()}
          </div>
        </Show>
        <Show
          when={!props.session?.fsError()}
          fallback={
            <div
              style={{
                padding: `${props.scale.panelPadding}px`,
                "font-size": `${props.scale.sm}px`,
                color: props.theme.errorText,
              }}
            >
              {props.session?.fsError()}
            </div>
          }
        >
          <Show
            when={rows()}
            fallback={
              <div
                style={{
                  padding: `${props.scale.panelPadding}px`,
                  "font-size": `${props.scale.sm}px`,
                  color: props.theme.dimFg,
                }}
              >
                {props.session ? "Opening…" : "No root selected."}
              </div>
            }
          >
            {(list) => (
              <Show
                when={list().length > 0}
                fallback={
                  <div
                    style={{
                      padding: `${props.scale.panelPadding}px`,
                      "font-size": `${props.scale.sm}px`,
                      color: props.theme.dimFg,
                    }}
                  >
                    {props.session?.treePhase() === "live"
                      ? "Empty directory."
                      : "Loading…"}
                  </div>
                }
              >
                <div style={{ padding: `${props.scale.tightGap}px 0` }}>
                  <Show when={createAtRoot()}>{editRow(0, "")}</Show>
                  <For each={list()}>
                    {(row) => {
                      const isDir = isDirLike(row.type, row.flags);
                      // Expansion follows symlinks; moving into one does not.
                      // A drop on a symlinked directory would rename *through*
                      // the link — the destination resolves under both the link
                      // and its real path, and fails EXDEV across devices. That
                      // is its own change; keep it to real directories, which is
                      // also what `createUnder` gates on.
                      const isMoveTarget = row.type === FS_ENTRY_DIR;
                      const isLink = row.type === FS_ENTRY_SYMLINK;
                      const unreadable =
                        (row.flags & FS_ENTRY_UNREADABLE) !== 0;
                      const unstable = (row.flags & FS_ENTRY_UNSTABLE) !== 0;
                      // The file open in the focused pane, and the terminal cwd,
                      // get a standing highlight.
                      const isActive = () =>
                        !isDir && props.activeFile === row.relPath;
                      const isCwd = () => isDir && props.cwd === row.relPath;
                      return (
                        <>
                          <Show
                            when={!isRenaming(row)}
                            fallback={editRow(row.depth, row.name)}
                          >
                            <div
                              style={{
                                ...rowBase(row.depth),
                                opacity: unreadable ? 0.5 : 1,
                                "font-style": isLink ? "italic" : "normal",
                                background:
                                  dropTarget() === row.relPath && isMoveTarget
                                    ? `color-mix(in srgb, ${props.theme.accent} 30%, transparent)`
                                    : isActive()
                                      ? `color-mix(in srgb, ${props.theme.accent} 22%, transparent)`
                                      : isCwd()
                                        ? props.theme.hoverBg
                                        : "transparent",
                                "box-shadow": isActive()
                                  ? `inset 2px 0 0 ${props.theme.accent}`
                                  : "none",
                              }}
                              onClick={() => {
                                const s = props.session;
                                if (!s) return;
                                if (isDir) s.toggleDir(row.relPath);
                                // A symlink to a file opens like the file it
                                // points at: the editor's single-file sync
                                // canonicalizes the root, so it loads the
                                // target's real bytes.
                                else if (
                                  row.type === FS_ENTRY_FILE ||
                                  row.type === FS_ENTRY_SYMLINK
                                ) {
                                  // Empty when the session has no synced root
                                  // yet; opening then would mint a tile with no
                                  // path that can never resolve.
                                  const a = s.fileAssignment(row.relPath);
                                  if (a) props.onOpenTile(a);
                                }
                              }}
                              onContextMenu={(e) => openMenu(e, row)}
                              draggable={
                                row.type === FS_ENTRY_FILE || isMoveTarget
                              }
                              onDragStart={(e) => {
                                const s = props.session;
                                if (!s) return;
                                // Files drag as tiles (droppable on panes) AND
                                // as move payloads (droppable on directories);
                                // directories move-drag only.
                                if (row.type === FS_ENTRY_FILE)
                                  startTileDrag(
                                    e,
                                    s.fileAssignment(row.relPath),
                                  );
                                addFsMoveDrag(e, {
                                  connectionId: String(s.connectionId),
                                  root: s.root() ?? "",
                                  relPath: row.relPath,
                                });
                              }}
                              onDragOver={(e) => {
                                if (isMoveTarget) acceptMove(e, row.relPath);
                              }}
                              onDragLeave={() => {
                                if (
                                  isMoveTarget &&
                                  dropTarget() === row.relPath
                                )
                                  setDropTarget(null);
                              }}
                              onDrop={(e) => {
                                if (isMoveTarget) performMove(e, row.relPath);
                              }}
                              title={
                                unreadable
                                  ? `${row.relPath} (unreadable)`
                                  : row.relPath
                              }
                            >
                              <span
                                style={{
                                  width: "10px",
                                  "flex-shrink": 0,
                                  color: props.theme.dimFg,
                                  "text-align": "center",
                                }}
                              >
                                {isDir ? (row.expanded ? "▾" : "▸") : ""}
                              </span>
                              <span style={{ "flex-shrink": 0, opacity: 0.85 }}>
                                {isDir
                                  ? "📁"
                                  : isLink
                                    ? "↪"
                                    : unreadable
                                      ? "🔒"
                                      : "📄"}
                              </span>
                              <span
                                style={{
                                  overflow: "hidden",
                                  "text-overflow": "ellipsis",
                                }}
                              >
                                {row.name}
                              </span>
                              <Show when={unstable}>
                                <span
                                  style={{
                                    color: props.theme.warning,
                                    "font-size": "0.85em",
                                  }}
                                  title="being written…"
                                >
                                  ●
                                </span>
                              </Show>
                              {/* Right side: for a directory, the collapsed roll-up
                              summary; for a file, its staged/unstaged git flags
                              (git porcelain XY) AND its size, side by side. */}
                              <span
                                style={{
                                  "margin-left": "auto",
                                  display: "flex",
                                  "align-items": "center",
                                  gap: `${props.scale.tightGap}px`,
                                  "flex-shrink": 0,
                                }}
                              >
                                <Show when={!isDir}>
                                  <Show when={fileFlags(row)}>
                                    {(f) => (
                                      <span
                                        style={{
                                          "white-space": "pre",
                                          "font-weight": 700,
                                          "font-size": `${props.scale.sm}px`,
                                          "font-family": props.fontFamily,
                                          cursor: "pointer",
                                        }}
                                        title="Open diff (git status)"
                                        onClick={(ev) => {
                                          ev.stopPropagation();
                                          const s = props.session;
                                          if (!s) return;
                                          const side: DiffSide =
                                            f().y === "?"
                                              ? "untracked"
                                              : f().y !== " "
                                                ? "unstaged"
                                                : "staged";
                                          const a = s.diffAssignment(
                                            row.relPath,
                                            side,
                                          );
                                          if (a) props.onOpenTile(a);
                                        }}
                                      >
                                        <span
                                          style={{
                                            color: statusCell(f().x).color,
                                          }}
                                        >
                                          {statusCell(f().x).ch}
                                        </span>
                                        <span
                                          style={{
                                            color: statusCell(f().y).color,
                                          }}
                                        >
                                          {statusCell(f().y).ch}
                                        </span>
                                      </span>
                                    )}
                                  </Show>
                                  <Show when={!isLink}>
                                    <span
                                      style={{
                                        color: props.theme.dimFg,
                                        "font-size": `${props.scale.xs}px`,
                                        "font-variant-numeric": "tabular-nums",
                                      }}
                                    >
                                      {humanSize(row.size)}
                                    </span>
                                  </Show>
                                </Show>
                                <Show when={dirSummary(row)}>
                                  {(b) => (
                                    <span
                                      style={{
                                        "font-weight": 700,
                                        "font-size": `${props.scale.xs}px`,
                                        "letter-spacing": "0.5px",
                                      }}
                                      title="changes below"
                                    >
                                      <For each={b().segments}>
                                        {(seg) => (
                                          <span style={{ color: seg.color }}>
                                            {seg.ch}
                                          </span>
                                        )}
                                      </For>
                                    </span>
                                  )}
                                </Show>
                              </span>
                            </div>
                          </Show>
                          <Show when={createUnder(row)}>
                            {editRow(row.depth + 1, "")}
                          </Show>
                        </>
                      );
                    }}
                  </For>
                </div>
              </Show>
            )}
          </Show>
        </Show>
      </div>
      <Show when={menu()}>
        {(m) => (
          <div
            data-fs-menu
            style={{
              position: "fixed",
              left: `${m().x}px`,
              top: `${m().y}px`,
              "z-index": 1000,
              background: props.theme.solidPanelBg,
              border: `1px solid ${props.theme.border}`,
              "border-radius": "3px",
              padding: "2px",
              "min-width": "150px",
              "font-family": props.fontFamily,
              "font-size": `${props.scale.sm}px`,
              "box-shadow": "0 4px 16px rgba(0, 0, 0, 0.4)",
            }}
          >
            <MenuItem
              label="New file…"
              theme={props.theme}
              scale={props.scale}
              onPick={() => startCreate("create-file")}
            />
            <MenuItem
              label="New folder…"
              theme={props.theme}
              scale={props.scale}
              onPick={() => startCreate("create-dir")}
            />
            <Show when={m().row}>
              <MenuItem
                label="Rename / move…"
                theme={props.theme}
                scale={props.scale}
                onPick={startRename}
              />
              <MenuItem
                label={m().confirmDelete ? "Really delete? ⏎" : "Delete"}
                color={m().confirmDelete ? props.theme.errorText : undefined}
                theme={props.theme}
                scale={props.scale}
                onPick={handleDelete}
              />
            </Show>
          </div>
        )}
      </Show>
    </div>
  );
}
