# blit IDE panels — build plan

- **Status:** Draft / execution roadmap
- **Companion to:** [ide.md](ide.md) (design + Decisions)

The design exploration and its resolved Decisions live in [ide.md](ide.md).
This is the execution plan: what to build, in what order, grounded in the
current tree, so each PR ships something usable and de-risks the next.
Line references are to the `fs` branch at the time of writing — re-confirm
before editing, they drift.

## Finished shape

The main row (`js/ui/src/Workspace.tsx`, the `flex-direction: row`
`<section>`) becomes **`[LeftDock] · [BSP flex:1] · [PreviewPanel]`** —
three `flex-shrink: 0` siblings, BSP absorbing the remainder. The
**LeftDock** mirrors the existing right-side `PreviewPanel` on the
opposite edge and shows one of four strictly **read-only** panels at a
time — **Explorer** (live fs tree), **SCM** (git status + stashes + op
banner), **Log** (live commit graph), **Problems** (LSP diagnostics) —
each toggled from a **StatusBar** button (highlighted, live-badged) and a
keybinding. Selecting a row opens a **BSP tile**: a `diff:` tile
(side-by-side `patch()` rows with intraline spans) or an `editor:` tile
(CodeMirror 6). Editor and diff tiles tile, resize, persist to the URL
hash, and survive layout churn exactly like terminals and surfaces today.
Everything — docks, panels, CM6 chrome, CM6 syntax — is tinted from the
active terminal ANSI palette via `themeFromPalette` and regenerates on
palette change. All handles (`syncFs` / `openRepo` / `openLsp`) open
lazily, ref-counted per **server-resolved root**, following the focused
pane's live cwd via `fromSessionId`.

## Foundations (land first)

Cross-cutting; every panel needs at least the bridge. F1+F2 first, F3 in
parallel, F4 gates the first tile (lands inside PR-6). ~6 days of
foundation unblocks ~30 days of panels.

### F1 — Reactive mirror bridge · `js/ui/src/ide/bridge.ts` · S

The mirrors are non-reactive plain classes/Maps: a `createMemo` over
`handle.live` will not re-run. One helper bumps a Solid signal inside
each push callback; selectors then read the map behind that version
token.

- git/lsp callbacks carry a monotonic id — `GitOpenOptions.onState(mirror,
stateId)`, `LspOpenOptions.onState/onDiagnostics(mirror, id)`,
  `watchLog`'s `onUpdate(page)`. Use the id as the new version.
- **fs callbacks are argument-less** — `FsSyncOptions.onRecord(record)` /
  `onReset()` / `onSync()` / `onUpdate()`. The helper self-increments a
  counter, and **must also bump on `onReset`** (a RESET restage swaps the
  map; a selector reading mid-restage sees a torn/empty tree).
- `onClosed` bumps a parallel **phase** signal driving the
  live / settling / closed badge (Decision 4).

### F2 — Handle registry + `BlitWorkspace.openLsp` · M

One ref-counted handle per **server-resolved canonical root**, opened
lazily with `fromSessionId`, released on last ref + idle grace. The
resolved root arrives only in the reply, so the registry keys
provisionally on `${connId}\0${fromSessionId|path}`, then **re-keys on the
returned root** so two panes in one worktree coalesce.

- **Root is not one field.** `FsSyncHandle.root` and `LspHandle.root`
  exist; git has **no `root`** — use `workdir || gitdir` (`workdir` is
  empty for a bare repo, so the fallback is mandatory or all bare repos
  collide onto `""`). The registry needs a per-kind root extractor.
- **Missing seam:** `syncFs`/`openRepo` exist on `BlitWorkspace`;
  **`openLsp` is connection-only**. Add `BlitWorkspace.openLsp(connId,
path, opts)` forwarding to `requireConnection(...).openLsp` (threading
  `fromSessionId`, already wired at the connection layer).
- **Standing ref for the focused root.** StatusBar chips (PR-3) must stay
  live when every panel is closed, so the registry keeps git+lsp open for
  the focused root — an explicit "focused root never releases to 0" rule,
  decided here, not in PR-3.

### F3 — Palette CM6 theme · `js/ui/src/ide/cm-theme.ts` · M (feeds editor PRs only)

Generate an `EditorView.theme` + `HighlightStyle` over
`@codemirror/language` tags from `Theme` + `palette.ansi[]`, regenerated
on palette change. Docks/panels need no new theming — they read `theme()`
and reuse `overlayChromeStyles` / `sidebarWidth` / `z`; hover popovers
need a **new z-tier above 40**. `viteSingleFile` inlines everything — CM6
fits, Monaco does not (Decision 5).

### F4 — New BSP tile-kind plumbing (lands in PR-6)

A tile's content is chosen by the **shape** of its assignment string
(`LeafPane` in `js/ui/src/bsp/BSPContainer.tsx`). Adding a kind touches
**five sites** — and the failure mode is a tile that vanishes one layout
churn later, not immediately:

1. **Kind-specific parser** — mirror `surfaceAssignment` but **do not
   reuse `parseSurfaceAssignment`**: its `lastIndexOf(":")` mis-splits any
   path/oid containing `:` or `/`. Split only the leading
   `<kind>:<conn>:` and keep the remainder **verbatim** (paths arrive
   escaped — pass through, never re-escape).
2. **Hash form** — `e:`/`d:` branch in `js/ui/src/bsp/layout.ts`
   `loadAssignmentsFromHash` + the emitter in `Workspace.tsx`.
3. **Carry-forward collector** — `BSPContainer.tsx` (the layout-change
   effect, ~`:215-243`) re-appends only surfaces + live sessions, so an
   `editor:`/`diff:` value is dropped on **layout churn**. This is the
   real vanish site. (Note: `reconcileAssignments` in
   `js/core/src/bsp/layout.ts` _keeps_ unknown values via its
   `!known.has(value)` branch — add a `continue` there as
   defense-in-depth, but it is **not** where the bug is.)
4. **Focus guards** — `assignedInPaneOrder` (~~`:441`) and especially
   `focusedPaneSessionId` (~~`:466`) return a non-surface value as a
   session id; focusing a read tile would emit `"editor:…"` into focus
   tracking / hash `p=` / auto-placement. Guard both.
5. **`LeafPane` `<Show>` branch** mounting the tile component.

**Test:** the churn-survival test must **swap the layout** (preset / split
firing the layout-change effect), not add/remove a session — a session
churn passes even when broken.

## Ordered PRs

The first five need **no editor and no grammar change**; the grammar lands
behind the read-only diff tile (PR-6); CM6 and writes follow.

| PR   | Delivers                                                                                          | Consumes | Deps           | Effort |
| ---- | ------------------------------------------------------------------------------------------------- | -------- | -------------- | ------ |
| 1    | LeftDock shell (clone PreviewPanel, mirror to the left edge)                                      | —        | —              | M      |
| 2    | Persistence + keybindings (Ctrl+Shift+E/G/L/P); also persist `previewPanelWidth`                  | —        | PR-1           | S      |
| 3    | StatusBar toggles + live chips (branch ↑↓ w/ GONE/COUNTS_VALID, LSP health, problem/dirty badges) | git, lsp | F1, F2, PR-1   | M      |
| 4    | ExplorerPanel (read-only tree; `.git` filtered; unreadable/no-content/symlink states)             | fs       | F1, F2, PR-1   | M      |
| 5    | SCM + Log + Problems panels (read-only; log graph via `watchLog`; absent path = unknown)          | git, lsp | F1, F2, PR-1   | L (×3) |
| 6    | BlitDiff tile — **introduces F4 grammar**; `patch()` rows; 3 endpoint modes; wires row-clicks     | git      | F4, PR-5       | L      |
| 7    | Read-only editor tile (`editor:` kind, byte viewer; grammar now proven)                           | fs       | F4, PR-4, PR-6 | M      |
| 8    | CM6 engine + LSP squiggles (**highest complexity risk**)                                          | fs, lsp  | F3, PR-7       | L      |
| 9    | Saves + file ops + read-only probe (CAS + Reload/Overwrite/Compare)                               | fs       | PR-4, PR-8     | L      |
| 10   | Interactive LSP: hover / def / refs / outline (WARMING auto-retry)                                | lsp      | F2, PR-8       | M      |
| 11\* | LSP server manager (`LSP_SERVERS/STOP/CANCEL` need `BlitConnection` methods)                      | lsp      | —              | S      |

\* optional.

**Panels are strictly read-only** (per [ide.md](ide.md)); only the editor
(PR-9) writes, via the fs-write CAS surface.

## Risks & sequencing notes

- **PR-6 is the highest _foundational_ risk** (the grammar + carry-forward
  drop) — land it behind the read-only diff tile where there's no editor
  state to lose, with a layout-churn survival test.
- **PR-8 is the highest _complexity_ risk** (the sleeper): pervasive
  0-based-line/UTF-8-byte ↔ UTF-16 transcoding (silent corruption on any
  off-by-one; core ships no helper), self-echo suppression racing
  `lastWrittenHash` against `MOVE`/`UPSERT` ordering, and CM6's
  `contenteditable` fighting the `LeafPane` auto-focus effect (which
  re-focuses any `[tabindex]` on every flush). Its bugs are invisible in a
  demo.
- **fs has no reactive id** — the F1 helper must special-case the
  argument-less fs callbacks (incl. `onReset`) with a self-incrementing
  counter, or fs panels silently never re-render.
- **Registry re-keying is load-bearing** — resolved root only arrives in
  the reply; re-home provisional keys on the returned root or two panes in
  one worktree open duplicate handles and thrash `gix discover`.
- **Absent LSP path ≠ clean** — the Problems panel must gate "no problems"
  on server phase being `READY`, or indexing repos render a false
  all-clear.
- **Ship-value ordering** — PR-1…PR-5 deliver a fully usable read-only IDE
  dock with zero grammar/editor risk; the grammar/CM6/write risk is
  deferred to PR-6+, each landing something usable.
