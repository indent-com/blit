# RFC: File Search and the Client-Side Index

- **Status:** Implemented (rides `FEATURE_FS`, protocol feature bit 6; no
  new bit). Also retro-documents `FS_SEARCH`, which shipped undocumented.
- **Date:** 2026-07-27
- **Companion to:** [fs-watch.md](fs-watch.md), [fs-write.md](fs-write.md)

## Summary

The switcher's `@query` mode wants a result list that updates per
keystroke. Two messages serve it, both root-path-based one-shots with
**no sync** — like `FS_SEARCH` always was, they take a root path rather
than a `sync_id`, so a search needs no `FS_SYNC` first:

- `FS_SEARCH` — server-side: walk, fuzzy-score, return the top matches.
  One round trip per query. Kept on the wire for API consumers and older
  clients; the shipped UI no longer calls it.
- `FS_INDEX` — client-side: the server ships the whole candidate list
  once, LZ4-compressed; the client scores every keystroke locally with
  zero round trips, caches per (connection, root), and re-pulls on a
  staleness TTL. The fast path, and the only way `@` can also rank
  recently-opened files first (recency lives client-side).

Both walk the same candidate set: **gitignore-filtered** (the `ignore`
crate — `.gitignore`, global and repo excludes, applied only inside a
git repository), `.git` always pruned, other dotfiles included, symlinks
not followed, regular files only. Ignore filtering is what makes the
list shippable at all: it is the difference between a repo's source tree
and its `target/`.

## Wire

No new feature bit — the family precedent ([fs-write.md](fs-write.md)):
`FEATURE_FS` covers the whole `0x40` block. A server that predates
`FS_INDEX` silently drops the unknown opcode, so the client's index
promise simply never resolves and `@` stays empty until the server
upgrades — refusal needs no advertisement. Gateway, proxy, and mux
forward all four messages unmodified.

| Dir | Opcode | Name        | Layout                                                            |
| --- | ------ | ----------- | ----------------------------------------------------------------- |
| C2S | `0x46` | `FS_SEARCH` | `[nonce:2][limit:2][root_len:2][root:N][query_len:2][query:M]`    |
| S2C | `0x45` | `FS_SEARCH` | `[nonce:2][status:1][count:2]` repeated`{ [path_len:2][path:N] }` |
| C2S | `0x47` | `FS_INDEX`  | `[nonce:2][flags:1][root_len:2][root:N]`                          |
| S2C | `0x46` | `FS_INDEX`  | `[nonce:2][status:1][flags:1][count:4][paths:LZ4]`                |

All integers little-endian; 16 MiB frame limit and
[protocol.md](../protocol.md) framing apply (`S2C_FRAGMENT` splits the
large `FS_INDEX` responses transparently).

### `FS_SEARCH`

`root` is an absolute server path; results are root-relative, best match
first, at most `limit`. Scoring is a case-insensitive subsequence match
over the whole relative path: every query character must appear in
order; contiguous runs, matches inside the basename, and shorter paths
score higher. `status` is the grandfathered `FS_SYNCED` table: `0` in
practice — an unreadable root walks nothing and returns zero paths —
except `3 RESOURCE_LIMIT` when the in-flight walk cap (§ Budgets)
refuses the request (the table has no `INVALID`, so a duplicate
in-flight nonce answers the same). Documented as-is rather than blessed:
a new message would use the unified table below.

### `FS_INDEX`

`C2S.flags` is reserved and must be `0`; nonzero answers `INVALID`. The
response's decompressed payload is repeated `[path_len:2][path:N]` —
root-relative paths, sorted, which is what lets prefix-heavy trees
compress well. `count` restates the record count as a cross-check; a
disagreeing payload — or a count over the protocol cap of 1 000 000
(`FS_INDEX_MAX_COUNT`) — is malformed: the decompression guard bounds
bytes, and the count cap keeps a hostile claim of tiny records from
forcing a giant preallocation. `S2C.flags` bit 0 `TRUNCATED`: a budget
clipped the walk, the list is a prefix of the tree, and clients should
keep using `FS_SEARCH` for that root rather than trust the partial
list — or, as the shipped UI does, serve the prefix best-effort.
Truncation is exact — it is only set when a file (or bounded work,
below) was actually dropped, so an exactly-at-budget tree reads as
complete.

Two walk edge cases are handled away from the naive shape: an
unreadable root (canonicalize succeeds on a mode-000 directory; the
walker would swallow the `EACCES`) is caught by a `read_dir` probe and
answers `PERMISSION` rather than an authoritative-looking empty `OK`;
and a tree whose _filtered_ walk comes back empty — a parent
`.gitignore` with a bare `*`, the dotfiles-repo-at-`$HOME` pattern,
blanks every non-repo subtree — retries without ignore rules, so `@`
never goes silently blind on a real tree.

`FS_INDEX.status` is the unified git/lsp table
([git.md](git.md) "Statuses"), not `FS_SYNCED`'s grandfathered `0`–`4`:
`0 OK`, `2 NOT_FOUND` (root missing), `3 WRONG_TYPE` (root is not a
directory), `4 PERMISSION`, `6 BUDGET` (in-flight cap), `7 INVALID`
(reserved flags set, duplicate nonce), `9 OTHER`. One response per nonce
in every outcome; a malformed frame that loses the nonce is dropped
(`FS_SEARCH`'s rule).

Paths in both messages are lossy UTF-8 of the on-disk names (matching
`FS_SEARCH`'s shipped behavior), **not** the fs family's escaped wire
form — these lists feed pickers and reopen through absolute-path joins,
never through `resolve_wire_path`.

## Budgets

| Knob                     | Default   | Env                     |
| ------------------------ | --------- | ----------------------- |
| Files per walk           | 400 000   | `BLIT_FS_INDEX_MAX`     |
| Yielded entries per walk | 4 × files | —                       |
| Raw path bytes per list  | 48 MiB    | —                       |
| Index walks in flight    | 8         | `BLIT_FS_WALK_INFLIGHT` |
| Search walks in flight   | 8         | `BLIT_FS_WALK_INFLIGHT` |
| Protocol count cap       | 1 000 000 | —                       |

The byte budget keeps the declared decompressed size well under the
protocol-wide 64 MiB receiver cap; any budget tripping sets
`TRUNCATED`. The yielded-entry budget bounds directory-heavy trees the
file budget can't see; entries the ignore rules suppress _inside_ the
walker cost I/O no budget observes (§ Rollout, deferred). The env knob
clamps to the protocol count cap so a raised budget can't emit an
unparseable count. The in-flight caps bound walk threads per connection;
`FS_SEARCH` predates them but is capped the same way (its own nonce set,
same limit), refusing over-cap requests with `RESOURCE_LIMIT`.

That cap was a bare `2` at three call sites, below what a single IDE session
asks for — the client carries retry-on-`BUDGET` code to absorb it. It is now
`BLIT_FS_WALK_INFLIGHT`, default 8. What it spends is threads: each walk's
file, byte and entry cost is bounded by the budgets above, so raising it does
not raise the ceiling on any one walk.

## Client behavior

`@blit-sh/core` exposes `indexFiles(root)` beside `searchFiles`. The UI
(`js/ui/src/ide/fileIndex.ts`) caches one list per (connection, root),
serves every keystroke from it synchronously, and refreshes it in the
background when a lookup finds it older than 60 s — stale-while-
revalidate, so a fresh file appears on the next switcher open without
ever blocking one. The index is the _only_ `@` path: until the list
lands (or against a pre-`FS_INDEX` server, ever) `@` simply shows
nothing, and a `TRUNCATED` prefix is served best-effort. The client
scorer is a faithful port of the server's, so `FS_SEARCH`-based API
consumers rank identically — minus the recency boost only the local
path can add: files with remembered editor positions rank above cold
matches, so an empty `@` is a most-recently-touched list.

## Security

Request validation (reserved flags, duplicate nonces) answers `INVALID`;
the walk runs off-thread so the connection loop never blocks; teardown
drops the in-flight set with the connection. The root is any path the
server user can read — the family's posture ([fs-watch.md](fs-watch.md)
§ Security), unchanged by these messages.

## Rollout

1. `crates/remote` opcodes + codecs, TypeScript mirror in
   `@blit-sh/core`, byte fixtures both sides. ✅
2. Server walk (`ignore`-crate based, shared by both messages) +
   dispatch + budgets. ✅
3. `js/core` `indexFiles`; `js/ui` cache, local scorer, recency boost,
   switcher wiring with `FS_SEARCH` fallback. ✅
4. Deferred, with triggers: a generation echo for cheap revalidation
   (trigger: re-pull bandwidth shows up in practice on big trees);
   watcher-driven invalidation via the fssync shared-root registry
   (trigger: the TTL demonstrably misses fresh files in real use);
   bounding the I/O of ignore-suppressed entries with a
   custom walker (trigger: a pathological mostly-ignored tree hurts in
   practice); precomputed-lowercase or worker-thread scoring (trigger:
   measured keystroke jank on ≥100k-file indexes); a dirty-input guard
   on the log-spec restore (trigger: a restore observed clobbering
   mid-typing on a slow link).
