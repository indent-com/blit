# IDE surfaces for blit — a design exploration

- **Status:** Draft / exploration (not an accepted spec)
- **Date:** 2026-07-23
- **Builds on:** [fs-watch.md](design/fs-watch.md), [git.md](design/git.md),
  [lsp.md](design/lsp.md), [fs-write.md](design/fs-write.md),
  [frontend.md](frontend.md)

## Summary

The fs branch adds three "family" protocols — filesystem sync, git
introspection, and language intelligence — each with a **complete,
headless browser core API and zero UI**. `js/core/src/{fs,git,lsp}.ts`
expose live mirrors and typed pull methods; nothing in `js/ui/src`
consumes them yet. The entire gap between "blit has these features" and
"a person sees them" is frontend.

Two facts shape every option below:

- **git and LSP are read-only by construction; fs sync is not.** The
  fs family already ships a full mutation surface — `writeFile`,
  `mkdir`, `remove`, `rename`, `symlink`, `hardlink` over
  `C2S_FS_WRITE`/`C2S_FS_OP`, CAS-guarded on a content hash
  ([fs-write.md](design/fs-write.md)) — gated only by a server-side
  `BLIT_FS_WRITE` deployment flag. An editor can **save today**, with no
  new backend work. Only staging/commit (git) and applyEdit/format
  (LSP) wait on a future mutation RFC.
- **blit is a tiling terminal WM whose entire chrome is derived from
  the active terminal ANSI palette** (`themeFromPalette` in
  `js/ui/src/theme.ts`). The design question is therefore not _which_
  IDE features to add but _how much IDE, expressed how_, without the app
  ceasing to be blit.

This doc inventories what the current UI gives us to build on, lists
what the new core APIs already make reachable, then lays out three
integration directions with honest trade-offs, a separable
editor-engine decision, a per-family checklist of the capabilities
worth honoring, an ordered rollout that **delivers value before any
editor exists**, and a resolved stance on every hard UX question the
design raises.

The recommendation, stated up front: go **terminal-first** — ambient
decorations plus summonable overlays plus _optional_ editor tiles —
because it is the smallest footprint, the truest to blit's identity, and
the direction whose first three increments ship without an editor at
all. The alternatives are real and are argued fairly; pick a different
one deliberately, not by default.

## What we're building on (current UI)

Everything here is verified against the fs branch. The web UI is one
SolidJS tree rooted at `WorkspaceScreen` (`js/ui/src/Workspace.tsx`,
~2470 lines), which owns essentially all state.

| Surface           | Where                                       | What it is                                                                                                                                                                  | The seam for new UI                                                                                                                                     |
| ----------------- | ------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **BSP tiling**    | `js/ui/src/bsp/BSPContainer.tsx` `LeafPane` | A pane's content is chosen purely by the _shape_ of its assignment string: plain `SessionId` → `BlitTerminal`, `surface:<conn>:<id>` → `BlitSurfaceView`, else `EmptyPane`. | A new `<Show>` branch in `LeafPane` keyed on a new assignment kind. This is _the_ place new pane types land.                                            |
| **Overlay stack** | `Workspace.tsx` `overlay()` signal (~L329)  | One string-union signal (`expose`/`palette`/`font`/`help`/`remotes`/`media`/`null`), each mounted as a sibling `<Show when={overlay()==="…"}>`.                             | Extend the union, add a `<Show>` block built from `Overlay.tsx`'s `OverlayBackdrop`/`OverlayPanel`/`OverlayHeader` (inherits palette tinting for free). |
| **Right panel**   | `Workspace.tsx` `PreviewPanel` (~L2018)     | A resizable right dock of off-screen session/surface thumbnails, toggled by `previewPanelOpen()`. There is **no left panel**.                                               | Template for any dockable side panel; or generalize its resize-handle shell into a reusable `Dock`.                                                     |
| **Status bar**    | `js/ui/src/StatusBar.tsx` (~30 flat props)  | Footer segments + action buttons; already buckets connections by status via a `ConnectionDot` pattern.                                                                      | A new segment is a new prop wired at the `Workspace.tsx` call site (~L1946) + an inline block.                                                          |
| **Keyboard**      | `js/ui/src/createKeyboardShortcuts.ts`      | Global **capture-phase** keydown handlers routing to Workspace actions (`toggleOverlay`, `createInPane`, …).                                                                | A which-key leader chord state machine slots in here; every binding must also register in `HelpOverlay` + `i18n.ts`.                                    |
| **Theme**         | `js/ui/src/theme.ts` `themeFromPalette`     | Every chrome token (bg/fg/accent/error/success/warning) is derived from `palette.bg/fg/ansi[]`.                                                                             | New chrome reads `theme()` and is palette-tinted automatically; the CM6 theme is generated from it.                                                     |
| **Multi-host**    | `BlitWorkspace` holds many `BlitConnection` | Session ids are minted `${conn}:${n}`, so every pane is host-prefixed; panes from several hosts coexist.                                                                    | Any repo/LSP handle is per-connection; a pane looks one up by its `connectionId` and prefixes results by host.                                          |

Two subtleties that will bite anyone who skips them:

- **The BSP assignment grammar has two encodings.** The URL-hash wire
  form uses `t:`/`s:` prefixes (`bsp/layout.ts` `loadAssignmentsFromHash`);
  the resolved in-memory value uses `surface:<conn>:<id>` for surfaces
  (`js/core/src/bsp/layout.ts` `surfaceAssignment`) and a **raw session-id
  string** for terminals. A new content kind must teach _both_ encodings
  plus `LeafPane`'s render dispatch.
- **`reconcileAssignments` (`js/core/src/bsp/layout.ts`) early-continues
  only for surfaces.** Every other assignment value falls into
  session-liveness logic and gets **cleared on the next
  session/connection churn**. A new kind needs its own `continue` branch
  with a validity rule, and the carry-forward collector in
  `BSPContainer.tsx` needs the same — this is the #1 "my pane vanished"
  bug for any new tile type.

## What's already reachable (zero backend work)

The core handles are done and headless. A UI opens them and reads live
maps; `BlitConnection` decodes, applies, and auto-acks every push.

| Handle                                                   | Live (pushed) mirror                                                                                      | Pull methods                                                                          | Rendered today with no server change                                                                     |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| `workspace.syncFs(conn, path, opts)` → `FsSyncHandle`    | `handle.live: ReadonlyMap<path, FsNode>` (flat `/`-keyed tree + content)                                  | `fetch(path)`; **writes**: `writeFile`/`mkdir`/`remove`/`rename`/`symlink`/`hardlink` | A live file tree; an editor buffer source; New File / rename / delete in an explorer.                    |
| `workspace.openRepo(conn, path, opts)` → `GitRepoHandle` | `state: GitStateMirror` (`head`, `refs`, `op`, `status[]`, `upstreams`, `stashes[]`, `flags`)             | `log` `tree` `blob` `diff` `patch` `index` `mergeBase` `resolve` `watchLog`           | Branch chip `main ↑2 ↓3`, in-progress-op banner, stash badge; a diff viewer from aligned `patch()` rows. |
| `connection.openLsp(path, opts)` → `LspHandle`           | `state: LspStateMirror` (per-backend phase/progress/caps/rss/msg), `diags: LspDiagMirror` (per-file sets) | `definition` `references` `hover` `documentSymbols` `workspaceSymbols` `rename`       | Diagnostics gutter/list, warmup/RSS chip, go-to-def/refs jumps, hover cards, outline, symbol finder.     |

The load-bearing details:

- **The mirrors are plain `Map`s/classes, not reactive.** A SolidJS UI
  must bridge every `onState`/`onDiagnostics`/`onUpdate`/`onRecord`
  callback into its own signal or store; a naive `createMemo` over
  `handle.live` will not re-run. Build one small "bump a signal in the
  callback" helper and reuse it everywhere.
- **The pushed git chip is _mostly_ free, not entirely.** `head`/`refs`
  and `UPSTREAM` ahead/behind are cheap pushed state (open with
  `{watch, tracking}`). But the dirty-file count needs the `status` open
  flag, which walks the worktree on a separate ~500 ms settle window
  (`BLIT_GIT_STATUS_LATENCY_MS`) — that is why git split it out. Ship
  refs + ahead/behind as the near-free first win; treat the dirty count
  as an opt-in that costs a scan.
- **`openLsp` lives only on `BlitConnection`**, not `BlitWorkspace`
  (unlike `syncFs`/`openRepo`). A workspace-level wrapper is a small,
  worthwhile addition.
- **Positions are 0-based line + UTF-8 _byte_ column** in every LSP
  message; a browser text surface indexes UTF-16 code units. There is
  no conversion helper in core — the UI transcodes at the edge, both
  directions.
- **Feature bits gate availability.** fs/git/LSP ride
  `FEATURE_FS`/`FEATURE_GIT`/`FEATURE_LSP` HELLO bits; surface each
  affordance only when the focused pane's connection advertises it, and
  degrade silently otherwise.

## Directions

Three coherent ways to spend the integration budget. They are not
mutually exclusive — A and B in particular compose — but they imply
different centers of gravity and different risks to blit's identity.

| Axis                    | A · Terminal-first          | B · BSP-native panes                   | C · Docks + Monaco                             |
| ----------------------- | --------------------------- | -------------------------------------- | ---------------------------------------------- |
| New persistent chrome   | A few status segments only  | New tile kinds (no new panel system)   | Left explorer + bottom problems + editor group |
| How content is summoned | Overlays + which-key leader | Tiled panes via the assignment grammar | Always-docked panels                           |
| Editor                  | Optional, opened on demand  | A first-class `editor:` tile           | Monaco, central and load-bearing               |
| Identity impact         | Preserves it                | Extends it ("windows can be code")     | Erodes it ("blit with an IDE bolted on")       |
| Bundle / theming risk   | Low                         | Low (until the editor engine)          | High (Monaco vs single-file + palette theming) |
| First useful ship       | A status chip, days         | A read-only `filetree:` tile           | An explorer dock over `syncFs`                 |
| Effort ceiling          | The optional editor         | The editor engine + reconcile plumbing | Monaco integration + a second theming system   |

### A. Terminal-first: decorations + summonable overlays (recommended)

**Thesis.** The tiling cell grid stays the app; fs/git/LSP are
_projected onto it_. Nothing new is persistently visible except a few
characters in the `StatusBar`; everything else is summoned, does its
job, and dismisses. This is the grain of the mock that read best, and
the cheapest real integration.

**What the user sees.** Three always-on `StatusBar` segments, each a new
prop fed by a signal bumped inside a mirror callback: a git chip
(`main ↑2 ↓3 ●5`, flipping to a merge/rebase banner from `state.op`, a
stash badge from `state.stashes`), an LSP health chip
(`rust-analyzer indexing 42%` from `state.servers`, grayed by `caps`),
and an fs-sync dot (live/settling/closed). Then a small set of overlays,
each built from the shared `Overlay.tsx` chrome:

- **git** — models `RemotesOverlay`'s table + status dots; a file opens
  a diff rendered straight from `patch()` aligned `PATCH_ROW` rows
  (intraline `oldSpans`/`newSpans` painted as highlights — no diff
  parser). Standard endpoints: `INDEX×WORKTREE`, `COMMIT(HEAD)×INDEX`,
  `MERGE_BASE(upstream)×topic` for PR review.
- **diagnostics** — a problems list over `diags.files`; Enter jumps to
  the range.
- **telescope finder** — the highest-leverage move is to _extend the
  existing `SwitcherOverlay`_, whose `query → sections → flatItems →
selectedIdx` skeleton already is a fuzzy finder. Add item kinds fed by
  `handle.live` keys (files), `workspaceSymbols(query)` (symbols), and
  `watchLog` (commits), with a leading sigil selecting mode. One finder,
  one muscle memory.
- **which-key leader** — a chord state machine in
  `createKeyboardShortcuts.ts` opens a cheat-sheet overlay routing
  `g s`→git, `g d`→diff, `d`→diagnostics, `f`→files, `s`→symbols,
  `K`→hover, `g d`(on symbol)→definition.

**The editor is opt-in**: only when a file is opened does a pane appear,
via a new BSP content kind (shared with Direction B's plumbing).

**Trade-offs.** Lowest effort of any real integration; decorations and
overlays reuse three existing templates. Risk: overlays are modals, so a
diff you are _reading_ vanishes on Escape (fine for review) but is wrong
for _editing_ (hence the editor is a pane, not an overlay); and hover
popovers need a new z-tier above the current `z` ceiling of 40 in
`theme.ts`. Identity: preserved — blit stays a multiplexer that
_happens to know your repo_.

### B. BSP-native non-terminal panes

**Thesis.** The WM already dispatches pane content on the shape of an
assignment string. Editor/diff/filetree/git-log become first-class tiles
by adding new value kinds to that grammar — they then tile, resize,
cycle-focus, persist to the hash, and appear in presets exactly like
terminals. No overlay stack, no panel system. This is the truest fit to
"a tiling WM whose windows can be code."

**The load-bearing change** is the grammar, in both encodings, mirroring
`surfaceAssignment`: add `editor:` / `diff:` / `filetree:` / `gitlog:`
helpers to `js/core/src/bsp/layout.ts`, teach `loadAssignmentsFromHash`
the new hash prefixes, and add emit branches to the Workspace hash
writer. **Two gotchas that _will_ cost a day each if missed:** (1) these
values carry fs paths and git refs, which contain `:` and `/`, so the
new parsers must consume only the leading `kind:conn:` and treat the
remainder verbatim (the surface parser's `lastIndexOf(':')` will
mis-split a path); (2) each kind needs its own `reconcileAssignments`
`continue` branch and carry-forward entry, or the tile clears on the
next churn.

`LeafPane` then gains `<Show>` branches mounting `BlitFileTree` /
`BlitDiff` / `BlitGitLog` / `BlitEditor`, each opening its
connection-level handle lazily. The tree _spawns tiles_: Enter on a file
opens an `editor:` assignment into a sibling pane via the existing
`moveToPane` plumbing — matching the WM grain.

**Trade-offs.** Non-terminal content becomes a native citizen; nothing
bifurcates into a separate modal/panel world. Cost: the editor engine
dominates the effort (the ~8 plumbing touch points are mechanical), and
keyboard cooperation is delicate — `createKeyboardShortcuts` captures on
the capture phase, so an editor tile needs the same focus-guard the
terminal/surface panes use or it gets its keys stolen. **A and B are the
same product**: A is the overlays and status chrome, B is the tiles; ship
A first, adopt B's tile plumbing when the editor lands.

### C. Conventional docks + Monaco

**Thesis.** Reshape the chrome into a familiar VS Code silhouette — left
explorer, center editor group, bottom problems — and let Monaco own
editing, diffing, and decoration. This was the write side's original
target; [fs-write.md](design/fs-write.md) now reflects the shipped CM6
editor. Be honest that this alternative buys familiarity by importing a
second app into blit.

**What it adds.** Generalize `PreviewPanel` into a reusable `Dock`
(left/right/bottom, min-size, hash-persisted width) and instantiate an
`ExplorerDock` over `syncFs`, a `ProblemsDock` over `diags.files`, and a
`MonacoEditorPane`. The editor still needs the new BSP `editor:`
assignment kind (Monaco does not remove that plumbing).

**What it costs, candidly.**

- **Bundle.** Monaco is multi-MB and assumes web workers + dynamic
  chunks; `js/ui/vite.config.ts` is `viteSingleFile` with base64-inlined
  WASM and brotli. Fitting Monaco is real surgery and dwarfs the current
  solid-only footprint.
- **Two theming systems.** `themeFromPalette` derives all chrome from
  the ANSI palette; Monaco needs a generated theme adapter that will
  always lag and never match, syntax colors least of all.
- **A visual seam.** Monaco renders its own DOM/canvas; in a tiling
  layout an editor tile beside a terminal tile will never be
  pixel-identical, and the seam is constant.
- **Its LSP client is dead weight.** blit projects LSP into binary
  records, not JSON-RPC; Monaco's built-in diff editor likewise wants
  two full texts and re-diffs client-side, **discarding `patch()`'s
  server-side alignment, `-b`/`-w`, and word/char spans** — PR #85's
  best asset. You either render diffs in your own pane anyway or feed
  Monaco raw `blob()` bytes and lose the alignment.

**Identity impact.** This is the boring-IDE silhouette made real. It is
the fastest way to something legible to a VS Code user and the fastest
way to stop looking like blit.

## The editor engine (a separable decision)

Whichever direction, an actual editor means choosing an engine — and the
choice is separable: build the pane behind the `parseEditorAssignment`
seam so the engine can be swapped without touching plumbing.

| Axis                           | Monaco                       | CodeMirror 6                             | blit-native cell grid                       |
| ------------------------------ | ---------------------------- | ---------------------------------------- | ------------------------------------------- |
| Single-file bundle fit         | Hostile (multi-MB, workers)  | Good (ESM, tree-shakeable, tens–100s KB) | Best (no new dep)                           |
| Palette theming                | Bespoke adapter, always lags | Small theme extension from `Theme`       | Native (reads the palette)                  |
| IME / mobile / DPR             | Weak                         | Strong by design                         | Reuse `BlitTerminalSurface` machinery       |
| LSP wiring                     | Built-in client unusable     | `@codemirror/lint` + custom over records | Fully hand-rolled                           |
| Visual match to the WebGL grid | No                           | Not pixel-identical (DOM)                | Pixel-perfect (`gl-renderer.ts`)            |
| Editor-authoring effort        | Lowest                       | Medium                                   | Highest (own tokenizer/undo/selection/wrap) |

**Recommendation: CodeMirror 6 for the first real editor; the native
cell-grid editor as the aesthetic end-state.** Monaco was the RFC's original
target but the worst house-style fit — its bundle and worker model fight
`viteSingleFile` directly, and its LSP client is dead weight. The native
editor is the truest to blit (the `gl-renderer.ts` `render()` seam is
already content-agnostic, and `measureCell` + `BlitTerminalSurface`'s
IME/DPR machinery are reusable), but the glyph atlas and vertex packer
live in the Rust/WASM `Terminal` crate, and there is no in-tree syntax
highlighter — so it means building an editor engine before the first
character renders. CM6 is the only option that is a real editor _and_
fits the bundle _and_ brings IME/mobile for free.

All three share the hard edges: transcode 0-based-line/UTF-8-byte
columns ↔ the engine's UTF-16 offsets; **save through CAS**
(`buildFsWriteMessage`, base = 16-byte content hash), reconciling the
server echo (`lastWrittenHash`, so the editor never `setValue`s over its
own write) and surfacing `FsConflictError`; and detect read-only
_late_ — `BLIT_FS_WRITE` is not advertised in HELLO, so attempt the
write, catch `PERMISSION`/`FS_DONE_PERMISSION`, and flip the pane to a
read-only badge.

## Surfacing each family well

The core APIs expose far more than a first pass will use. This is the
checklist that separates "wired up" from "done," distilled from
[git.md](design/git.md), [lsp.md](design/lsp.md),
[fs-watch.md](design/fs-watch.md), and [fs-write.md](design/fs-write.md).

### git

- **Stash** is only a count today; a stash is a commit, so open it with
  the existing diff renderer on `stash^1×stash` (untracked bytes hang off
  its third parent).
- **In-progress op** is five, not two: the `OP` record carries merge,
  rebase, cherry-pick, revert, bisect. When `STATUS` marks `CONFLICTED`,
  offer a read-only 3-way view — base via `mergeBase()`, ours/theirs via
  stage-2/3 `index()` oids through `blob()`.
- **Ahead/behind honesty:** render `GONE` (upstream deleted) and dim the
  counts when `COUNTS_VALID` is clear (walk budget hit — the numbers are
  approximate). `UPSTREAM` arrives for _every_ local branch, so a
  branches overlay can show who's behind before you switch.
- **MERGE_BASE triple-dot** deserves to be a mode, not an endpoint: a
  "Review PR" view over `MERGE_BASE(upstream)×topic` with the `BASE`
  commit in the header and changed files as a checklist.
- **Blame / file-history** is the marquee one-request feature — `log()`
  with `FOLLOW | PATH_OIDS` emits the object at the rename-adjusted path
  per commit, oid-addressed and cacheable; drive a gutter blame column
  and a history scrubber that time-travels the buffer through past
  `blob()` versions.
- **Live log** via `watchLog(spec)` repaints `main..HEAD` when either
  endpoint moves — a first-class commit-graph pane, not a static page.
- **Staged vs unstaged** are distinct views (`HEAD×INDEX` vs
  `INDEX×WORKTREE`); the status overlay should split them (read-only — no
  staging). `index()` adds `INTENT_TO_ADD`/`SKIP_WORKTREE`/conflict
  stages.
- **Refs / historical browsing:** `STATE_REF` streams every ref
  (annotated-tag `PEELED_VALID`, `SYMBOLIC`); a refs palette opens
  `tree()`/`log()` at any ref, and the file tree gains a "view at ref"
  mode over `tree()` instead of `syncFs`.
- **Diff controls the protocol hands you free:** `-b`/`-w`
  (`IGNORE_SPACE_CHANGE`/`IGNORE_ALL_SPACE`), word vs `CHAR_SPANS`
  granularity, `NO_SPANS`, and `TEXT` (unified export for `git apply`) —
  all request-bit flips, zero client reprocessing. Surface a diff-view
  toolbar. Render `R98%` similarity badges and "binary file changed" for
  `BINARY`/`SUBMODULE` entries.
- **Repo shape:** annotate `BARE`/`SHALLOW`/`SPARSE`/`LINKED` so the user
  knows _why_ history or files are missing.

### LSP

- **Server manager is unwired.** `LSP_SERVERS`/`LSP_STOP`/`LSP_CANCEL`
  have full codecs in `lsp.ts` but no `BlitConnection` methods. Wire
  them for a cross-root backend list (RSS, phase, uptime, stop) — the
  real `MAX_SERVERS` defense — and a cancel button for hung queries.
- **Multi-backend + absent binaries:** one open can bind rust-analyzer
  _and_ gopls; `LSP_OPENED.detail` names matched-but-uninstalled servers
  (`"gopls: not found on PATH"`). Expand the health chip per backend and
  turn an absent binary into an actionable "install gopls" hint, not
  silence.
- **`WARMING` is a query state, not just a chip.** A def/hover on a cold
  server resolves with status `WARMING` (retryable) — show
  "server warming — retrying" and auto-retry on the next `ready`,
  mirroring `blit lsp wait`.
- **Cold-file coverage:** _absence of a `DIAG_FILE` means unknown, not
  clean._ A problems view that lists `diags.files` will read "0 problems
  = clean," which is false for never-opened TypeScript/pyright/clangd
  files. Distinguish "clean" from "not yet analyzed."
- **Outline vs search:** `documentSymbols` (depth-nested) is a breadcrumb
  / outline of the focused file; `workspaceSymbols` is the finder — keep
  them distinct (and note ws-symbols routes to the first capable backend,
  so a second language's symbols in the same root may be missed).
- **Rename is preview-only and may be `INCOMPLETE`:** the `EDIT` records
  are text edits against a hash, _never applied_, and `incomplete` means
  whole-file create/rename/delete ops were dropped. Show it as an
  advisory diff with an "advisory — file moves not included" warning when
  partial; do not present it as a complete edit set. (Applying it is a
  deliberate fs-write compose — see below.)
- **Detail rendering:** fade `UNNECESSARY`, strike `DEPRECATED`,
  severity-color the gutter; render hover `MARKUP` as sanitized markdown
  and highlight its returned range; group `references` by file; gray
  leader entries per `caps` bits and re-enable on an `epoch` bump.
  Provide your own SymbolKind→icon and severity→color tables — core ships
  only `lspStatusText`.

### fs (read)

- **Content availability drives the UX:** `UNSTABLE` (being written —
  show "waiting to settle," not blank/stale), `UNREADABLE` (dim + lock
  glyph), `NO_CONTENT` (large — a size-aware "fetch N MiB?" affordance
  streaming via `fetch()`).
- **Symlink content is its _target bytes_** — opening a symlink edits the
  link target, not the destination. Mark symlinks (`a → b`) and offer
  "edit link target" vs "follow to destination" (a fresh sync of the
  resolved path).
- **Non-UTF-8 paths arrive escaped** (`%XX`/`%uXXXX`/`%25`); the security
  note is explicit — _never render escaped names as trusted text_. Show a
  decoded-with-replacement label, keep a copy-exact affordance, and pass
  the escaped form verbatim back to `fetch`/`writeFile` — never re-escape
  a display string.
- **`EXCLUDE_GIT` is unimplemented in the TS client** (only
  `RECURSIVE`/`CONTENT`/`CROSS_FILESYSTEM` exist). So a file tree must
  filter `.git` client-side **and** eat the bandwidth of mirroring
  object-store churn — a small server + core follow-up removes both.
- **`MOVE` and `FS_CLOSED`:** an open editor whose path is renamed gets a
  `MOVE` record — follow it and retitle the tab rather than losing the
  buffer; on `FS_CLOSED`, show a reason-specific banner
  ("root deleted" / "permission lost") with a re-sync button, not a
  silent freeze.

### fs (write) — the explorer half, largely uncovered

The write surface exists today; treating writes as only "the editor's
Save" leaves most of it on the floor.

- **Explorer operations:** New File (create-exclusive, zero `base`, so
  two tabs can't clobber), New Folder (`mkdir`), Delete (`remove` with
  `ifHash`), Rename & drag-move (`rename`, one op), New Symlink
  (`symlink`).
- **Conflict → 3-way, no round trip:** `FS_DONE.hash` on `CONFLICT`
  carries the current disk hash, so present Reload / Overwrite
  (`NO_CAS`) / Compare (the git patch renderer against the fetched disk
  bytes) with the hash already in hand.
- **External-change discipline:** on an incoming `UPSERT` whose hash
  differs from the editor's, clean buffer → silently apply the computed
  diff (never `setValue`); dirty buffer → a "changed on disk —
  Reload/Keep/Compare" banner.
- **Apply an LSP rename as N CAS'd writes** — the flagship fs×LSP
  compose, surfaced by nobody: rename `EDIT` records → N `writeFile`
  calls, each `ifHash`-guarded, stopping mid-batch on `CONFLICT` and
  reporting which files landed. This is the one place the read-only
  stance is deliberately crossed.

## Cross-cutting concerns

- **Multi-host repo identity.** Handles are per-connection; the git chip
  tracks the _focused pane's_ host, and finder results are host-prefixed
  (`rabbit:src/main.rs`) exactly as terminals are.
- **Hash-based staleness join.** `LOCATION.hash`, `DIAG_FILE.hash`, and
  `FsNode.hash` are all BLAKE3-128; dim a diagnostic/hover/def answer
  whose hash ≠ the current fs-sync `node.hash` as "stale — recomputing."
  Do _not_ try to join git into this — git uses oids, a different space.
- **Read-only stance messaging.** Three families are read-only by
  construction where they are read-only (no staging/commit/push, no
  applyEdit/format, fs writes may be gated off). A one-time explainer and
  grayed, tooltip'd affordances keep "why can't I commit here?" from
  reading as a bug.
- **Overflow is everywhere.** `REFS_TRUNCATED`/`STATUS_TRUNCATED`, tree/
  diff/patch/index `TRUNCATED`, log `MORE` + `frontier`, blob
  `TOO_LARGE`, `!COUNTS_VALID`. Render "N more / results truncated"
  footers and paginate logs statelessly from `frontier`.
- **Mobile / touch and accessibility.** blit is iPad/Android-hardened,
  yet leader chords and telescope are keyboard-first — every summonable
  surface needs a touch trigger (long-press rows, a leader button,
  swipe-dismiss). Overlays need dialog roles + focus traps; diff rows and
  diagnostics need text alternatives so the WebGL-adjacent chrome stays
  navigable.

## Corrections to common assumptions

Verified against the fs branch, to save the next reader the rediscovery:

- fs sync is **read-write**, not read-only — an editor saves today
  (`writeFile`, CAS, `BLIT_FS_WRITE` gate). Only git and LSP are
  read-only.
- The git branch chip is **near-free for refs + ahead/behind**, but the
  dirty-file count costs a worktree scan on a second settle window.
- LSP rename preview is **not** a drop-in for the git patch renderer —
  `EDIT` records are per-file text edits, not `PATCH_ROW` rows; feeding
  the row renderer means synthesizing before/after text and re-diffing.
- "Jump from a diagnostic to the terminal that owns the file" is a
  **fuzzy heuristic** — there is no file→terminal-by-cwd API. Make the
  primary jump target an editor pane at the range (the fs-sync path is
  authoritative).
- `openLsp` is **connection-only**; `BlitWorkspace.openLsp` does not
  exist yet.
- `EXCLUDE_GIT` is **not** exposed by the TS client; `.git` filtering is
  currently mandatory and client-side.

## Rollout

Ordered so each step ships value and de-risks the next; the first three
need **no editor**.

1. **Status segments + the reactive bridge.** git chip (refs +
   ahead/behind + op banner + stash badge) and an LSP warmup chip and a
   diagnostics count. Touches only `StatusBar.tsx` / `Workspace.tsx`;
   pure pushed state; highest value-to-effort.
2. **Read-only git diff overlay.** `RemotesOverlay` template + `patch()`
   row rendering — the reusable primitive for rename-preview and, later,
   the editor's diff view.
3. **Diagnostics overlay + terminal decorations + the telescope
   finder** (extend `SwitcherOverlay`, don't build a new overlay).
4. **Read-only viewer tile.** The new `editor:`/`diff:` assignment kind
   end-to-end — grammar helpers, the `reconcileAssignments` branch (the
   #1 gotcha), hash (de)serialization, a `LeafPane` `<Show>` branch over
   `syncFs` bytes with LSP decorations. Read-only sidesteps CAS.
5. **CodeMirror 6 behind that tile** — hover/def/refs/outline from
   `openLsp`; add the `BlitWorkspace.openLsp` wrapper.
6. **Saves** — `buildFsWriteMessage`, conflict UX, read-only probe — and
   **explorer file ops** (New File/Folder, rename, delete). Rename-apply
   reuses step 2's renderer for preview and step 6's writes for apply.

Later, as adoption warrants: blame + file-history scrubber, the PR-review
`MERGE_BASE` mode, the LSP server manager (`LSP_SERVERS`/`STOP`), the
`fromSessionId` active-root resolution so git/LSP chrome follows `cd`
(Decision 3), and — spike permitting (Decision 5) — a native cell-grid
editor for pixel-perfect terminal parity.

## Design decisions

The hard questions this design raises, each taken to a decision optimized
for UX quality and pressure-tested against its strongest objection.

### 1 · Content kind — assignment-driven, with the DSL as sugar

**Assignment-driven placement is the only substrate; a DSL sigil is
sugar that pre-writes one assignment, never a parallel "role" system, and
never for editor/diff.** A target-less kind declared in a preset —
`line(@filetree, shell)` — must be pinned to the layout's _owning_
connection, not the focused one, or a shared layout renders differently
depending on whose pane happened to be focused when it loaded, which
destroys the reproducibility that justifies touching the DSL at all.
Editor/diff stay action-opened: a preset can't name a file, and an empty
pane is a better first impression than an arbitrary auto-opened buffer.
Framing the sigil as sugar over the existing assignment string (not a new
concept beside it) keeps exactly one source of truth.

- _Trade-off:_ presets pin pane _roles_, never "editor showing
  engine.rs" — that's session state, not layout.
- _Mechanism:_ a `kind` field on `BSPLeaf` (`js/core/src/bsp/dsl.ts`); an
  instantiation effect writes a `surfaceAssignment`-style
  `filetree:<rootConn>` string into `BSPAssignments`, which
  `reconcileAssignments` already preserves (its `!known.has(value)` keep
  branch) and the hash persists; editor/diff keep landing only through
  `moveToPane` (`BSPContainer.tsx`).

### 2 · Finder vs. overlays — one to _go_, surfaces to _read_

**One navigation finder plus two reading surfaces — and the finder is
locate-and-jump only (`@` symbols, `:` commits, `!` diagnostics, default
files); commands are not a finder mode.** The taxonomy is GO / READ / DO:
GO resolves to a location (the finder), READ is a surface you dwell in
(the git status+diff, the problems list), DO resolves to an action (the
which-key leader). A command palette is DO, so it stays on the leader — a
`>` sigil would just duplicate it. Diagnostics deliberately live in both
places (`!` to jump, the problems surface to scan), which is coherent so
long as both read from the _same_ `LspDiagMirror` and never drift.

- _Trade-off:_ two surfaces to maintain plus a `!`-vs-problems seam, paid
  down by identical Enter→jump on both and one shared diagnostic source.
- _Mechanism:_ extend `SwitcherOverlay`'s `flatItems`/`selectedIdx` with
  `workspaceSymbols`/`watchLog`/`diags.files` item kinds; dedicated
  overlays off `Overlay.tsx` reuse `RemotesOverlay`'s table for the git
  diff (`patch()`/`PATCH_ROW`) and `LspDiagMirror` for problems; commands
  stay in `createKeyboardShortcuts.ts`.

### 3 · Handle binding — one ref-counted handle per resolved root, following focus

**Panes bind to a server-resolved canonical root through a
reference-counted handle the user never names; the active root follows
focus — a file pane's path, else the focused terminal's live cwd — and
all git/LSP chrome reflects it.** Two panes in one worktree coalesce onto
one handle keyed by the root that `gix` / the LSP marker-walk already
discover upward. The catch, verified against the branch: a terminal's cwd
lives only server-side (`pty_cwd`, read from the kernel) and is never
pushed to the browser, so the client's launch-cwd is stale the instant
you `cd` — pointing the branch chip and diagnostics at the wrong repo.
The fix keeps the model intact and is the _one_ backend addition this
design asks for: let the open verbs take a source session.

- _Trade-off:_ correct-on-`cd` active root needs a small `fromSessionId`
  on three verbs (so this part is _not_ zero-backend) plus a debounce, so
  cross-tree focus hops don't re-run `gix discover` + the worktree status
  scan.
- _Mechanism:_ add `fromSessionId` to `openRepo`/`openLsp`/`syncFs`
  mirroring the existing `cwdFromSessionId` (`BlitWorkspace.ts`); the
  server resolves the live cwd (`pty_cwd`) then `gix discover`; the client
  coalesces and ref-counts on the resolved root returned in the reply
  (file-pane paths stay authoritative); release is backstopped by an idle
  grace, each affordance gated on its `FEATURE_*` bit.

### 4 · Surface writes — yes, with legible, intent-scoped capability

**Editing is first-class where the host allows it, and write-capability
is a legible ambient per-host state resolved the moment you open a file
to edit — never guessed at connect time, never discovered by a bounced
save.** Because `BLIT_FS_WRITE` isn't advertised in the handshake, probe
_with intent_: on the first editable open, piggyback a create-exclusive
write (`base=0`) on that just-opened, guaranteed-existing path — it
returns `PERMISSION` on a read-only host and `CONFLICT` on a writable one,
and mutates nothing either way. Show the result as a new
live / read-only / settling dot in the `ConnectionDot` _visual idiom_ — a
new state, not `RemotesOverlay`'s `readOnly` (which only gates editing the
remotes list). Every real save is CAS; a `CONFLICT`'s returned fresh hash
drives one Reload / Overwrite / Compare surface that doubles as the
human-vs-agent coherence layer; read-only hosts degrade to a polished
viewer, not a disabled editor.

- _Trade-off:_ one speculative round-trip per editable open and a brief
  "settling" state, since the protocol offers no capability bit to read.
- _Mechanism:_ `FS_DONE_PERMISSION` (the server short-circuits
  `C2S_FS_WRITE` when `BLIT_FS_WRITE=0`) vs `FS_DONE_CONFLICT`; conflict
  rebase via the fresh hash in `FsConflictError` (`js/core/src/fs.ts`);
  badge in `StatusBar.tsx`.

### 5 · Editor engine — CodeMirror 6 is the end-state; native is spike-gated

**CodeMirror 6 is the durable default editor; the native cell-grid editor
is gated on a time-boxed spike, not pre-committed as the end-state.** A
CM6 theme generated from `themeFromPalette` on matched terminal font
metrics erases the glyph/color mismatch, leaving only a sub-pixel
DOM-vs-WebGL difference — and in terminal-first blit the editor tile is
_summoned_, not omnipresent, so pixel parity is the marginal last
increment, not a wound felt every session. Native's real cost is also
mislocated: the atlas/vertex packer already live in the WASM `Terminal`
crate and the IME/DPR machinery is reusable, so the genuinely-unbuilt work
is the editor _semantics_ CM6 hands you free (buffer, undo, selection,
soft-wrap). Fund native only if a spike proves the grid crate can cheaply
model an editable, soft-wrapped buffer _and_ editor tiles become an
always-tiled primary surface; otherwise CM6 is the honest end-state.

- _Trade-off:_ accept an imperceptible DOM/WebGL seam indefinitely to get
  a real, IME-strong editor now with zero speculative engine build.
- _Mechanism:_ CM6 behind the `editor:` kind (`bsp/layout.ts` +
  `LeafPane`), themed via `themeFromPalette`, saving through
  `buildFsWriteMessage` CAS; the spike targets `gl-renderer.ts` `render()`
  against the WASM `Terminal` crate's glyph atlas.
