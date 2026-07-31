/**
 * Cache of a commit's message and patch, keyed by connection + repo + oid.
 *
 * A commit is immutable: its oid names its message, its author, and its patch
 * against its first parent, for as long as the object exists. The git family
 * is built on that ("oid-addressed, cache forever" — docs/design/git.md), and
 * this is where a tile takes it up.
 *
 * Without it, moving a commit tile to the dock and back refetched a log page
 * and the commit's whole patch over the wire on every move, because the tile
 * unmounts and remounts (BlitTile keys on the assignment string) and its load
 * effect starts from nothing. A commit view is also the one tile whose content
 * cannot go stale, so there is nothing to trade away.
 *
 * Bounded by rows rather than entries: one merge across a generated file can
 * outweigh a hundred ordinary commits, so counting commits would bound the
 * wrong thing. Eviction is least-recently-used, since a user paging through
 * history revisits what they just looked at.
 */

import type { GitPatchRecord } from "@blit-sh/core";

export interface CommitInfo {
  short: string;
  message: string;
  author: string;
  email: string;
  /** Author time (git log's convention for the header line). */
  time: bigint;
  committer: string;
  committerEmail: string;
  committerTime: bigint;
  /** Full hex oids of this commit's parents — two or more for a merge,
   *  none for a root commit. Each opens as its own commit tile. */
  parents: string[];
}

export interface FileDiff {
  newPath: string;
  oldPath: string;
  rows: GitPatchRecord[];
}

export interface CachedCommit {
  commit: CommitInfo;
  files: FileDiff[];
}

/** Patch rows held across all cached commits before the oldest is dropped.
 *  ~200k rows is a few tens of MB of records and covers any realistic
 *  browsing session; the cap only stops an unbounded walk of history. */
const ROW_BUDGET = 200_000;

const cache = new Map<string, CachedCommit>();
let rows = 0;

function rowCount(entry: CachedCommit): number {
  let n = 0;
  for (const file of entry.files) n += file.rows.length;
  return n;
}

export function commitCacheKey(
  connectionId: string,
  repoPath: string,
  oid: string,
): string {
  // NUL-separated, like every other composite key here: a repo path can
  // contain anything a filesystem allows except this byte.
  return `${connectionId}\0${repoPath}\0${oid}`;
}

/** A cached commit, promoted to most-recently-used, or undefined. */
export function getCachedCommit(key: string): CachedCommit | undefined {
  const hit = cache.get(key);
  if (!hit) return undefined;
  // Map preserves insertion order, so re-inserting is the LRU bump.
  cache.delete(key);
  cache.set(key, hit);
  return hit;
}

export function putCachedCommit(key: string, entry: CachedCommit): void {
  const existing = cache.get(key);
  if (existing) {
    rows -= rowCount(existing);
    cache.delete(key);
  }
  cache.set(key, entry);
  rows += rowCount(entry);
  for (const [oldest, value] of cache) {
    if (rows <= ROW_BUDGET) break;
    // Never evict the entry just inserted: a single commit larger than the
    // whole budget is still the one being displayed.
    if (oldest === key) break;
    cache.delete(oldest);
    rows -= rowCount(value);
  }
}

/** Drop everything for one connection — its oids stop being addressable when
 *  the connection is gone for good, and a workspace teardown should not leak
 *  patches for a box the user has closed. */
export function dropCachedCommits(connectionId: string): void {
  for (const [key, value] of cache) {
    if (key.startsWith(`${connectionId}\0`)) {
      cache.delete(key);
      rows -= rowCount(value);
    }
  }
}

/** Test seam: the row total the cache is holding. */
export function cachedCommitRows(): number {
  return rows;
}
