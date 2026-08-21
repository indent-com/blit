---
title: Structured events protocol
---

# `blit.events.v1`

`blit.events.v1` is the bounded remote and file representation for structured
server events. All integers are little-endian.

## Discovery and envelope

A server advertises feature bit 31 (`FEATURE_EVENTS`). Both directions use the
direction-local opcode `0x96` and this eight-byte envelope:

```text
[0x96][version:1 = 1][kind:1][flags:1 = 0][request_id:4][body...]
```

`request_id = 0` is used for unsolicited stream data; requests carry a
caller-selected id. A receiver rejects unknown versions, kinds, and envelope
flags. Once the full envelope has arrived, an invalid request can always be
answered with the same request id using `S2C_STATUS`; an incomplete envelope
cannot be correlated.

## Event record

Remote dumps, live data, and files use the same 64-byte record:

```text
[sequence:8]
[monotonic_ns:8]
[event_id:4][flags:2][source:1][schema:1]
[connection:8][subject:8]
[arg0:8][arg1:8][arg2:8]
```

`sequence` is the monotonic ring sequence. `monotonic_ns` uses the server's
monotonic clock and is meaningful only within one boot. Event catalogs define
the remaining fields. Unknown record flags are preserved because their meaning
is selected by `(event_id, schema)` rather than by this transport.

## Configuration and dump

The activation mask is exactly 16 bytes. Bit `n` controls event id `n` for ids
0 through 127. Ring size is in records and must be in `1..=1,048,576`.

Stable named ids are grouped into these ranges:

| Range   | Family             | Named ids                                                                                                                      |
| ------- | ------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| 0–7     | server             | starting, started, stopping, stopped, error                                                                                    |
| 8–15    | client             | connected, ready, disconnecting, disconnected, error                                                                           |
| 16–23   | raw request        | read, dispatch, done, reject                                                                                                   |
| 24–31   | writer             | dequeue, write-begin, write-end, error, backpressure                                                                           |
| 32–55   | PTY                | create lifecycle, read/queue/drain/parse/frame/input/resize/exit/evict/I/O error                                               |
| 56–63   | process            | request, spawn, result, I/O, exit, error                                                                                       |
| 64–71   | compositor/surface | compositor lifecycle and surface create/destroy/frame/error                                                                    |
| 72–103  | protocol           | core, PTY, process, compositor, surface, input, clipboard, filesystem, network, KV, browser, audio, events, integration, error |
| 104–111 | task               | spawned, completed, cancelled, failed                                                                                          |
| 112–127 | recorder           | config changed/error, ring dropped/overwritten, stream gap/error                                                               |

The complete stable `(id, name)` table is `blit_remote::events::EVENT_NAMES`.
Names use lowercase kebab case. Unlisted bits are reserved but remain visible
and round-trip in activation masks.

The server allocates a 1 MiB ring by default. Its default activation enables
low-rate server, client, PTY-create, PTY/process exit, compositor/surface
lifecycle, and error/refusal events; request, writer, PTY I/O, process I/O,
frame, and protocol-family events are opt-in. Startup configuration is:

```text
BLIT_EVENTS_BYTES=1MiB
BLIT_EVENTS=default|all|none|family,event,+event,-event
BLIT_EVENTS_FILE=/path/to/capture.events
```

`BLIT_EVENTS_BYTES` must be a multiple of the 64-byte record size. Runtime
`CONFIG_SET` can change both capacity and the activation bitset. Resizing keeps
the newest complete records that fit; a producer that collides with resize or
an in-progress overwrite consumes a sequence and is therefore visible as a
gap rather than silently disappearing. `CONFIG_SET_IF` performs the same change
only if both current fields still match the expected configuration. A mismatch
returns `STATUS_CONFLICT` and the current configuration without changing it.

Client-to-server kinds:

```text
1 CONFIG_GET      []
2 CONFIG_SET      [ring_size:4][activation:16]
3 DUMP            [from_sequence:8][limit:4]
8 CONFIG_SET_IF   [expected_ring_size:4][expected_activation:16]
                  [ring_size:4][activation:16]
```

Server-to-client kinds:

```text
0 STATUS        [request_kind:1][status:1]
1 CONFIG        [status:1][ring_size:4][activation:16]
2 DUMP          [status:1][first_sequence:8][next_sequence:8]
                [count:4][record:64]...
```

A dump limit is `1..=65,536`. `first_sequence` reports the first returned
sequence after any eviction clamp. `next_sequence` is the cursor for the next
request. Status values come from the common protocol status registry.

## Client live streams

Client-to-server kinds:

```text
4 STREAM_START  [stream_id:4][from_sequence:8][flags:1]
5 STREAM_STOP   [stream_id:4]
```

`STREAM_FOLLOW` (flags bit 0) keeps the stream open after replay reaches the
live edge. Other bits are invalid.

Server-to-client kinds:

```text
3 STREAM_STATUS [status:1][stream_id:4][next_sequence:8]
4 STREAM_DATA   [stream_id:4][server_monotonic_ns:8]
                [count:4][record:64]...
```

`STREAM_STATUS` is correlated to start or stop. Unsolicited statuses use request
id zero: `STATUS_BUDGET` reports a gap, while the first `STATUS_OK` marks the
transition from replay to the live edge. They cannot be mistaken for a second
reply to the completed start request. `STREAM_DATA` is also unsolicited, so its
envelope request id is zero. `server_monotonic_ns` is sampled from the recorder
clock when the packet is built, allowing a consumer to age replayed records
without assuming immediate delivery. One data packet carries at most 65,536
records.

## Server-side file streams

These streams make the server write canonical event files without relaying all
records through a CLI or guest.

Client-to-server kinds:

```text
6 FILE_START    [stream_id:4][flags:1][path_len:2][path...]
7 FILE_STOP     [stream_id:4]
```

Paths are nonempty, NUL-free UTF-8 of at most 4096 bytes. `FILE_APPEND` is flags
bit 0 and `FILE_SYNC` is bit 1. Other bits are invalid.

Server-to-client kind:

```text
5 FILE_STATUS   [status:1][stream_id:4][records_written:8][bytes_written:8]
                [detail_len:2][detail...]
```

Status is correlated to start or stop. Detail is UTF-8 of at most 4096 bytes.

## Canonical event file

A canonical file begins with this 32-byte header, followed immediately by
64-byte event records:

```text
["blit.events.v1\0\0":16]
[version:1 = 1][flags:1 = 0]
[header_size:2 = 32][record_size:2 = 64]
[reserved:10 = 0]
```

There is exactly one valid v1 header encoding. A reader rejects nonzero flags or
reserved bytes and mismatched sizes rather than guessing a layout.

## CLI mapping

`blit events config [--json]` sends `CONFIG_GET`. `blit events config set`
sends `CONFIG_SET`; when only one of `--bytes` and `--active` is present, it
first reads the current configuration and preserves the omitted field. The
CLI presents ring capacity in bytes even though the wire stores record count,
and requires the byte value to be a multiple of 64. Activation input is either
32 hexadecimal digits (the 16 wire bytes in display order) or comma-separated
names, numeric ids, and family selectors.

`blit events dump` emits one canonical header followed by the records from one
`DUMP` reply. `blit events stream` uses a random nonzero stream id, maps
`oldest` to sequence zero and `now` to `u64::MAX`, writes one canonical header,
and appends each `STREAM_DATA` record unchanged. It sends `STREAM_STOP` on
Ctrl-C or a broken output pipe. Every solicited reply is accepted only when its
request id and operation match the request; unsolicited stream data must have
the protocol-mandated zero request id.

`blit events file start` and `stop` map directly to the server-side file stream
messages. Paths name the server filesystem, not the client filesystem. The CLI
prints the stream id and counters returned by `FILE_STATUS`.
