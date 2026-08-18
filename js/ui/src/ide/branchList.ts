/**
 * The Branches panel's data, derived from what the repo already streams.
 *
 * Branches cost nothing new: `GIT_STATE` already pushes every ref, HEAD, and
 * (with the `TRACKING` open flag) per-branch ahead/behind, so this is a pure
 * reshaping of the mirror rather than another request. Worktrees do come from
 * a request (`GIT_WORKTREES`), and this module only turns its records into
 * rows — the refetching lives in the session, keyed on the state stream's
 * worktree generation.
 *
 * Kept apart from the component so both derivations are testable without a
 * DOM, which is the only way to pin the ordering rules below.
 */

import {
  GIT_HEAD_DETACHED,
  GIT_HEAD_UNBORN,
  GIT_REF_PEELED_VALID,
  GIT_REF_SYMBOLIC,
  GIT_UPSTREAM_COUNTS_VALID,
  GIT_UPSTREAM_GONE,
  GIT_WORKTREE_BARE,
  GIT_WORKTREE_CURRENT,
  GIT_WORKTREE_DETACHED,
  GIT_WORKTREE_LOCKED,
  GIT_WORKTREE_MAIN,
  GIT_WORKTREE_PRUNABLE,
  gitOidHex,
  type GitStateMirror,
  type GitWorktreeRecord,
} from "@blit-sh/core";

export type BranchKind = "local" | "remote" | "tag";

export interface BranchRow {
  /** Full ref name — the identity, and what a log spec is set to. */
  ref: string;
  /** Display label: `main`, `origin/main`, `v1`. */
  label: string;
  kind: BranchKind;
  /** Hex of the commit this row points at (a tag's peeled target). */
  oid: string;
  /** HEAD is on this branch. */
  head: boolean;
  /** The remote's default branch, from its `HEAD` symref — so the panel can
   *  say which one that is instead of guessing at `main` or `master`. */
  isRemoteDefault: boolean;
  /** Local branches with an upstream configured, when the repo was opened
   *  with tracking. `countsValid` false means the counts were not
   *  affordable, which reads differently from "0 ahead, 0 behind". */
  upstream?: {
    ref: string;
    ahead: number;
    behind: number;
    gone: boolean;
    countsValid: boolean;
  };
}

export interface BranchGroups {
  local: BranchRow[];
  remote: BranchRow[];
  tags: BranchRow[];
  /** Detached HEAD: no local row is `head`, and this is where it sits. */
  detachedAt: string | null;
  /** An unborn branch (`git init`, or `worktree add -b` before its first
   *  commit) — named, but on no commit, so it has no ref record at all. */
  unbornBranch: string | null;
}

/** Local-branch ref name ordering: `main` and `master` first because that is
 *  what a reader looks for, then case-insensitive alphabetical. Slashes sort
 *  naturally, so `feature/a` and `feature/b` stay adjacent. */
function compareLocal(a: BranchRow, b: BranchRow): number {
  const rank = (row: BranchRow) =>
    row.label === "main" || row.label === "master" ? 0 : 1;
  return (
    rank(a) - rank(b) ||
    a.label.localeCompare(b.label, undefined, { sensitivity: "base" })
  );
}

/**
 * Group the mirror's refs into the panel's three lists.
 *
 * Skips everything that is not a branch or tag a reader would recognize:
 * `refs/stash`, notes, `refs/bisect/*`, the gitdir pseudo-refs the server
 * streams during a merge or rebase, and `refs/remotes/*​/HEAD` — that last
 * one is a symref, not a branch anyone can be on, so it is consumed to mark
 * the remote's default rather than listed as a fourth `origin/HEAD` row.
 */
export function collectBranches(
  gs: GitStateMirror | null,
  oidFormat?: number,
): BranchGroups {
  const groups: BranchGroups = {
    local: [],
    remote: [],
    tags: [],
    detachedAt: null,
    unbornBranch: null,
  };
  if (!gs) return groups;

  const head = gs.head;
  const headBranch =
    head && !(head.flags & (GIT_HEAD_DETACHED | GIT_HEAD_UNBORN))
      ? head.name
      : null;
  if (head && head.flags & GIT_HEAD_DETACHED) {
    groups.detachedAt = gitOidHex(head.oid, oidFormat);
  }
  if (head && head.flags & GIT_HEAD_UNBORN) {
    groups.unbornBranch = head.name.replace(/^refs\/heads\//, "");
  }

  // Remote default branches, keyed by remote name, from the HEAD symrefs.
  const remoteDefaults = new Map<string, string>();
  for (const [name, ref] of gs.refs) {
    const m = /^refs\/remotes\/([^/]+)\/HEAD$/.exec(name);
    if (m && ref.flags & GIT_REF_SYMBOLIC && ref.target) {
      remoteDefaults.set(m[1], ref.target);
    }
  }

  for (const [ref, state] of gs.refs) {
    // A tag decorates what it peels to; a branch points at its own oid.
    const target =
      state.flags & GIT_REF_PEELED_VALID ? state.peeled : state.oid;
    const oid = gitOidHex(target, oidFormat);
    let label: string;
    if ((label = ref.replace(/^refs\/heads\//, "")) !== ref) {
      const tracking = gs.upstreams.get(ref);
      groups.local.push({
        ref,
        label,
        kind: "local",
        oid,
        head: ref === headBranch,
        isRemoteDefault: false,
        upstream: tracking && {
          ref: tracking.upstream,
          ahead: tracking.ahead,
          behind: tracking.behind,
          gone: (tracking.flags & GIT_UPSTREAM_GONE) !== 0,
          countsValid: (tracking.flags & GIT_UPSTREAM_COUNTS_VALID) !== 0,
        },
      });
    } else if ((label = ref.replace(/^refs\/tags\//, "")) !== ref) {
      groups.tags.push({
        ref,
        label,
        kind: "tag",
        oid,
        head: false,
        isRemoteDefault: false,
      });
    } else if ((label = ref.replace(/^refs\/remotes\//, "")) !== ref) {
      // The `<remote>/HEAD` symref is not a branch; it named the default
      // above and has no row of its own.
      if (/^[^/]+\/HEAD$/.test(label)) continue;
      groups.remote.push({
        ref,
        label,
        kind: "remote",
        oid,
        head: false,
        isRemoteDefault: [...remoteDefaults.values()].includes(ref),
      });
    }
    // Everything else (refs/stash, refs/notes, refs/bisect, pseudo-refs)
    // is not a branch or tag and stays out.
  }

  groups.local.sort(compareLocal);
  const byLabel = (a: BranchRow, b: BranchRow) =>
    a.label.localeCompare(b.label, undefined, { sensitivity: "base" });
  groups.remote.sort(byLabel);
  // Tags newest-looking first: a version-aware compare, descending, so `v10`
  // sits above `v9` instead of below it as a plain string sort would put it.
  groups.tags.sort((a, b) =>
    b.label.localeCompare(a.label, undefined, {
      numeric: true,
      sensitivity: "base",
    }),
  );
  return groups;
}

export interface WorktreeRow {
  /** Absolute path on the server; "" for a bare main worktree. */
  path: string;
  /** Last path segment, the panel's label. */
  name: string;
  /** Full ref name HEAD is on; "" when detached. */
  branch: string;
  /** `branch` without `refs/heads/`, for display. */
  branchLabel: string;
  oid: string;
  main: boolean;
  /** The worktree this session's repo handle is open at. */
  current: boolean;
  locked: boolean;
  lockReason: string;
  /** The checkout is gone from disk — nothing to navigate to. */
  prunable: boolean;
  detached: boolean;
  bare: boolean;
}

/**
 * Turn a `GIT_WORKTREES` reply into rows, dropping the trailing `CURSOR`.
 *
 * Server order is kept: the main worktree first, then the linked ones by
 * their administrative path. That is stable across refetches, which matters
 * more here than any prettier ordering — rows that reshuffle under a click
 * are worse than rows in an arbitrary but fixed order.
 */
export function worktreeRows(records: GitWorktreeRecord[]): WorktreeRow[] {
  const rows: WorktreeRow[] = [];
  for (const record of records) {
    if (record.kind !== "tree") continue;
    const bare = (record.flags & GIT_WORKTREE_BARE) !== 0;
    const segments = record.path.split("/").filter(Boolean);
    rows.push({
      path: record.path,
      name: bare ? "(bare)" : (segments[segments.length - 1] ?? record.path),
      branch: record.branch,
      branchLabel: record.branch.replace(/^refs\/heads\//, ""),
      oid: gitOidHex(record.oid),
      main: (record.flags & GIT_WORKTREE_MAIN) !== 0,
      current: (record.flags & GIT_WORKTREE_CURRENT) !== 0,
      locked: (record.flags & GIT_WORKTREE_LOCKED) !== 0,
      lockReason: record.lockReason,
      prunable: (record.flags & GIT_WORKTREE_PRUNABLE) !== 0,
      detached: (record.flags & GIT_WORKTREE_DETACHED) !== 0,
      bare,
    });
  }
  return rows;
}

/** Which worktree, if any, has `ref` checked out — a branch checked out
 *  elsewhere cannot be checked out here, which is the thing a reader most
 *  wants marked on a branch row. */
export function worktreeForBranch(
  rows: WorktreeRow[],
  ref: string,
): WorktreeRow | null {
  return rows.find((row) => row.branch === ref) ?? null;
}
