# `systemd`

A live view of the systemd system and user unit tables, published on the
`blit.systemd.v1` native channel and readable from the CLI as `@systemd`.

```bash
blit ext run --persist --restart always systemd extensions/dist/systemd.wasm
blit @systemd status
blit @systemd list --scope system ssh
blit @systemd watch sshd
blit @systemd logs -u sshd.service -n 50
blit @systemd logs -u sshd.service --follow
```

`logs` is the journal reader from a shell, and the honest oracle for it: `-f`
is the same live stream the panel gets, and `--cursor` walks scrollback a page
at a time. It prints one JSON object per line, as `watch` does.

The browser UI reads the same channel: expand a remote in the Remotes panel and
its **systemd** tab is a filterable unit table over a journal reader
(`js/ui/src/systemd.ts` mirrors the protocol). The tab exists only where this
extension answers, so a server without it shows no such tab rather than an
empty one.

## How it sees the machine

A Wasm guest reaches nothing but Blit packets — no filesystem, no sockets, no
D-Bus. The unit tables therefore come from `systemctl list-units` running as a
native child process through the process family, and the extension diffs
successive snapshots.

`gdbus monitor` on the same child only _pokes_: a signal means "ask again
soon", never "here is the new state". A missed or unbroadcast signal costs
latency, not correctness, and a host without `gdbus` degrades to a one-second
poll rather than to nothing. (systemd only broadcasts unit signals while some
client holds a `Manager.Subscribe()`, which is not guaranteed on any given
box — hence the design.)

Nobody watching means a thirty-second heartbeat; the cost is only paid while
someone is looking. Arguments:

| argument               | default | meaning                             |
| ---------------------- | ------- | ----------------------------------- |
| `--scopes system,user` | both    | which managers to watch             |
| `--interval-ms N`      | 5000    | refresh while someone is subscribed |
| `--idle-interval-ms N` | 30000   | refresh while nobody is             |
| `--debounce-ms N`      | 250     | how long a signal poke is coalesced |
| `--no-signals`         | off     | ignore D-Bus and poll only          |

## Channel protocol

`blit.systemd.v1`, one JSON object per message.

- `{"type":"hello","protocol":"blit.systemd.v1","ts":…,"scopes":[{"scope","source","units"}]}`
- `{"type":"snapshot","scope":…,"ts":…,"chunk":N,"last":bool,"units":[{name,load,active,sub,description}]}`
- `{"type":"change","scope":…,"ts":…,"added":[…],"changed":[{…,"previous":{load,active,sub}}],"removed":["unit"]}`

Client to server, one bare text line per message: `resync`, `filter PREFIX`,
`scopes system,user`, `ping`.

A subscriber that stops reading is waited for, then dropped: what will not fit
in the peer's window is queued in order, and the ACK that returns credit drains
it. A peer that lets 8 MiB pile up is closed rather than grown into. Queueing
rather than discarding is what lets a chunked answer — a snapshot, a journal
page — commit to reaching its `last` marker instead of stopping mid-stream and
leaving the reader waiting for a message that is never coming.

## Journal queries

The same channel answers paged journal queries. A journal is far too large to
mirror, so unlike the unit table these are request/response, correlated by
`id`, chunked, and anchored by journald's own cursors rather than an offset
that would drift as entries arrive.

```json
{"type":"logs","id":"7","scope":"system","unit":"sshd.service","boot":"…",
 "priority":"4","grep":"denied","cursor":"s=…","direction":"backward","limit":200}
{"type":"boots","id":"8"}
{"type":"follow","id":"9","scope":"system","unit":"sshd.service","cursor":"s=…"}
{"type":"unfollow"}
{"type":"cancel","id":"7"}
```

The reply arrives as `{"type":"logs","id":"7","chunk":N,"entries":[…],"last":bool}`
with `"more":bool` on the last chunk, entries always oldest-first:

```json
{
  "cursor": "s=…",
  "realtime": "1787014660944726",
  "priority": "6",
  "unit": "sshd.service",
  "pid": "1234",
  "message": "…"
}
```

- `direction: "backward"` reads older than the cursor, `"forward"` newer. That
  is `--after-cursor … --reverse` and `--after-cursor …` respectively.
- Scrollback is unbounded. `limit` caps one page, not the history: each page is
  anchored on the cursor the last one ended on, so a reader walks back as far
  as the journal goes, one page at a time. `more` says whether `journalctl`
  produced a full page — counted from what it emitted, not from what survived
  the parse, so an entry without a cursor cannot end the walk early.

### Following

`{"type":"follow"}` starts a live `journalctl --follow` and streams entries as
they are written, until `unfollow`, another `follow`, or the channel closing.
Replies carry `"follow":true` and are otherwise ordinary `logs` messages:

```json
{"type":"logs","follow":true,"id":"9","entries":[…],"last":true}
{"type":"followEnd","id":"9","message":"journal follow ended"}
```

Passing the `cursor` the loaded page ended on is what makes the join seamless —
the stream resumes exactly there, so nothing written while the page was
rendering is missed and nothing arrives twice. Without a cursor it starts at
the end (`--lines=0`) rather than replaying the journal.

Entries are batched for 200 ms (or 256 entries, whichever comes first): a busy
unit emits bursts, and one channel message per line would be mostly framing.
Measured end to end, an entry reaches a reader about 700 ms after it is
written, most of which is `journalctl`'s own follow poll.

`followEnd` means the stream stopped — a rotated journal, a killed child — and
is the reader's cue to offer a resume rather than sit in front of a dead tail.

- `grep` is a server-side regex, so a search covers the whole boot rather than
  whatever the client has fetched.
- `scope: "all"` drops the `--system`/`--user` filter, which is what reading a
  copied or foreign journal needs.
- One query of each kind per channel: a viewer scrolling fast replaces its own
  page, but the boot list it asked for at the same time survives.

**The server's user must be able to read the journal.** journald keeps
`/var/log/journal` as `root:systemd-journal`, so a server running as an
ordinary user gets _"No journal files were opened due to insufficient
permissions"_ — reported verbatim rather than as an empty page. Add the user to
`systemd-journal`, or point the extension at a journal it can read:

| argument             | meaning                                                                             |
| -------------------- | ----------------------------------------------------------------------------------- |
| `--journal-dir PATH` | read this journal directory (`journalctl --directory`) instead of the machine's own |
