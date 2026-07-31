import { describe, expect, it, vi, afterEach } from "vitest";
import type { GitPatchRecord } from "@blit-sh/core";
import {
  cachedCommitRows,
  commitCacheKey,
  dropCachedCommits,
  getCachedCommit,
  putCachedCommit,
  type CachedCommit,
} from "../ide/commitCache";
import { acquireRepo } from "../ide/repoRegistry";

const info = {
  short: "abc1234",
  message: "subject\n\nbody",
  author: "A",
  email: "a@example.com",
  time: 0n,
  committer: "A",
  committerEmail: "a@example.com",
  committerTime: 0n,
  parents: [],
};

/** A commit of `rows` patch rows across one file. */
const entry = (rows: number): CachedCommit => ({
  commit: info,
  files: [
    {
      newPath: "a.rs",
      oldPath: "a.rs",
      rows: Array.from({ length: rows }, () => ({}) as GitPatchRecord),
    },
  ],
});

describe("commit cache", () => {
  afterEach(() => {
    dropCachedCommits("c1");
    dropCachedCommits("c2");
  });

  it("returns a commit a remounted tile already loaded", () => {
    // The bug: moving a commit tile to the dock and back unmounts and remounts
    // it, and the load effect refetched a log page and the whole patch.
    const key = commitCacheKey("c1", "/w", "deadbeef");
    expect(getCachedCommit(key)).toBeUndefined();
    putCachedCommit(key, entry(3));
    expect(getCachedCommit(key)?.files[0].rows).toHaveLength(3);
  });

  it("keys by connection and repo, not by oid alone", () => {
    // The same oid can exist in two checkouts on two boxes, and the patch is
    // only the same by coincidence of content.
    putCachedCommit(commitCacheKey("c1", "/w", "beef"), entry(1));
    expect(getCachedCommit(commitCacheKey("c2", "/w", "beef"))).toBeUndefined();
    expect(getCachedCommit(commitCacheKey("c1", "/o", "beef"))).toBeUndefined();
  });

  it("evicts least-recently-used once over the row budget", () => {
    // 200k rows is the budget; three 90k-row commits do not fit.
    const a = commitCacheKey("c1", "/w", "a");
    const b = commitCacheKey("c1", "/w", "b");
    const c = commitCacheKey("c1", "/w", "c");
    putCachedCommit(a, entry(90_000));
    putCachedCommit(b, entry(90_000));
    // Touch `a` so `b` is the least recently used…
    expect(getCachedCommit(a)).toBeDefined();
    putCachedCommit(c, entry(90_000));
    expect(getCachedCommit(b)).toBeUndefined();
    expect(getCachedCommit(a)).toBeDefined();
    expect(getCachedCommit(c)).toBeDefined();
    expect(cachedCommitRows()).toBe(180_000);
  });

  it("keeps a commit larger than the whole budget", () => {
    // It is the one being displayed; dropping it on insert would mean the view
    // never caches anything at all.
    const big = commitCacheKey("c1", "/w", "big");
    putCachedCommit(big, entry(400_000));
    expect(getCachedCommit(big)).toBeDefined();
  });

  it("drops a connection's commits when it goes away", () => {
    putCachedCommit(commitCacheKey("c1", "/w", "x"), entry(5));
    putCachedCommit(commitCacheKey("c2", "/w", "x"), entry(5));
    dropCachedCommits("c1");
    expect(getCachedCommit(commitCacheKey("c1", "/w", "x"))).toBeUndefined();
    expect(getCachedCommit(commitCacheKey("c2", "/w", "x"))).toBeDefined();
    expect(cachedCommitRows()).toBe(5);
  });
});

describe("shared repo registry lingers briefly", () => {
  const workspace = (opened: { n: number }) =>
    ({
      openRepo: async () => {
        opened.n++;
        return {
          workdir: "/w",
          close: () => {},
        };
      },
    }) as never;

  it("hands a released repo back without reopening it", async () => {
    vi.useFakeTimers();
    const opened = { n: 0 };
    const ws = workspace(opened);
    const first = await acquireRepo(ws, "c1", "/w/src");
    first.close();
    // The move: the old tile released, the new one acquires a moment later.
    vi.advanceTimersByTime(50);
    const second = await acquireRepo(ws, "c1", "/w/src");
    expect(opened.n).toBe(1);
    second.close();
    vi.useRealTimers();
  });

  it("closes it once the linger elapses with no consumer", async () => {
    vi.useFakeTimers();
    const opened = { n: 0 };
    const ws = workspace(opened);
    const first = await acquireRepo(ws, "c1", "/w/src");
    first.close();
    vi.advanceTimersByTime(30_000);
    await acquireRepo(ws, "c1", "/w/src");
    expect(opened.n).toBe(2);
    vi.useRealTimers();
  });
});
