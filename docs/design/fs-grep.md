# RFC: Project-Wide Content Search

- **Status:** Implemented (rides `FEATURE_FS`, protocol feature bit 6; no new
  bit, per the family precedent in [fs-search.md](fs-search.md))
- **Date:** 2026-07-28
- **Companion to:** [fs-search.md](fs-search.md), [fs-watch.md](fs-watch.md)

## Summary

`FS_SEARCH` and `FS_INDEX` find files by _name_. Nothing finds them by
_content_: the only grep in the tree is `blit terminal grep`, which
searches PTY backlog, treating each terminal as a file. This adds the
missing half — one message, server-side, that walks a root and returns
matching lines.

Server-side rather than client-side by construction. The `@` index works
because a path list is small enough to ship once and score locally; file
_contents_ are not, so the walk and the match stay where the bytes are
and only hits cross the wire.

Two switches, both on the request: **case-sensitive or not**, and
**regex or literal**. Semantics match `crates/cli/src/grep.rs` so the
CLI and the UI agree: literal mode is `regex::escape` of the query, and
case-insensitivity is `RegexBuilder::case_insensitive`. Both use the
Rust `regex` crate — RE2-style, the same engine ripgrep defaults to, so
lookaround and backreferences are compile errors rather than silent
mismatches.

## `.gitignore` filters by default, and ranks when told not to

`FS_INDEX` prunes ignored files, and by default so does this: on a repo
with build output, the ignored half of the tree is the entire cost.
Measured on a 56 GB checkout, the same query is **11 ms** with ignore
rules and **5.0 s** without — and `rg` shows the same split (16 ms vs
5.6 s), so this is the shape of the problem, not an implementation
artifact.

But the reason to grep is often precisely to find the thing that is _not_
where you expected, including in generated output or a vendored tree. So
`NO_IGNORE` widens the search rather than being unavailable — and when it
does, ignore rules **rank instead of exclude**: matches in tracked files
come first, matches in ignored files after, each flagged so a client can
dim them.

The walk is two-phase. The tracked pass leaves the walker's standard
filters on, so it never descends into `target/` at all; the ignored pass
turns them off and skips what the first already covered, by set
membership rather than a per-path ignore matcher (`IncrementalIgnore`
documents itself as too slow to drive a traversal). Running them in order
makes the ordering fall out of the traversal instead of a sort, and means
a response that fills up drops the part you care about least.

`.git` is still pruned outright. It is an object database, not source;
its contents are compressed binary that no textual query can usefully
match, and on a large repo it dwarfs the tree.

Two filters remain, both about _matchability_ rather than relevance:

- **Binary files are skipped** — a NUL byte in the first 8 KiB. Matching
  a compiled artifact yields unreadable lines and burns budget.
- **Very large files are skipped** past `FS_GREP_MAX_FILE` (4 MiB), the
  same posture the editor takes on its own buffers.

Neither sets `TRUNCATED`. They are _scope_ rules, exactly like pruning
`.git` — a file that cannot usefully match was never a result to clip.
Conflating the two was the first version's mistake: with `target/`
unpruned, any real repo has thousands of files over the size cap, so
every search reported itself as incomplete and the one signal that
should mean "there is more to find" became noise.

## Wire

No new feature bit — `FEATURE_FS` covers the whole `0x40` block. A
server that predates this drops the unknown opcode, so the client's
promise never resolves; the client applies its own timeout and reports
the root as unsearchable rather than hanging a panel.

| Dir | Opcode | Name      | Layout                                                                                        |
| --- | ------ | --------- | --------------------------------------------------------------------------------------------- |
| C2S | `0x48` | `FS_GREP` | `[nonce:2][flags:1][max_matches:2][max_per_file:2][root_len:2][root:N][query_len:2][query:M]` |
| S2C | `0x47` | `FS_GREP` | `[nonce:2][status:1][flags:1][detail_len:2][detail:N][records:LZ4]`                           |

All integers little-endian; the 16 MiB frame limit and
[protocol.md](../protocol.md) framing apply (`S2C_FRAGMENT` splits a
large response transparently).

### Request

`root` is an absolute server path, plain UTF-8 — a client-chosen
filesystem location, as in `FS_SYNC` and `FS_INDEX`, not the fs family's
escaped wire form.

`query` is the pattern. Empty answers `INVALID`: an empty regex matches
every line of every file and is never what a user meant.

`max_matches` and `max_per_file` are optional caps for a client that
wants a preview rather than an answer. **Zero means unlimited, and
unlimited is the default** — the response is bounded by what the wire can
carry, not by a count someone guessed. A search that says "3 results"
when there are four is worse than a slow one.

C2S `flags`:

| Bit | Name             | Meaning                                                                                                               |
| --- | ---------------- | --------------------------------------------------------------------------------------------------------------------- |
| 0   | `CASE_SENSITIVE` | Match case exactly. Default (unset) is insensitive.                                                                   |
| 1   | `REGEX`          | `query` is a regex. Default (unset) is literal.                                                                       |
| 2   | `NO_IGNORE`      | Search gitignored files too, ranked last. Default (unset) applies ignore rules.                                       |
| 3   | `WORD`           | Match whole words: the pattern is wrapped in `\b(?:…)\b` after any literal escaping, so it composes with either mode. |

Bits 4–7 are reserved and must be zero; nonzero answers `INVALID`.
Deliberately _not_ smart-case: the CLI has it because a shell has no
room for a checkbox, but a UI with two toggles should do exactly what
the toggles say.

### Response

`detail` is a human-readable failure reason, empty on success. It earns
its place on `INVALID`: a regex that fails to compile returns the
engine's own message ("unclosed character class"), which is the only
useful thing to show someone mid-typing.

S2C `flags` bit 0 `TRUNCATED`: a budget clipped the search — matches
exist that are not in this response. Exact, as in `FS_INDEX`: set only
when something was actually dropped, so an exactly-at-budget result
reads as complete.

`records` is LZ4 of a `[record_len:4][kind:1][…]` stream, the framing
[lsp.md](lsp.md) uses. Unknown kinds are skipped; a record whose decoder
overruns its body ends the payload.

```text
FILE  0x01: [kind:1][flags:1][n:2][path_len:2][path:N]
            The following `n` MATCH records belong to this file.
            flags bit 0 IGNORED — the file is gitignored; it sorts after
            every non-ignored file and a client may dim it.

MATCH 0x02: [kind:1][line:4][col:4][end_line:4][end_col:4][text_len:4][text:N]
            0-based lines, UTF-8 byte columns — the same shape as an LSP
            range. `end_line` differs from `line` when the pattern matched
            across a newline (a regex containing `\n`); `text` then carries
            every line the match spans, joined by `\n`, so a client can
            show the whole match rather than its first line. The cap
            scales with the span: FS_GREP_MAX_LINE per line, hard ceiling
            8 KiB.

            One record per *match*, not per line: a line containing the
            query twice yields two records with the same `line` and
            `text` but different columns, so a client can put the cursor
            on the hit the user actually clicked.
```

Positions are 0-based lines with UTF-8 byte columns — the convention
[lsp.md](lsp.md) already uses, so a grep hit and a diagnostic reveal
through identical client code.

Line text is carried, not just coordinates, because the alternative is a
fetch per hit to render a result list. Lines past `FS_GREP_MAX_LINE`
(512 bytes) are truncated on a UTF-8 boundary; a minified bundle should
cost one line of wire, not one line of megabyte.

Paths are root-relative lossy UTF-8 of the on-disk names, matching
`FS_SEARCH`/`FS_INDEX` — these feed result lists that reopen through
absolute-path joins, never through `resolve_wire_path`.

`status` uses the
[common status registry](../protocol.md#common-status-registry): `0 OK`,
`2 NOT_FOUND` (root missing), `3 WRONG_TYPE` (root is not a directory),
`4 PERMISSION` (unreadable root, caught by an explicit `read_dir` probe
rather than swallowed into an authoritative-looking empty `OK`, as
`FS_INDEX` does), `6 BUDGET` (in-flight cap), `7 INVALID` (reserved
flags, empty query, bad regex, duplicate nonce), `9 OTHER`. Exactly one
response per nonce in every outcome.

## Budgets

There is deliberately no match budget. The only thing allowed to stop a
search early is running out of wire:

| Knob                  | Value     | Sets `TRUNCATED`                         |
| --------------------- | --------- | ---------------------------------------- |
| Records per response  | 48 MiB    | yes                                      |
| Matches per file      | 65 535    | yes                                      |
| Files opened per walk | 1 000 000 | yes                                      |
| Largest file read     | 64 MiB    | no — out of scope                        |
| Binary sniff          | 8 KiB     | no — out of scope                        |
| Longest line returned | 512 B     | no — the line is clipped, not the result |
| Greps in flight       | 2         | answers `BUDGET`                         |

48 MiB of records is the protocol's LZ4 decompression cap (64 MiB) with
the headroom `FS_INDEX` already leaves. At realistic line lengths that is
six figures of matches — effectively unlimited for a human, and honest
when it does trip. It is charged **as matches are found**, not once per
file: a pattern matching most of a large file would otherwise build its
whole match list before a between-files check looked at it, and the
pattern comes from the client.

65 535 matches per file is the `n` field's own ceiling, not a guess. Past
it a FILE record could not state its count truthfully, so `max_per_file`
saturates there and "unlimited" means exactly this.

What makes an unpruned walk affordable without a byte budget is that the
expensive files are rejected _cheaply_: a size check is a stat the walk
already did, and a binary check reads 8 KiB rather than the whole file.
So the cost scales with the number of files, not with the 56 GB of build
output that `.gitignore`-as-ranking leaves in the tree.

`TRUNCATED` is exact, as in `FS_INDEX`: set only when a match that exists
is missing from the response.

## Client behavior

`@blit-sh/core` exposes `grep(root, query, opts)` beside `searchFiles`
and `indexFiles`. The UI drives it from a left-dock panel with the two
toggles, debounced per keystroke and cancelled on the next one — the
same shape the switcher's `#symbol` mode uses, because like an LSP
symbol query and unlike the `@` index, every keystroke is a round trip.

Results group by file, tracked files first, and a row reveals its line
through the existing `setReveal` + `editorAssignment` path, so a grep
hit opens exactly the way a diagnostic or a definition does.

## Security

Request validation (reserved flags, empty query, uncompilable regex,
duplicate nonce) answers `INVALID` before any I/O. The root is any path
the server user can read — the family's posture
([fs-watch.md](fs-watch.md) § Security), unchanged.

Not filtering by `.gitignore` widens what a search can _read_ relative
to `FS_INDEX`, but not relative to the family: `FS_SYNC` and `FS_FILE`
already serve any readable path, so an ignored file was never a
protected one. Worth stating explicitly all the same, because it means a
grep can surface the contents of `.env` files that the file picker hides
— which is the intended behavior for a tool searching your own machine,
and a reason not to point a blit server at a tree you would not `cat`.

## Rollout

1. `crates/remote` opcodes + codecs, TypeScript mirror in
   `@blit-sh/core`, byte fixtures both sides. ✅
2. Server walk (two-phase, `ignore`-crate based) + dispatch + budgets. ✅
3. `js/core` `grep()`; `js/ui` search panel, toggles, result list. ✅
4. Deferred, with triggers: streaming partial results as the walk
   progresses (trigger: a cold large-tree search feels unresponsive
   before the single response lands); a replace-across-files mutation
   (trigger: demand — and it wants the CAS discipline
   [fs-write.md](fs-write.md) already defines, not a new one); reusing a
   warm `FS_SYNC` mirror to skip re-walking a root already being watched
   (trigger: measured duplicate walk cost).
