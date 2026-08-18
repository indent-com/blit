# One-shot reads (`FS_READ`)

Read a fixed set of files, once, without a sync session.

## Why it exists

The fs family was built for an editor watching a tree: `FS_SYNC` establishes a
session, updates stream, and `FS_FETCH` names a `sync_id`. Everything else it
grew — `FS_SEARCH`, `FS_INDEX`, `FS_GREP` — asks about a root with no session,
because *discovery* has nothing to watch. Reading did not get the same treatment,
so anything wanting a handful of files had two choices: sync a directory it did
not want watched, or spawn `/bin/sh -c 'cat …'`.

The session supervisor took the second one, twice — every `.desktop` file at
startup, and every icon a panel asks for — and the shell brought its own problems
with it: quoting rules for every path, base64 to get bytes back through a text
stream, and `wc -c` to enforce a size limit the protocol should own. `FS_READ` is
the missing read.

It grants nothing new. A client with `FEATURE_FS` can already read any file the
server user can, by syncing its directory and fetching it; this is the same
authority in one message instead of three.

## Wire

```
C2S_FS_READ  [0x4D][nonce:2][flags:1][max_bytes:4][group_count:2]
             then group_count × ( [path_count:2]
                                  then path_count × [path_len:2][path:N] )

S2C_FS_READ  [0x48][nonce:2][status:1][count:2][records:LZ4]
             records = count × [status:1][path_len:2][path:N][size:4][data:size]
```

Paths come in groups, and a group is one question. Without `FS_READ_FIRST` the
groups are read straight through and the grouping does not matter; with it each
group is answered by its own first readable path. Paths total 1 to
`FS_READ_MAX_PATHS` (512) however they are grouped. `max_bytes` is the per-file
ceiling, zero meaning `FS_READ_DEFAULT_BYTES` (1 MiB).

`status` is the common registry (`FS_DONE_*`): `INVALID` for unknown flags,
`BUDGET` when too many reads are already in flight for this connection, `OK`
otherwise — including when every individual path failed, because that is an
answer about those paths rather than a failure of the request.

Each record carries its own `FS_FILE_*`, so one unreadable path does not spoil
the rest:

| Status | Meaning |
| --- | --- |
| `OK` | content follows |
| `NOT_FOUND` | no such path |
| `UNREADABLE` | permission denied, or not a regular file — a directory is `FS_INDEX`'s business |
| `TOO_LARGE` | exists, not read: over `max_bytes`, or over what was left of the reply budget |
| `OTHER` | anything else I/O reported |

A reply carries at most `FS_READ_MAX_TOTAL_BYTES` (8 MiB) of content. Files past
that are `TOO_LARGE` rather than silently dropped, so a caller can re-ask for the
remainder in smaller batches.

### `FS_READ_FIRST`

With `flags` bit 0 set, each group is answered by the first path in it that can
be read; missing, unreadable and oversized paths are stepped over rather than
reported. There is exactly one record per group, in group order, so answers align
with questions by position: a group that matched nothing carries
`FS_FILE_NOT_FOUND` and an empty path.

This is the search-path question — *the first of these that exists, in my order
of preference* — which is otherwise a round trip per candidate. The icon lookup
is exactly this: rank every directory on the icon path once, then ask for
`dir/name.svg`, `dir/name.png`, … in that order and take the first hit. Groups
are what make a screenful of them one message: one group per name, and the reply
carries one record per name.

## `FS_INDEX` flags

The listing side grew two flags for the same callers, on the byte that was
reserved:

- `FS_INDEX_DIRS_ONLY` (bit 0) lists directories instead of files — the shape of
  a tree without its contents, which is what a search path is. An icon theme is
  fifty directories holding fifty thousand files.
- `FS_INDEX_FOLLOW_LINKS` (bit 1) descends through symlinked directories. Off by
  default, because a tree's links can point anywhere. It exists because some
  trees are *made* of links: on a Nix system every directory under
  `/run/current-system/sw/share/icons` is one, and a walk that stops at them
  reports a theme's name and nothing inside it.

Independently of the flags, an entry that is a symlink is now classified by what
it points at rather than skipped — one `stat` per link. Without it a Nix system
`applications` directory indexes as empty, every `.desktop` in it being a link
into the store.

## Not in v1

- **No byte ranges.** Every caller so far wants whole files, and a range needs an
  offset, a length, and a rule for a file that changed under it.
- **No metadata-only mode.** `TOO_LARGE` already answers "how big is it" for the
  only question anyone asked, which is whether it is worth carrying.
- **No directory reads.** That is `FS_INDEX`, and conflating them would make one
  message answer two shapes.
