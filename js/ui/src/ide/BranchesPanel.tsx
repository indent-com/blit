/**
 * BranchesPanel — `git branch` for the left dock, plus the worktrees.
 *
 * A read-only view. Clicking a branch retargets the Log panel's revision spec
 * at it rather than checking it out: the protocol has no checkout, and on a
 * box where the same repository is open in several worktrees a click that
 * moved HEAD would be the most destructive thing in the sidebar. Clicking a
 * worktree navigates — it re-roots the whole dock — because that is how you
 * actually move between lines of work here.
 *
 * Local branches are always listed; remotes and tags fold away, because a
 * repository with 4 branches can easily have 400 tags and the panel's job is
 * to answer "what am I on and what else is there" at a glance.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  For,
  onCleanup,
  Show,
} from "solid-js";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import type { IdeSession } from "./session";
import {
  worktreeForBranch,
  type BranchRow,
  type WorktreeRow,
} from "./branchList";

/** How many commits' worth of divergence renders as an arrow pair. */
function aheadBehind(row: BranchRow): string {
  const u = row.upstream;
  if (!u) return "";
  if (u.gone) return "gone";
  // A budget-exhausted count is not zero. Saying "↑0 ↓0" for "the server
  // could not afford to walk this" would be a lie in the one direction that
  // matters — it reads as "in sync".
  if (!u.countsValid) return "±?";
  const parts: string[] = [];
  if (u.ahead) parts.push(`↑${u.ahead}`);
  if (u.behind) parts.push(`↓${u.behind}`);
  return parts.join(" ");
}

/**
 * The keyboard half of a `role="button"` row.
 *
 * These rows are flex containers with their own column layout, not `<button>`s,
 * so the button *behaviour* has to be spelled out: Enter and Space both
 * activate, and Space is prevented from scrolling the panel out from under the
 * row it just activated. Same shape as the unit rows in SystemdPanel.
 *
 * Only when the row itself holds the key. A worktree row contains a real
 * `<button>` (open a terminal here), and keydown bubbles whether or not that
 * button's click was stopped — without this, Enter on it would both open a
 * terminal and navigate away from the worktree it was opening one in.
 */
function onActivateKey(run: () => void) {
  return (event: KeyboardEvent) => {
    if (event.target !== event.currentTarget) return;
    if (event.key !== "Enter" && event.key !== " ") return;
    event.preventDefault();
    run();
  };
}

export function BranchesPanel(props: {
  session: IdeSession | null;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  /** Re-root the dock at a worktree. */
  onOpenWorktree?: (path: string) => void;
  /** Open a terminal in a worktree — the secondary, explicit action. */
  onOpenTerminalIn?: (path: string) => void;
}) {
  // The worktree list is a request that costs the server a repository open
  // per worktree, so it only runs while this panel is mounted. The dock
  // unmounts a collapsed section, which is what makes folding free.
  createEffect(() => {
    const release = props.session?.ensureWorktrees();
    if (release) onCleanup(release);
  });

  const [showRemotes, setShowRemotes] = createSignal(false);
  const [showTags, setShowTags] = createSignal(false);

  const groups = () => props.session?.branches();
  const worktrees = (): WorktreeRow[] => props.session?.worktrees() ?? [];
  const spec = () => props.session?.logSpec() ?? "";

  // A branch is "shown" when the log spec is exactly it — the panel reflects
  // what the Log panel is walking rather than keeping its own selection, so
  // the two cannot disagree.
  const shownRef = createMemo(() => spec().trim());

  const rowPad = () =>
    `1px ${props.scale.panelPadding}px 1px ${props.scale.panelPadding + 6}px`;

  function branchRow(row: BranchRow) {
    const elsewhere = () => {
      const wt = worktreeForBranch(worktrees(), row.ref);
      return wt && !wt.current ? wt : null;
    };
    const shown = () => shownRef() === row.ref;
    const divergence = aheadBehind(row);
    return (
      <div
        // A stable hook for tests: the tooltip carries a worktree path when
        // the branch is checked out elsewhere, so matching on it cannot tell
        // a branch row from a worktree row.
        data-branch={row.ref}
        role="button"
        tabindex={0}
        onClick={() => props.session?.setLogSpec(row.ref)}
        onKeyDown={onActivateKey(() => props.session?.setLogSpec(row.ref))}
        title={[
          row.ref,
          row.oid.slice(0, 12),
          row.upstream ? `tracks ${row.upstream.ref}` : "",
          elsewhere() ? `checked out in ${elsewhere()!.path}` : "",
        ]
          .filter(Boolean)
          .join("\n")}
        style={{
          display: "flex",
          "align-items": "baseline",
          gap: `${props.scale.tightGap}px`,
          padding: rowPad(),
          "font-family": props.fontFamily,
          "font-size": `${props.scale.md}px`,
          cursor: "pointer",
          background: shown() ? props.theme.hoverBg : undefined,
        }}
      >
        {/* `git branch`'s own marker, and the only glyph column here. */}
        <span
          style={{
            width: "1ch",
            "flex-shrink": 0,
            color: props.theme.accent,
          }}
        >
          {row.head ? "*" : ""}
        </span>
        <span
          style={{
            color: row.head ? props.theme.accent : props.theme.fg,
            "font-weight": row.head ? 700 : 400,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
          }}
        >
          {row.label}
        </span>
        <Show when={row.isRemoteDefault}>
          <span
            title="the remote's default branch"
            style={{
              "font-size": `${props.scale.xs}px`,
              color: props.theme.dimFg,
              "flex-shrink": 0,
            }}
          >
            default
          </span>
        </Show>
        {/* A branch checked out in another worktree cannot be checked out
            here — the single most useful thing to mark in a repo that is
            used through worktrees at all. */}
        <Show when={elsewhere()}>
          {(wt) => (
            <span
              title={`checked out in ${wt().path}`}
              style={{
                "font-size": `${props.scale.xs}px`,
                color: props.theme.dimFg,
                "flex-shrink": 0,
              }}
            >
              ⌥{wt().name}
            </span>
          )}
        </Show>
        <span style={{ "margin-left": "auto", "flex-shrink": 0 }}>
          <Show when={divergence}>
            <span
              style={{
                "font-size": `${props.scale.xs}px`,
                color:
                  row.upstream?.gone || !row.upstream?.countsValid
                    ? props.theme.warning
                    : props.theme.dimFg,
                "font-variant-numeric": "tabular-nums",
              }}
            >
              {divergence}
            </span>
          </Show>
        </span>
      </div>
    );
  }

  function worktreeRow(row: WorktreeRow) {
    // A prunable worktree has no directory to open, and a bare one never had
    // a checkout: both are listed (they are real entries) but neither is a
    // navigation target, so nothing pretends they are clickable — which now
    // also means not taking the focus and not calling itself a button.
    const navigable = () => !row.prunable && !row.bare && row.path !== "";
    return (
      <div
        data-worktree={row.path}
        role={navigable() ? "button" : undefined}
        tabindex={navigable() ? 0 : undefined}
        onClick={() => {
          if (navigable()) props.onOpenWorktree?.(row.path);
        }}
        // No handler at all on a non-navigable row: swallowing Space there
        // would stop the panel scrolling for an activation that never happens.
        onKeyDown={
          navigable()
            ? onActivateKey(() => props.onOpenWorktree?.(row.path))
            : undefined
        }
        title={[
          row.path || "(bare — no checkout)",
          row.detached
            ? `detached at ${row.oid.slice(0, 12)}`
            : row.branchLabel
              ? `on ${row.branchLabel}`
              : "",
          row.locked
            ? `locked${row.lockReason ? `: ${row.lockReason}` : ""}`
            : "",
          row.prunable
            ? "checkout is gone — `git worktree prune` would drop it"
            : "",
        ]
          .filter(Boolean)
          .join("\n")}
        style={{
          display: "flex",
          "align-items": "baseline",
          gap: `${props.scale.tightGap}px`,
          padding: rowPad(),
          "font-family": props.fontFamily,
          "font-size": `${props.scale.md}px`,
          cursor: navigable() ? "pointer" : "default",
          background: row.current ? props.theme.hoverBg : undefined,
          opacity: row.prunable ? 0.55 : 1,
        }}
      >
        <span
          style={{ width: "1ch", "flex-shrink": 0, color: props.theme.accent }}
        >
          {row.current ? "●" : ""}
        </span>
        <span
          style={{
            color: props.theme.fg,
            "font-weight": row.current ? 700 : 400,
            "text-decoration": row.prunable ? "line-through" : undefined,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
          }}
        >
          {row.name}
        </span>
        <Show when={row.main}>
          <span
            style={{
              "font-size": `${props.scale.xs}px`,
              color: props.theme.dimFg,
              "flex-shrink": 0,
            }}
          >
            main
          </span>
        </Show>
        <span
          style={{
            "font-size": `${props.scale.xs}px`,
            color: row.detached ? props.theme.warning : props.theme.dimFg,
            overflow: "hidden",
            "text-overflow": "ellipsis",
            "white-space": "nowrap",
          }}
        >
          {row.detached ? `detached ${row.oid.slice(0, 7)}` : row.branchLabel}
        </span>
        <span
          style={{
            "margin-left": "auto",
            "flex-shrink": 0,
            display: "flex",
            "align-items": "center",
            gap: `${props.scale.tightGap}px`,
          }}
        >
          <Show when={row.locked}>
            <span
              title={`locked${row.lockReason ? `: ${row.lockReason}` : ""}`}
              style={{
                "font-size": `${props.scale.xs}px`,
                color: props.theme.dimFg,
              }}
            >
              🔒
            </span>
          </Show>
          <Show when={navigable() && props.onOpenTerminalIn}>
            <button
              onClick={(e) => {
                e.stopPropagation();
                props.onOpenTerminalIn?.(row.path);
              }}
              title={`Open a terminal in ${row.path}`}
              style={{
                background: "transparent",
                border: "none",
                color: props.theme.dimFg,
                cursor: "pointer",
                padding: "0 2px",
                "font-family": props.fontFamily,
                "font-size": `${props.scale.xs}px`,
              }}
            >
              {"❯"}
            </button>
          </Show>
        </span>
      </div>
    );
  }

  /** A fold-away sub-heading for the lists that can be enormous. */
  function subHeading(
    label: string,
    count: number,
    open: () => boolean,
    toggle: () => void,
  ) {
    return (
      <div
        // A disclosure, not a navigation target: aria-expanded is what says
        // which way the ▸/▾ is pointing to anything that cannot see it.
        role="button"
        tabindex={0}
        aria-expanded={open()}
        onClick={toggle}
        onKeyDown={onActivateKey(toggle)}
        style={{
          display: "flex",
          "align-items": "center",
          gap: `${props.scale.tightGap}px`,
          padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px 2px`,
          "font-family": props.fontFamily,
          "font-size": `${props.scale.xs}px`,
          color: props.theme.dimFg,
          cursor: "pointer",
          "user-select": "none",
        }}
      >
        <span style={{ width: "1ch", "flex-shrink": 0 }}>
          {open() ? "▾" : "▸"}
        </span>
        <span>{label}</span>
        <span
          style={{
            "margin-left": "auto",
            "font-variant-numeric": "tabular-nums",
          }}
        >
          {count}
        </span>
      </div>
    );
  }

  const heading = (label: string, count?: number) => (
    <div
      style={{
        display: "flex",
        "align-items": "center",
        gap: `${props.scale.tightGap}px`,
        padding: `${props.scale.tightGap}px ${props.scale.panelPadding}px 2px`,
        "font-family": props.fontFamily,
        "font-size": `${props.scale.xs}px`,
        color: props.theme.dimFg,
      }}
    >
      <span>{label}</span>
      <Show when={count !== undefined}>
        <span
          style={{
            "margin-left": "auto",
            "font-variant-numeric": "tabular-nums",
          }}
        >
          {count}
        </span>
      </Show>
    </div>
  );

  return (
    <div
      style={{
        flex: "1 1 0",
        "min-height": 0,
        "overflow-y": "auto",
        ...scrollbarStyle(props.theme),
      }}
    >
      <Show
        when={props.session}
        fallback={
          <div
            style={{
              padding: `${props.scale.panelPadding}px`,
              "font-size": `${props.scale.sm}px`,
              color: props.theme.dimFg,
            }}
          >
            No root selected.
          </div>
        }
      >
        <Show
          when={!props.session!.noRepo()}
          fallback={
            <div
              style={{
                padding: `${props.scale.panelPadding}px`,
                "font-size": `${props.scale.sm}px`,
                color: props.theme.dimFg,
              }}
            >
              Not a git repository.
            </div>
          }
        >
          {/* ── Worktrees, above the branches: this panel's reason to exist
              is moving between them, and there are always few of them. A
              repository with only its main worktree still lists that one —
              saying "1 worktree" is how the section explains itself. */}
          {heading("WORKTREES", worktrees().length || undefined)}
          <Show
            when={worktrees().length > 0}
            fallback={
              <div
                style={{
                  padding: `0 ${props.scale.panelPadding}px 2px ${props.scale.panelPadding + 6}px`,
                  "font-size": `${props.scale.sm}px`,
                  color: props.theme.dimFg,
                }}
              >
                <Show
                  when={props.session!.worktreesError()}
                  fallback="Loading…"
                >
                  {(err) => (
                    <span style={{ color: props.theme.warning }} title={err()}>
                      {err()}
                    </span>
                  )}
                </Show>
              </div>
            }
          >
            <For each={worktrees()}>{worktreeRow}</For>
          </Show>

          {/* "LOCAL", not "BRANCHES": the dock section header above already
              says Branches, and this way the three ref groups read as a set
              (LOCAL / REMOTE / TAGS). */}
          {heading("LOCAL", groups()?.local.length)}
          {/* An unborn HEAD has no ref record, so without this a fresh repo
              (or a `worktree add -b` before its first commit) shows an empty
              branch list while `git branch --show-current` names one. */}
          <Show when={groups()?.unbornBranch}>
            {(name) => (
              <div
                style={{
                  padding: rowPad(),
                  "font-family": props.fontFamily,
                  "font-size": `${props.scale.md}px`,
                  color: props.theme.dimFg,
                }}
                title="no commits yet"
              >
                <span style={{ color: props.theme.accent }}>*</span> {name()}{" "}
                <span style={{ "font-size": `${props.scale.xs}px` }}>
                  unborn
                </span>
              </div>
            )}
          </Show>
          <Show when={groups()?.detachedAt}>
            {(oid) => (
              <div
                style={{
                  padding: rowPad(),
                  "font-family": props.fontFamily,
                  "font-size": `${props.scale.md}px`,
                  color: props.theme.warning,
                }}
                title={`detached HEAD at ${oid()}`}
              >
                <span style={{ color: props.theme.accent }}>*</span> HEAD
                detached at {oid().slice(0, 7)}
              </div>
            )}
          </Show>
          <For each={groups()?.local ?? []}>{branchRow}</For>

          <Show when={(groups()?.remote.length ?? 0) > 0}>
            {subHeading("REMOTE", groups()!.remote.length, showRemotes, () =>
              setShowRemotes((v) => !v),
            )}
            <Show when={showRemotes()}>
              <For each={groups()!.remote}>{branchRow}</For>
            </Show>
          </Show>

          <Show when={(groups()?.tags.length ?? 0) > 0}>
            {subHeading("TAGS", groups()!.tags.length, showTags, () =>
              setShowTags((v) => !v),
            )}
            <Show when={showTags()}>
              <For each={groups()!.tags}>{branchRow}</For>
            </Show>
          </Show>
        </Show>
      </Show>
    </div>
  );
}
