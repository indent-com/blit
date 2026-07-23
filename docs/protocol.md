# Wire Protocol

The blit wire protocol is a custom binary format defined in `crates/remote/`. There is no protobuf, JSON, or external schema. The protocol is symmetric in framing but asymmetric in message types: clients send `C2S_*` messages, servers send `S2C_*` messages. It is intentionally version-stable: new message types are added with new opcodes; existing opcodes never change layout.

## Framing

Every non-WebSocket transport wraps messages in a **4-byte little-endian length prefix** followed by the payload:

```
[len:4 LE][payload:len]
```

WebSocket provides its own framing, so the length prefix is omitted over WebSocket — each binary WebSocket frame is exactly one blit message. This framing convention is shared by:

- `blit-server` (`crates/server/src/lib.rs`)
- `blit-cli` (`crates/cli/src/transport.rs`)
- `blit-gateway` (`crates/gateway/src/lib.rs`)
- `blit-proxy` (`crates/proxy/src/lib.rs`)
- Browser WebTransport/WebRTC (`js/core/src/transports/`)

Maximum frame size: **16 MiB**.

## Message format

Every message begins with a **1-byte opcode**. All multi-byte fields are little-endian. Fields are tightly packed with no padding or alignment. PTY identifiers are 2-byte unsigned integers.

## Client → Server (C2S)

| Opcode | Name                   | Layout                                                                           |
| ------ | ---------------------- | -------------------------------------------------------------------------------- |
| `0x00` | `INPUT`                | `[pty_id:2][data:N]`                                                             |
| `0x01` | `RESIZE`               | `[pty_id:2][rows:2][cols:2]…` (batch, repeating triplets)                        |
| `0x02` | `SCROLL`               | `[pty_id:2][offset:4]`                                                           |
| `0x03` | `ACK`                  | (no payload)                                                                     |
| `0x04` | `DISPLAY_RATE`         | `[fps:2]`                                                                        |
| `0x05` | `CLIENT_METRICS`       | `[backlog:2][ack_ahead:2][apply_ms_x10:2]`                                       |
| `0x06` | `MOUSE`                | `[pty_id:2][type:1][button:1][col:2][row:2]`                                     |
| `0x07` | `RESTART`              | `[pty_id:2]`                                                                     |
| `0x08` | `PING`                 | _(empty)_ — application-level keepalive                                          |
| `0x0F` | `QUIT`                 | _(empty)_ — request server shutdown                                              |
| `0x10` | `CREATE`               | `[rows:2][cols:2][tag_len:2][tag:N]`                                             |
| `0x11` | `FOCUS`                | `[pty_id:2]`                                                                     |
| `0x12` | `CLOSE`                | `[pty_id:2]`                                                                     |
| `0x13` | `SUBSCRIBE`            | `[pty_id:2]`                                                                     |
| `0x14` | `UNSUBSCRIBE`          | `[pty_id:2]`                                                                     |
| `0x15` | `SEARCH`               | `[request_id:2][query:N]`                                                        |
| `0x16` | `CREATE_AT`            | `[rows:2][cols:2][src_pty_id:2][tag_len:2][tag:N]`                               |
| `0x17` | `CREATE_N`             | `[nonce:2][rows:2][cols:2][tag_len:2][tag:N]`                                    |
| `0x18` | `CREATE2`              | `[nonce:2][rows:2][cols:2][features:1][tag_len:2][tag:N][optional…]`             |
| `0x19` | `READ`                 | `[nonce:2][pty_id:2][offset:4][limit:4][flags:1]`                                |
| `0x1A` | `KILL`                 | `[pty_id:2][signal:4]` — send signal to PTY session leader                       |
| `0x1B` | `COPY_RANGE`           | `[nonce:2][pty_id:2][start_tail:4][start_col:2][end_tail:4][end_col:2][flags:1]` |
| `0x20` | `SURFACE_INPUT`        | `[surface_id:2][keycode:4][pressed:1]`                                           |
| `0x21` | `SURFACE_POINTER`      | `[surface_id:2][type:1][button:1][x:2][y:2]`                                     |
| `0x22` | `SURFACE_POINTER_AXIS` | `[surface_id:2][axis:1][value:4]`                                                |
| `0x23` | `SURFACE_RESIZE`       | `[surface_id:2][width:2][height:2][scale_120:2]`                                 |
| `0x24` | `SURFACE_FOCUS`        | `[surface_id:2]`                                                                 |
| `0x25` | `CLIPBOARD_SET`        | `[mime_len:2][mime:N][data_len:4][data:M]`                                       |
| `0x26` | `SURFACE_LIST`         | _(empty)_ — request list of compositor surfaces                                  |
| `0x27` | `SURFACE_CAPTURE`      | `[surface_id:2][format:1][quality:1]` — screenshot (0=PNG, 1=AVIF)               |
| `0x28` | `SURFACE_SUBSCRIBE`    | `[surface_id:2][codec:1][quality:1]`                                             |
| `0x29` | `SURFACE_UNSUBSCRIBE`  | `[surface_id:2]`                                                                 |
| `0x2A` | `SURFACE_ACK`          | `[surface_id:2]` — acknowledge receipt of video frame                            |
| `0x2B` | `SURFACE_CLOSE`        | `[surface_id:2]` — request close of Wayland surface                              |
| `0x2C` | `CLIPBOARD_LIST`       | (no payload)                                                                     |
| `0x2D` | `CLIENT_FEATURES`      | `[codec_support:1]` — client capability advertisement                            |
| `0x2E` | `CLIPBOARD_GET`        | `[mime_len:2][mime:N]`                                                           |
| `0x2F` | `SURFACE_TEXT`         | `[surface_id:2][text:N]` — composed text input (UTF-8)                           |
| `0x30` | `AUDIO_SUBSCRIBE`      | `[bitrate_kbps:2]`                                                               |
| `0x31` | `AUDIO_UNSUBSCRIBE`    | (no payload)                                                                     |
| `0x40` | `FS_SYNC`              | `[nonce:2][flags:1][latency_ms:2][inline_max:4][path_len:2][path:N]`             |
| `0x41` | `FS_STOP`              | `[sync_id:2]`                                                                    |
| `0x42` | `FS_ACK`               | `[sync_id:2][update_id:4]` — cumulative                                          |
| `0x43` | `FS_FETCH`             | `[nonce:2][sync_id:2][path_len:2][path:N]`                                       |

**Notes:**

`CREATE2` extends `CREATE` with a nonce for response correlation and optional fields gated by feature bits in the `features` byte:

- Bit 0 (`HAS_SRC_PTY`): followed by `[src_pty_id:2]` — create the new PTY in the same working directory as `src_pty_id`.
- Bit 1 (`HAS_COMMAND`): remaining bytes after tag (and `src_pty_id` if present) are the UTF-8 command string (no length prefix) — spawn this command instead of the default shell.
- Bit 2 (`HAS_CWD`): followed by `[cwd_len:2][cwd:N]` (before any command bytes) — spawn in this working directory.

`READ` requests text from a PTY's scrollback + viewport:

- `offset`: lines to skip (from top, or from end when `READ_TAIL` is set).
- `limit`: max lines to return (0 = all).
- `flags`: bit 0 (`READ_ANSI`) includes ANSI escape sequences; bit 1 (`READ_TAIL`) counts from the end.
- Server responds with `S2C_TEXT` echoing the same nonce.

`RESIZE` is batched: after the opcode, the payload contains one or more `[pty_id:2][rows:2][cols:2]` triplets. Requires the `RESIZE_BATCH` feature bit in `S2C_HELLO`.

`SURFACE_SUBSCRIBE` has two optional trailing bytes for per-surface codec/quality control:

- `codec` (byte 3): `CODEC_SUPPORT_*` bitmask restricting which codecs the server may use for this surface. `0` = use the connection-level default (from `C2S_CLIENT_FEATURES`).
- `quality` (byte 4): desired compression quality. `0` = server default (from `BLIT_SURFACE_QUALITY`), `1` = low, `2` = medium, `3` = high, `4` = lossless.

Both bytes are optional — a 3-byte message uses connection/server defaults. Re-subscribing to an already-subscribed surface with different values updates the preferences and forces encoder recreation.

## Server → Client (S2C)

| Opcode | Name                | Layout                                                                                                     |
| ------ | ------------------- | ---------------------------------------------------------------------------------------------------------- |
| `0x00` | `UPDATE`            | `[pty_id:2][lz4-compressed-frame]`                                                                         |
| `0x01` | `CREATED`           | `[pty_id:2][tag:N]`                                                                                        |
| `0x02` | `CLOSED`            | `[pty_id:2]`                                                                                               |
| `0x03` | `LIST`              | `[count:2][entries…]`                                                                                      |
| `0x04` | `TITLE`             | `[pty_id:2][title:N]`                                                                                      |
| `0x05` | `SEARCH_RESULTS`    | `[request_id:2][results…]`                                                                                 |
| `0x06` | `CREATED_N`         | `[nonce:2][pty_id:2][tag:N]`                                                                               |
| `0x07` | `HELLO`             | `[version:2][features:4]`                                                                                  |
| `0x08` | `EXITED`            | `[pty_id:2][exit_status:4]`                                                                                |
| `0x09` | `READY`             | (no payload)                                                                                               |
| `0x0A` | `TEXT`              | `[nonce:2][pty_id:2][total_lines:4][offset:4][text:N]`                                                     |
| `0x0B` | `PING`              | _(empty)_ — server keepalive                                                                               |
| `0x0C` | `QUIT`              | _(empty)_ — server shutting down                                                                           |
| `0x0D` | `USED_ROWS`         | `[pty_id:2][used_rows:2]`                                                                                  |
| `0x20` | `SURFACE_CREATED`   | `[surface_id:2][parent_id:2][w:2][h:2][title_len:2][title:N][app_id_len:2][app_id:M]`                      |
| `0x21` | `SURFACE_DESTROYED` | `[surface_id:2]`                                                                                           |
| `0x22` | `SURFACE_FRAME`     | `[surface_id:2][timestamp:4][flags:1][w:2][h:2][data:N]`                                                   |
| `0x23` | `SURFACE_TITLE`     | `[surface_id:2][title:N]`                                                                                  |
| `0x24` | `SURFACE_RESIZED`   | `[surface_id:2][w:2][h:2]`                                                                                 |
| `0x25` | `CLIPBOARD_CONTENT` | `[mime_len:2][mime:N][data_len:4][data:M]`                                                                 |
| `0x26` | `SURFACE_LIST`      | `[count:2]` repeated `[surface_id:2][parent_id:2][w:2][h:2][title_len:2][title:N][app_id_len:2][app_id:M]` |
| `0x27` | `SURFACE_CAPTURE`   | `[surface_id:2][width:4][height:4][image_data:N]` — PNG or AVIF                                            |
| `0x28` | `SURFACE_APP_ID`    | `[surface_id:2][app_id:N]`                                                                                 |
| `0x29` | `SURFACE_CURSOR`    | `[surface_id:2][shape_len:1][shape:N]` — CSS cursor keyword                                                |
| `0x2A` | `SURFACE_ENCODER`   | `[surface_id:2][name][0x00][codec_string]` — encoder display name + WebCodecs codec string, NUL-separated  |
| `0x2B` | `FRAGMENT`          | `[flags:1][chunk:N]` — see [Fragmentation](#fragmentation)                                                 |
| `0x2C` | `CLIPBOARD_LIST`    | `[count:2] repeated{ [mime_len:2][mime:N] }`                                                               |
| `0x30` | `AUDIO_FRAME`       | `[timestamp:4][flags:1][data:N]`                                                                           |
| `0x40` | `FS_SYNCED`         | `[nonce:2][sync_id:2][status:1][detail_len:2][detail:N]`                                                   |
| `0x41` | `FS_UPDATE`         | `[sync_id:2][update_id:4][flags:1][records:LZ4]`                                                           |
| `0x42` | `FS_FILE`           | `[nonce:2][status:1][data:LZ4]`                                                                            |
| `0x43` | `FS_CLOSED`         | `[sync_id:2][reason:1]`                                                                                    |

**Notes:**

`S2C_HELLO` is the first message sent on every new connection. `version` is the server's protocol version. `features` is a 4-byte bitmask:

| Bit | Name           | Meaning                                                        |
| --- | -------------- | -------------------------------------------------------------- |
| 0   | `CREATE_NONCE` | Server supports `CREATE2` / `CREATED_N` with nonce correlation |
| 1   | `RESTART`      | Server supports `C2S_RESTART` to respawn exited PTYs           |
| 2   | `RESIZE_BATCH` | Server accepts batched resize entries in a single `C2S_RESIZE` |
| 3   | `COPY_RANGE`   | Server supports range-based text copy                          |
| 4   | `COMPOSITOR`   | Server supports headless Wayland compositor                    |
| 5   | `AUDIO`        | Server supports audio forwarding (PipeWire capture + Opus)     |
| 6   | `FS_SYNC`      | Server supports the `FS_*` filesystem sync family              |
| 7   | `GIT`          | Server supports the `GIT_*` git introspection family           |
| 8   | `LSP`          | Server supports the `LSP_*` language intelligence family       |

`S2C_LIST` entry layout: `[pty_id:2][tag_len:2][tag:N][cmd_len:2][cmd:M]` per
PTY. The trailing command field is a backward-compatible extension; old
entries without it parse as an empty command.

`S2C_EXITED` exit status: `WEXITSTATUS` for normal exits (0, 1, …); negative signal number for signal deaths (-9 = SIGKILL); `i32::MIN` when status is unknown.

`S2C_SURFACE_FRAME` flags byte: bit 0 is the keyframe flag; bits 1–2 encode the codec — H.264 (0), AV1 (1), PNG (2). Remaining bits are reserved. `timestamp` is a monotonic millisecond counter captured at compositor-commit time (not wire-send time), so clients can drive video presentation and A/V sync off encode-time instead of network-delivery jitter.

Each `(client, surface)` pair runs at most one server-side encoder, at the compositor's native pixel size. Multiple mounts on the same client share the stream via refcounting; `S2C_SURFACE_FRAME` is broadcast to every subscribed client.

`S2C_AUDIO_FRAME` carries Opus-encoded audio from the compositor's mixed output. `timestamp` is a sample offset in 48 kHz ticks. `flags` bits 1-2 encode the codec (0 = Opus). Audio is per-compositor (one mixed stream from all apps), not per-surface. Only sent when the `AUDIO` feature bit is set in `S2C_HELLO`.

`C2S_AUDIO_SUBSCRIBE` carries a `bitrate_kbps` field (little-endian u16): the desired Opus bitrate in kbps, e.g. 64 for 64 kbps. `0` means server default. Clients may re-send `AUDIO_SUBSCRIBE` to adjust bitrate without unsubscribing first. When multiple clients are subscribed, the server uses the highest requested bitrate.

## Connection lifecycle

On connect, the server immediately sends:

```
S2C_HELLO       (version + feature bits)
S2C_LIST        (all existing PTYs)
S2C_TITLE       (one per PTY, if title is set)
S2C_EXITED      (one per exited-but-retained PTY)
S2C_READY       (end of initial burst)
```

After `S2C_READY`, the client can start sending commands. `S2C_UPDATE` frames are not sent until the client subscribes to a PTY with `C2S_SUBSCRIBE`.

## Frame update encoding

`S2C_UPDATE` payload (after opcode and pty_id) is LZ4-compressed (`lz4_flex::compress_prepend_size`). Decompressed:

**Header (12 bytes):**

```
[rows:2][cols:2][cursor_row:2][cursor_col:2][mode:2][title_field:2]
```

`title_field` packs flags in the upper 4 bits and title UTF-8 length in bits 0–11:

| Bit  | Flag                 |
| ---- | -------------------- |
| 15   | `TITLE_PRESENT`      |
| 14   | `OPS_PRESENT`        |
| 13   | `STRINGS_PRESENT`    |
| 12   | `LINE_FLAGS_PRESENT` |
| 0–11 | Title UTF-8 length   |

**Cell operations** follow the header when `OPS_PRESENT`:

- `OP_COPY_RECT (0x01)` — copy a rectangle of cells from another position. Encodes scrolling without retransmitting unchanged content.
- `OP_FILL_RECT (0x02)` — fill a rectangle with a single cell value. Efficient for clears and blank regions.
- `OP_PATCH_CELLS (0x03)` — bitmask-indexed individual cell updates, column-major interleaved. Only changed cells are transmitted.

**Cell format** — each cell is exactly **12 bytes**:

```
Byte 0 (flags0): fg_type[2] | bg_type[2] | bold | dim | italic | underline
Byte 1 (flags1): inverse | wide | wide_continuation | content_len[3] | (reserved)
Bytes 2–4:       fg color (r, g, b) or palette index
Bytes 5–7:       bg color (r, g, b) or palette index
Bytes 8–11:      UTF-8 content (up to 4 bytes)
```

Color type encoding: 0 = default terminal color, 1 = indexed (256-color palette), 2 = RGB true color.

When `content_len == 7`, the cell's text exceeds 4 bytes. Bytes 8–11 hold an FNV-1a hash used for diff comparison; the actual UTF-8 string is transmitted in the `STRINGS_PRESENT` section, keyed by cell index.

**Mode bits** (16-bit field in frame header):

- Bits 0–8: cursor style, app cursor keys (`DECCKM`), app keypad, alternate screen, mouse mode (X10/VT200/button-event/any-event), mouse encoding (UTF-8/SGR/pixel)
- Bit 9: PTY echo flag (`tcgetattr ECHO`)
- Bit 10: PTY canonical mode (`tcgetattr ICANON`)

Mode bits are tracked by `ModeTracker` in `blit-alacritty`, which intercepts CSI/DCS sequences from raw PTY output.

## Fragmentation

`S2C_FRAGMENT` (`0x2B`) splits any bulk server message into chunks so small
frames (audio, keepalives) never sit behind a multi-megabyte write:

```
[0x2B][flags:1][chunk:N]
```

Flag bit 0 (`FRAGMENT_FLAG_LAST`) marks the final chunk. Chunks carry the
original message's bytes verbatim (its opcode arrives in the first chunk);
the receiver concatenates chunks into one logical message and dispatches it
normally. Fragments of different messages never interleave — one pending
reassembly buffer suffices — and only `S2C_AUDIO_FRAME` may appear between
fragments. The server chunks any payload over 4 KiB; logical messages may
exceed the 16 MiB frame limit.

## Compressed payloads

Fields documented as `:LZ4` are `lz4_flex::compress_prepend_size` (a 4-byte
LE uncompressed size, then the LZ4 block). Receivers MUST check the
declared size against `MAX_DECOMPRESSED` (64 MiB) _before_ allocating, so a
hostile or corrupt length cannot force a giant allocation. The constant is
protocol-wide — exported as `MAX_DECOMPRESSED` from `blit-remote` and
`@blit-sh/core` (the fs family's `FS_MAX_DECOMPRESSED` is the same value) —
and every family bounds its responses well under it, so a well-behaved
peer never trips the guard.

## Filesystem sync

The `FS_*` family (feature bit 6) mirrors a server-side directory tree into
clients as ordered state diffs: a client `FS_SYNC`s a path, receives a staged
snapshot followed by live updates (`RESET`/`SYNC` flags delimit staged
series), applies LZ4-compressed `UPSERT`/`DELETE`/`MOVE` records to a map,
and acknowledges cumulatively via `FS_ACK` (byte-window pacing,
`BLIT_FS_WINDOW`). `FS_FETCH` pulls one file's full content on demand;
`FS_WRITE`/`FS_OP` write back to disk — content upserts under
compare-and-swap on the synced content hash, plus
mkdir/remove/rename/symlink/hardlink —
each answered by one `FS_DONE`
([design/fs-write.md](design/fs-write.md)). The write side shares the
family's feature bit; `BLIT_FS_WRITE=0` makes a deployment read-only
(writes answer `PERMISSION`).
Wire details, record layouts, and semantics:
[design/fs-watch.md](design/fs-watch.md); server engine:
`crates/fssync`; codecs and the `FsMirror` reference reducer:
`crates/remote/src/fs.rs` (Rust) and `js/core/src/fs.ts` (TypeScript,
surfaced as `syncFs` on `BlitConnection`/`BlitWorkspace`).

## Git introspection

The `GIT_*` family (feature bit 7) opens repositories by path, pushes
mutable state (HEAD, refs, in-progress operation, status) as
whole-snapshot `GIT_STATE` messages, and pulls immutable content
(commits, trees, blobs, diffs, patches) by content address through
nonce request/response pairs. Wire details:
[design/git.md](design/git.md); server engine: `crates/git`; codecs and
the `GitStateMirror` reference reducer: `crates/remote/src/git.rs` and
`js/core/src/git.ts` (surfaced as `openRepo` on
`BlitConnection`/`BlitWorkspace`).

## Language intelligence

The `LSP_*` family (feature bit 8) terminates LSP at the server: warm
language-server backends are daemon-owned and shared, backend
phase/capabilities are pushed as `LSP_STATE` snapshots, diagnostics as
per-file replacement sets (`LSP_DIAG`, `FULL` replay on subscribe), and
definition/references/hover/symbols/rename-as-data are pulled through
the single `LSP_QUERY` opcode. Positions are 0-based lines with UTF-8
byte columns; the server transcodes. Wire details:
[design/lsp.md](design/lsp.md); server engine: `crates/lsp`; codecs and
the `LspStateMirror`/`LspDiagMirror` reference reducers:
`crates/remote/src/lsp.rs` and `js/core/src/lsp.ts` (surfaced as
`openLsp` on `BlitConnection`).

## Multiplexed WebSocket (`/mux`)

The `/mux` WebSocket endpoint carries traffic for **all** gateway destinations over a single connection. This replaces the legacy model where the browser opened one WebSocket per remote (`/d/<name>`).

### Authentication

Same as the per-destination handler: the browser sends the passphrase as a text frame. The server responds with `"mux"` (not `"ok"`) to confirm multiplexed mode. After auth, all subsequent frames are binary.

### Framing

Every binary frame is prefixed with a **2-byte LE channel ID**:

```
[channel_id:2 LE][payload:N]        channel_id < 0xFFFF → data
[0xFFFF][control_opcode:1][...]      channel_id = 0xFFFF → control
```

Data frames carry raw blit protocol messages (starting with the usual 1-byte opcode). The gateway strips the channel prefix before forwarding to the upstream blit server and prepends it to responses.

### Control messages

| Direction | Opcode | Name     | Layout                               |
| --------- | ------ | -------- | ------------------------------------ |
| C → S     | `0x01` | `OPEN`   | `[channel_id:2][name_len:2][name:N]` |
| C → S     | `0x02` | `CLOSE`  | `[channel_id:2]`                     |
| S → C     | `0x81` | `OPENED` | `[channel_id:2]`                     |
| S → C     | `0x82` | `CLOSED` | `[channel_id:2]`                     |
| S → C     | `0x83` | `ERROR`  | `[channel_id:2][msg_len:2][msg:N]`   |

The client assigns channel IDs (starting from 0). `OPEN` maps a channel ID to a named destination; the server connects upstream and responds with `OPENED` or `ERROR`. `CLOSE` tears down a channel. The server also sends `CLOSED` when the upstream connection drops.

### Lifecycle

```
Browser                          Gateway                    blit server
  |                                 |                            |
  |-- WS /mux ------------------->|                            |
  |   (text: passphrase)          |                            |
  |<-- text: "mux" ---------------|                            |
  |                                |                            |
  |  [0xFFFF][OPEN][0][local]     |                            |
  |  --------------------------->  |-- Unix socket ----------->|
  |<-- [0xFFFF][OPENED][0]        |                            |
  |                                |                            |
  |  [0xFFFF][OPEN][1][rabbit]    |                            |
  |  --------------------------->  |-- SSH streamlocal ------->|
  |<-- [0xFFFF][OPENED][1]        |                            |
  |                                |                            |
  |  [0][C2S_INPUT ...]           |-- write_frame(payload) -->|
  |<-- [0][S2C_UPDATE ...]        |<-- read_frame ------------|
  |<-- [1][S2C_HELLO ...]         |<-- read_frame ------------|
```

The legacy `/d/<name>` endpoint remains available for backward compatibility and embedding scenarios.

## ACK and flow control

The gateway and proxy pass `C2S_ACK` through to the server unchanged. Each ACK retires the oldest in-flight frame and updates the server's RTT estimate. The server uses this to:

1. Compute per-client bandwidth-delay product.
2. Pace frame sends to match the client's actual render rate.
3. Avoid pipelining more frames than the link can absorb.

See [docs/server.md § Per-client frame pacing](server.md#per-client-frame-pacing) for details.
