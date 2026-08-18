import { describe, expect, it } from "vitest";
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
  GitStateMirror,
  type GitOid,
  type GitWorktreeRecord,
} from "@blit-sh/core";
import {
  collectBranches,
  worktreeForBranch,
  worktreeRows,
} from "../ide/branchList";

function oid(fill: number): GitOid {
  const out = new Uint8Array(32);
  out.fill(fill, 0, 20);
  return out;
}

/** A mirror with the refs/head/upstreams given, without going through the
 *  wire — these tests are about the reshaping, not the codec (that is pinned
 *  in js/core's git.test.ts). */
function mirror(init: {
  head?: { flags: number; oid: GitOid; name: string };
  refs?: Record<
    string,
    { flags?: number; oid: GitOid; peeled?: GitOid; target?: string }
  >;
  upstreams?: Record<
    string,
    { flags: number; ahead: number; behind: number; upstream: string }
  >;
}): GitStateMirror {
  const m = new GitStateMirror();
  if (init.head) m.head = init.head;
  for (const [name, ref] of Object.entries(init.refs ?? {})) {
    m.refs.set(name, {
      flags: ref.flags ?? 0,
      oid: ref.oid,
      peeled: ref.peeled ?? new Uint8Array(32),
      target: ref.target ?? "",
    });
  }
  for (const [name, u] of Object.entries(init.upstreams ?? {}))
    m.upstreams.set(name, u);
  return m;
}

describe("collectBranches", () => {
  it("groups locals, remotes and tags, and marks HEAD", () => {
    const groups = collectBranches(
      mirror({
        head: { flags: 0, oid: oid(1), name: "refs/heads/feature" },
        refs: {
          "refs/heads/main": { oid: oid(1) },
          "refs/heads/feature": { oid: oid(2) },
          "refs/remotes/origin/main": { oid: oid(1) },
          "refs/tags/v1": { oid: oid(3) },
        },
      }),
    );
    expect(groups.local.map((r) => r.label)).toEqual(["main", "feature"]);
    expect(groups.remote.map((r) => r.label)).toEqual(["origin/main"]);
    expect(groups.tags.map((r) => r.label)).toEqual(["v1"]);
    expect(groups.local.find((r) => r.label === "feature")!.head).toBe(true);
    expect(groups.local.find((r) => r.label === "main")!.head).toBe(false);
  });

  it("sorts main and master first, then case-insensitively", () => {
    const groups = collectBranches(
      mirror({
        refs: {
          "refs/heads/zebra": { oid: oid(1) },
          "refs/heads/Apple": { oid: oid(1) },
          "refs/heads/main": { oid: oid(1) },
          "refs/heads/banana": { oid: oid(1) },
        },
      }),
    );
    // `main` first because it is what a reader looks for; then A/b/z
    // together rather than uppercase-first as a byte sort would give.
    expect(groups.local.map((r) => r.label)).toEqual([
      "main",
      "Apple",
      "banana",
      "zebra",
    ]);
  });

  it("orders tags newest-looking first, numerically", () => {
    const groups = collectBranches(
      mirror({
        refs: {
          "refs/tags/v2": { oid: oid(1) },
          "refs/tags/v10": { oid: oid(1) },
          "refs/tags/v9": { oid: oid(1) },
        },
      }),
    );
    // A plain string sort would put v9 above v2 above v10.
    expect(groups.tags.map((r) => r.label)).toEqual(["v10", "v9", "v2"]);
  });

  it("consumes the remote HEAD symref to mark a default, not as a branch", () => {
    const groups = collectBranches(
      mirror({
        refs: {
          "refs/remotes/origin/HEAD": {
            flags: GIT_REF_SYMBOLIC,
            oid: oid(1),
            target: "refs/remotes/origin/main",
          },
          "refs/remotes/origin/main": { oid: oid(1) },
          "refs/remotes/origin/topic": { oid: oid(2) },
        },
      }),
    );
    // `origin/HEAD` is not a branch anyone can be on, so it gets no row —
    // it names the default instead.
    expect(groups.remote.map((r) => r.label)).toEqual([
      "origin/main",
      "origin/topic",
    ]);
    expect(
      groups.remote.find((r) => r.label === "origin/main")!.isRemoteDefault,
    ).toBe(true);
    expect(
      groups.remote.find((r) => r.label === "origin/topic")!.isRemoteDefault,
    ).toBe(false);
  });

  it("leaves out refs that are not branches or tags", () => {
    const groups = collectBranches(
      mirror({
        refs: {
          "refs/heads/main": { oid: oid(1) },
          "refs/stash": { oid: oid(2) },
          "refs/notes/commits": { oid: oid(3) },
          "refs/bisect/bad": { oid: oid(4) },
          // A gitdir pseudo-ref, streamed only mid-merge.
          MERGE_HEAD: { oid: oid(5) },
        },
      }),
    );
    expect(groups.local.map((r) => r.label)).toEqual(["main"]);
    expect(groups.remote).toEqual([]);
    expect(groups.tags).toEqual([]);
  });

  it("reports a tag's peeled target, not the tag object", () => {
    const groups = collectBranches(
      mirror({
        refs: {
          "refs/tags/v1": {
            flags: GIT_REF_PEELED_VALID,
            oid: oid(0xaa), // the annotated tag object
            peeled: oid(0xbb), // the commit it points at
          },
        },
      }),
    );
    // A pill on the tag object's oid would decorate a commit that does not
    // exist in the log.
    expect(groups.tags[0].oid).toBe("bb".repeat(20));
  });

  it("carries upstream divergence, and distinguishes gone from unpriced", () => {
    const groups = collectBranches(
      mirror({
        refs: {
          "refs/heads/main": { oid: oid(1) },
          "refs/heads/stale": { oid: oid(2) },
          "refs/heads/costly": { oid: oid(3) },
        },
        upstreams: {
          "refs/heads/main": {
            flags: GIT_UPSTREAM_COUNTS_VALID,
            ahead: 2,
            behind: 3,
            upstream: "refs/remotes/origin/main",
          },
          "refs/heads/stale": {
            flags: GIT_UPSTREAM_GONE,
            ahead: 0,
            behind: 0,
            upstream: "refs/remotes/origin/stale",
          },
          "refs/heads/costly": {
            flags: 0,
            ahead: 0,
            behind: 0,
            upstream: "refs/remotes/origin/costly",
          },
        },
      }),
    );
    const by = (label: string) => groups.local.find((r) => r.label === label)!;
    expect(by("main").upstream).toEqual({
      ref: "refs/remotes/origin/main",
      ahead: 2,
      behind: 3,
      gone: false,
      countsValid: true,
    });
    expect(by("stale").upstream!.gone).toBe(true);
    // Counts the server could not afford are NOT zeroes: a panel that
    // rendered "in sync" here would be lying.
    expect(by("costly").upstream!.countsValid).toBe(false);
    expect(by("main").upstream!.countsValid).toBe(true);
  });

  it("surfaces a detached HEAD, which no branch row can carry", () => {
    const groups = collectBranches(
      mirror({
        head: { flags: GIT_HEAD_DETACHED, oid: oid(7), name: "" },
        refs: { "refs/heads/main": { oid: oid(1) } },
      }),
    );
    expect(groups.detachedAt).toBe("07".repeat(20));
    expect(groups.local.every((r) => !r.head)).toBe(true);
  });

  it("names an unborn branch, which has no ref record at all", () => {
    const groups = collectBranches(
      mirror({
        head: {
          flags: GIT_HEAD_UNBORN,
          oid: new Uint8Array(32),
          name: "refs/heads/main",
        },
      }),
    );
    // A fresh `git init`, or `worktree add -b` before its first commit:
    // `git branch --show-current` names it, so the panel must too.
    expect(groups.unbornBranch).toBe("main");
    expect(groups.local).toEqual([]);
  });

  it("is empty, not thrown, without a repo", () => {
    expect(collectBranches(null)).toEqual({
      local: [],
      remote: [],
      tags: [],
      detachedAt: null,
      unbornBranch: null,
    });
  });
});

describe("worktreeRows", () => {
  const tree = (
    over: Partial<Extract<GitWorktreeRecord, { kind: "tree" }>>,
  ): GitWorktreeRecord => ({
    kind: "tree",
    flags: 0,
    oid: oid(1),
    path: "/w/x",
    branch: "refs/heads/x",
    lockReason: "",
    ...over,
  });

  it("labels rows by their last path segment and strips refs/heads/", () => {
    const rows = worktreeRows([
      tree({
        path: "/src/blit",
        branch: "refs/heads/main",
        flags: GIT_WORKTREE_MAIN,
      }),
      tree({
        path: "/src/blit/.claude/worktrees/epic",
        branch: "refs/heads/topic",
      }),
    ]);
    expect(rows.map((r) => r.name)).toEqual(["blit", "epic"]);
    expect(rows.map((r) => r.branchLabel)).toEqual(["main", "topic"]);
    expect(rows[0].main).toBe(true);
  });

  it("keeps the server's order across refetches", () => {
    // Stable order matters more than a prettier one: rows that reshuffle
    // under a click are worse than rows in a fixed arbitrary order.
    const records = [
      tree({ path: "/w/b" }),
      tree({ path: "/w/a" }),
      tree({ path: "/w/c" }),
    ];
    expect(worktreeRows(records).map((r) => r.name)).toEqual(["b", "a", "c"]);
  });

  it("decodes every flag, and labels a bare worktree", () => {
    const [current, locked, prunable, detached, bare] = worktreeRows([
      tree({ flags: GIT_WORKTREE_CURRENT }),
      tree({ flags: GIT_WORKTREE_LOCKED, lockReason: "on usb" }),
      tree({ flags: GIT_WORKTREE_PRUNABLE }),
      tree({ flags: GIT_WORKTREE_DETACHED, branch: "" }),
      tree({ flags: GIT_WORKTREE_MAIN | GIT_WORKTREE_BARE, path: "" }),
    ]);
    expect(current.current).toBe(true);
    expect([locked.locked, locked.lockReason]).toEqual([true, "on usb"]);
    expect(prunable.prunable).toBe(true);
    expect([detached.detached, detached.branchLabel]).toEqual([true, ""]);
    // A bare repo has no checkout, so there is no segment to name it after.
    expect([bare.bare, bare.name]).toEqual([true, "(bare)"]);
  });

  it("drops the trailing cursor of a truncated page", () => {
    const rows = worktreeRows([
      tree({ path: "/w/a" }),
      { kind: "cursor", after: "", pos: 256n },
    ]);
    expect(rows.map((r) => r.name)).toEqual(["a"]);
  });
});

describe("worktreeForBranch", () => {
  it("finds the worktree holding a branch, so the panel can say so", () => {
    const rows = worktreeRows([
      {
        kind: "tree",
        flags: GIT_WORKTREE_MAIN | GIT_WORKTREE_CURRENT,
        oid: oid(1),
        path: "/src/blit",
        branch: "refs/heads/main",
        lockReason: "",
      },
      {
        kind: "tree",
        flags: 0,
        oid: oid(2),
        path: "/src/wt/topic",
        branch: "refs/heads/topic",
        lockReason: "",
      },
    ]);
    expect(worktreeForBranch(rows, "refs/heads/topic")!.name).toBe("topic");
    expect(worktreeForBranch(rows, "refs/heads/nope")).toBeNull();
    // A detached worktree claims no branch, so it never matches one.
    expect(worktreeForBranch(rows, "")).toBeNull();
  });
});
