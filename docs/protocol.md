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

Any per-request reply guarantee is conditional on the logical connection
remaining live through that reply. A transport failure or a documented fatal
framing, protocol, or endpoint-resource violation closes the connection and
cancels its outstanding requests without synthesizing replies. Clients resolve
every pending operation as a connection error in that case.

## Client → Server (C2S)

| Opcode | Name                    | Layout                                                                                                                                                                                                                                           |
| ------ | ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `0x00` | `INPUT`                 | `[pty_id:2][data:N]`                                                                                                                                                                                                                             |
| `0x01` | `RESIZE`                | `[pty_id:2][rows:2][cols:2]…` (batch, repeating triplets)                                                                                                                                                                                        |
| `0x02` | `SCROLL`                | `[pty_id:2][offset:4]`                                                                                                                                                                                                                           |
| `0x03` | `ACK`                   | (no payload)                                                                                                                                                                                                                                     |
| `0x04` | `DISPLAY_RATE`          | `[fps:2]`                                                                                                                                                                                                                                        |
| `0x05` | `CLIENT_METRICS`        | `[backlog:2][ack_ahead:2][apply_ms_x10:2]`                                                                                                                                                                                                       |
| `0x06` | `MOUSE`                 | `[pty_id:2][type:1][button:1][col:2][row:2]`                                                                                                                                                                                                     |
| `0x07` | `RESTART`               | `[pty_id:2]`                                                                                                                                                                                                                                     |
| `0x08` | `PING`                  | _(empty)_ — application-level keepalive                                                                                                                                                                                                          |
| `0x09` | `CLIENT_LIST`           | `[nonce:2]` — enumerate connected clients                                                                                                                                                                                                        |
| `0x0A` | `KICK`                  | `[nonce:2][client_id:8][reason:N]` — disconnect another client with a UTF-8 reason                                                                                                                                                               |
| `0x0B` | `CLIENT_WATCH`          | `[nonce:2]` — subscribe to live client-catalog snapshots                                                                                                                                                                                         |
| `0x0C` | `CLIENT_UNWATCH`        | `[nonce:2]` — stop the client-catalog subscription using this nonce                                                                                                                                                                              |
| `0x0F` | `QUIT`                  | _(empty)_ — request server shutdown                                                                                                                                                                                                              |
| `0x10` | `CREATE`                | `[rows:2][cols:2][tag_len:2][tag:N]`                                                                                                                                                                                                             |
| `0x11` | `FOCUS`                 | `[pty_id:2]`                                                                                                                                                                                                                                     |
| `0x12` | `CLOSE`                 | `[pty_id:2]`                                                                                                                                                                                                                                     |
| `0x13` | `SUBSCRIBE`             | `[pty_id:2]`                                                                                                                                                                                                                                     |
| `0x14` | `UNSUBSCRIBE`           | `[pty_id:2]`                                                                                                                                                                                                                                     |
| `0x15` | `SEARCH`                | `[request_id:2][query:N]`                                                                                                                                                                                                                        |
| `0x16` | `CREATE_AT`             | `[rows:2][cols:2][src_pty_id:2][tag_len:2][tag:N]`                                                                                                                                                                                               |
| `0x17` | `CREATE_N`              | `[nonce:2][rows:2][cols:2][tag_len:2][tag:N]`                                                                                                                                                                                                    |
| `0x18` | `CREATE2`               | `[nonce:2][rows:2][cols:2][features:1][tag_len:2][tag:N][optional…]`                                                                                                                                                                             |
| `0x19` | `READ`                  | `[nonce:2][pty_id:2][offset:4][limit:4][flags:1]`                                                                                                                                                                                                |
| `0x1A` | `KILL`                  | `[pty_id:2][signal:4][flags:1]` — send signal to a PTY's process group; `flags` optional                                                                                                                                                         |
| `0x1B` | `COPY_RANGE`            | `[nonce:2][pty_id:2][start_tail:4][start_col:2][end_tail:4][end_col:2][flags:1]`                                                                                                                                                                 |
| `0x1C` | `TERM_CWD`              | `[nonce:2][pty_id:2]` — request a PTY's live working directory (see [Working directory tracking](#working-directory-tracking))                                                                                                                   |
| `0x1D` | `DEADLINE`              | `[pty_id:2][ms:4]` — arm or refresh a server-enforced deadline; `ms = 0` clears it                                                                                                                                                               |
| `0x1E` | `SCROLL_BY`             | `[pty_id:2][delta:4 i32]` — move a scrolled view relative to where the server holds it (see [Scrollback](#scrollback))                                                                                                                           |
| `0x20` | `SURFACE_INPUT`         | `[surface_id:2][keycode:4][pressed:1][time_ms:4]` — `time_ms` is the browser `KeyboardEvent.timeStamp`, `0` when the sender synthesised the key                                                                                                  |
| `0x21` | `SURFACE_POINTER`       | `[surface_id:2][type:1][button:1][x:2][y:2]`                                                                                                                                                                                                     |
| `0x22` | `SURFACE_POINTER_AXIS`  | `[surface_id:2][axis:1][value:4]` — legacy scroll, superseded by `0x32`                                                                                                                                                                          |
| `0x23` | `SURFACE_RESIZE`        | `[surface_id:2][width:2][height:2][scale_120:2]`                                                                                                                                                                                                 |
| `0x24` | `SURFACE_FOCUS`         | `[surface_id:2]`                                                                                                                                                                                                                                 |
| `0x25` | `CLIPBOARD_SET`         | `[mime_len:2][mime:N][data_len:4][data:M]`                                                                                                                                                                                                       |
| `0x26` | `SURFACE_LIST`          | _(empty)_ — request list of compositor surfaces                                                                                                                                                                                                  |
| `0x27` | `SURFACE_CAPTURE`       | `[surface_id:2][format:1][quality:1]` — screenshot (0=PNG, 1=AVIF)                                                                                                                                                                               |
| `0x28` | `SURFACE_SUBSCRIBE`     | `[surface_id:2][codec:1][bandwidth:1][speed:1][width:2][height:2][max_fps:2]`                                                                                                                                                                    |
| `0x29` | `SURFACE_UNSUBSCRIBE`   | `[surface_id:2]`                                                                                                                                                                                                                                 |
| `0x2A` | `SURFACE_ACK`           | `[surface_id:2]` — acknowledge receipt of video frame                                                                                                                                                                                            |
| `0x2B` | `SURFACE_CLOSE`         | `[surface_id:2]` — request close of Wayland surface                                                                                                                                                                                              |
| `0x2C` | `CLIPBOARD_LIST`        | (no payload)                                                                                                                                                                                                                                     |
| `0x2D` | `CLIENT_FEATURES`       | `[codec_support:1]` — client capability advertisement                                                                                                                                                                                            |
| `0x2E` | `CLIPBOARD_GET`         | `[mime_len:2][mime:N]`                                                                                                                                                                                                                           |
| `0x2F` | `SURFACE_TEXT`          | `[surface_id:2][text:N]` — composed text input (UTF-8)                                                                                                                                                                                           |
| `0x30` | `AUDIO_SUBSCRIBE`       | `[bitrate_kbps:2]`                                                                                                                                                                                                                               |
| `0x31` | `AUDIO_UNSUBSCRIBE`     | (no payload)                                                                                                                                                                                                                                     |
| `0x32` | `SURFACE_POINTER_AXIS2` | `[surface_id:2][flags:1][dx_x100:4][dy_x100:4][v120_x:2][v120_y:2]` — see [Scroll](#scroll)                                                                                                                                                      |
| `0x33` | `PRIMARY_SET`           | `[mime_len:2][mime:N][data_len:4][data:M]` — take PRIMARY, see [Primary selection](#primary-selection)                                                                                                                                           |
| `0x34` | `SURFACE_PREEDIT`       | `[surface_id:2][cursor:2][text:N]` — composition in progress (UTF-8); `cursor` is a byte offset, empty text withdraws it                                                                                                                         |
| `0x35` | `SURFACE_DRAG_ENTER`    | `[surface_id:2][x:2][y:2][mime_count:2][mime entries][optional item trailer]` — begin/retarget a drag; each entry is `[mime_len:2][mime:N]`; the append-only trailer is `[item_count:2][item MIME entries]`, see [Drag and drop](#drag-and-drop) |
| `0x36` | `SURFACE_DRAG_MOTION`   | `[surface_id:2][x:2][y:2]` — move the drag                                                                                                                                                                                                       |
| `0x37` | `SURFACE_DRAG_LEAVE`    | `[surface_id:2]` — the drag left the surface                                                                                                                                                                                                     |
| `0x38` | `SURFACE_DRAG_DROP`     | `[surface_id:2][x:2][y:2][item_count:2][items]` — complete the drop; item is `[mime_len:2][mime][name_len:2][name][data_len:4][data]`, see [Drag and drop](#drag-and-drop)                                                                       |
| `0x39` | `SURFACE_DRAG_CANCEL`   | _(empty)_ — abort the drag (Escape / drag left the window)                                                                                                                                                                                       |
| `0x3A` | `SURFACE_TOUCH`         | `[surface_id:2][phase:1][contact_count:1][time_ms:4][contacts…]`; contact is `[identifier:4 i32][x_x100:4 i32][y_x100:4 i32]`; `time_ms` is the browser `TouchEvent.timeStamp`, see [Direct touch](#direct-touch)                                |
| `0x3B` | `DESKTOP_SUBSCRIBE`     | `[flags:1]`; bit 0 tray, bit 1 notifications, `0` unsubscribes; see [tray/notification design](design/tray-notifications.md#wire-protocol)                                                                                                       |
| `0x3C` | `TRAY_EVENT`            | `[tray_id:4][kind:1][menu_revision:4][value:4 i32][flags:1]`; see [tray/notification design](design/tray-notifications.md#client-to-server)                                                                                                      |
| `0x3D` | `NOTIFICATION_EVENT`    | `[notification_id:4][revision:4][kind:1][key_len:2][key:N]`; see [tray/notification design](design/tray-notifications.md#client-to-server)                                                                                                       |
| `0x3E` | `MEDIA_CONTROL`         | `[subtype:1][payload:N]`; viewer media leases, portal replies/stops, and MPRIS subscriptions/actions, see [media devices and portals](design/media-devices-portals.md#client-to-server-control)                                                  |
| `0x3F` | `MEDIA_DATA`            | `[lease_id:4][sequence:4][capture_us:8][kind:1][codec:1][flags:1][fragment_index:2][fragment_count:2][frame_len:4][data:N]`, see [media devices and portals](design/media-devices-portals.md#media-data-and-fragmentation)                       |
| `0x40` | `FS_SYNC`               | `[nonce:2][flags:2][latency_ms:2][inline_max:4][path_len:2][path:N]` + `[exclude_len:2][exclude:M]` if `EXCLUDE` + `[src_pty_id:2]` if `FROM_PTY`; `STAGING` roots the sync at the drag staging dir, see [Drag and drop](#drag-and-drop)         |
| `0x41` | `FS_STOP`               | `[sync_id:2]`                                                                                                                                                                                                                                    |
| `0x42` | `FS_ACK`                | `[sync_id:2][update_id:4]` — cumulative                                                                                                                                                                                                          |
| `0x43` | `FS_FETCH`              | `[nonce:2][sync_id:2][path_len:2][path:N]`                                                                                                                                                                                                       |
| `0x44` | `FS_WRITE`              | `[nonce:2][sync_id:2][flags:1][base:16][mode:4][content_kind:1][path_len:2][path:N][content:LZ4]` — CAS content upsert ([design/fs-write.md](design/fs-write.md))                                                                                |
| `0x45` | `FS_OP`                 | `[nonce:2][sync_id:2][op:1][flags:1][base:16][mode:4][a_len:2][a:N][b_len:2][b:N]` — mkdir/remove/rename/symlink/hardlink ([design/fs-write.md](design/fs-write.md))                                                                             |
| `0x46` | `FS_SEARCH`             | `[nonce:2][limit:2][root_len:2][root:N][query_len:2][query:M]` — server-side fuzzy file search ([design/fs-search.md](design/fs-search.md))                                                                                                      |
| `0x47` | `FS_INDEX`              | `[nonce:2][flags:1][root_len:2][root:N]` — candidate list for client-side search ([design/fs-search.md](design/fs-search.md))                                                                                                                    |
| `0x49` | `FS_UPLOAD_BEGIN`       | `[nonce:2][sync_id:2][flags:1][base:16][mode:4][size:8][path_len:2][path:N]` — begin a chunked upload; `base` is the `FS_WRITE` CAS precondition                                                                                                 |
| `0x4A` | `FS_UPLOAD_CHUNK`       | `[upload_id:2][offset:8][data:LZ4]` — sequential append; `offset` must equal the bytes accepted so far                                                                                                                                           |
| `0x4B` | `FS_UPLOAD_FINISH`      | `[nonce:2][upload_id:2]` — land the upload (terminates it either way)                                                                                                                                                                            |
| `0x4C` | `FS_UPLOAD_CANCEL`      | `[upload_id:2]` — abort the upload; no reply                                                                                                                                                                                                     |

**Notes:**

`SURFACE_RESIZE.scale_120` is the viewer's requested presentation scale in
1/120th units: 60 = 0.5×, 120 = 1×, 240 = 2×, and 0 means unspecified
(1×). It may carry the display's DPI-derived scale or an exact scale chosen
independently of display DPI. Sub-1× values enlarge the surface's logical
window while the compositor stays at Wayland's minimum 1× output scale; the
viewer receives a downscaled stream at its requested physical size.

`CREATE2` extends `CREATE` with a nonce for response correlation and optional fields gated by feature bits in the `features` byte:

- Bit 0 (`HAS_SRC_PTY`): followed by `[src_pty_id:2]` — create the new PTY in the same working directory as `src_pty_id`.
- Bit 1 (`HAS_COMMAND`): remaining bytes after tag (and `src_pty_id` if present) are the UTF-8 command string (no length prefix) — spawn this command instead of the default shell.
- Bit 2 (`HAS_CWD`): followed by `[cwd_len:2][cwd:N]` (before any command bytes) — spawn in this working directory.
- Bit 3 (`WANT_STATUS`): valid only when `HELLO` advertises `CREATE_STATUS`; requests one correlated `CREATED_N` or `CREATE_FAILED` outcome. It adds no trailing field.
- Bit 4 (`HAS_DEADLINE`): followed by `[ms:4]`, after any cwd and before any command bytes — arm a deadline at creation. Valid only when `HELLO` advertises `PTY_DEADLINE`.

`READ` requests text from a PTY's scrollback + viewport:

- `offset`: lines to skip (from top, or from end when `READ_TAIL` is set).
- `limit`: max lines to return (0 = all).
- `flags`: bit 0 (`READ_ANSI`) includes ANSI escape sequences; bit 1 (`READ_TAIL`) counts from the end.
- Server responds with `S2C_TEXT` echoing the same nonce.

`RESIZE` is batched: after the opcode, the payload contains one or more `[pty_id:2][rows:2][cols:2]` triplets. Requires the `RESIZE_BATCH` feature bit in `S2C_HELLO`.

Client control is gated by `FEATURE_CLIENT_CONTROL`. `C2S_CLIENT_LIST` returns
the requester's own server-assigned `u64` ID plus every live client, including
the requester, in ascending ID order. Each client is encoded as
`[client_id:8][age_secs:8][outbound_bytes_per_sec:8]`
`[inbound_bytes_per_sec:8][terminal_count:2]`
`[surface_count:2][subscription_count:2]`, followed by terminal records
(`[pty_id:2][rows:2][cols:2]`), surface records
(`[surface_id:2][width:2][height:2][scale_120:2]`), and auxiliary subscription
records (`[kind:1][id:2]`). `age_secs` is the whole-second connection age;
`outbound_bytes_per_sec` is the latest one-second sample of successfully
written, length-prefixed server-to-client **bytes** — not bits — and
`inbound_bytes_per_sec` is the same sample of length-prefixed bytes read _from_
the client. Both are the server's own accounting of the socket, taken in one
tick so the pair covers a single interval. No client of any kind reports its
own bandwidth, which is what makes a command-line client's figures directly
comparable to a browser's. Zero size fields mean that the client subscribed
without reporting a view size.

Auxiliary subscription kinds are `1` audio (ID is zero), `2` filesystem sync,
`3` Git repo, `4` LSP attachment, `5` KV watch, and `6` network flow. IDs are
local to each family. `C2S_CLIENT_WATCH` immediately returns the same
`S2C_CLIENT_LIST` shape and sends changed snapshots under the same nonce until
`C2S_CLIENT_UNWATCH`. Age and bandwidth are sampled once a second, so a watch
can produce an update that often even when topology is unchanged; between
samples only a real topology change publishes. Multiple watch nonces per client
are valid.

`S2C_KICK_RESULT` is the status reply for the whole client-control family, not
only for `C2S_KICK`: a malformed `CLIENT_LIST` / `CLIENT_WATCH` /
`CLIENT_UNWATCH` is answered with `INVALID` under the sender's nonce rather
than dropped. Clients must therefore settle a pending list or watch on a
`KICK_RESULT` carrying its nonce, or the request hangs until they time out.
A request too short to carry a nonce (fewer than three bytes) is the one case
the server drops, because the nonce would be a guess.

The `blit client list` CLI filters its own short-lived connection from its
output, while persistent clients can use `self_id` to identify their own live
record. Client IDs are a per-process counter, not a capability or a stable
identity: they are only meaningful for the life of the server process,
`HELLO.boot_generation` identifies that lifetime, and a client can infer from
its own ID how many connections preceded it.

`C2S_KICK` cannot target its sender. Its correlated `S2C_KICK_RESULT` uses the
common status registry (`OK`, `NOT_FOUND`, `INVALID`, or `TOO_LARGE`) and may
append diagnostic text. `OK` means the target's connection was told to close,
not that the reason was acknowledged — a target that is already disconnecting
still reports `OK`. On `OK`, the target receives `S2C_KICKED` with the reason
and is then forcibly disconnected. A browser suppresses automatic reconnect
after `KICKED`, but permits a user-requested reconnect; command-line clients
report the reason as an error.

Reasons are UTF-8 and capped at `KICK_REASON_MAX` (1024) bytes. Requesters are
expected to validate and refuse an over-long reason rather than send one: the
CLI and the browser both do, the server answers `TOO_LARGE`, and the message
builders truncate at a UTF-8 scalar boundary only as a last-resort backstop so
a bug cannot put an invalid tail on the wire.

**Authorization.** There is none beyond reaching the socket. `blit-server` has
no read-only mode of its own, so any connection that completes the handshake
can enumerate and kick any other. The read-only capability enforced for
`blit share` consumers lives in the WebRTC forwarder's allowlist, which denies
the entire client-control family — including `CLIENT_LIST`, because telling an
untrusted viewer which ptys and surfaces the other viewers hold discloses
resources that viewer was never offered. Treat "can open a connection" as
"can kick".

`SURFACE_SUBSCRIBE` has optional trailing bytes for per-surface codec, bandwidth, speed, fixed encode size, and cadence control:

- `codec` (byte 3): `CODEC_SUPPORT_*` bitmask restricting which codecs the server may use for this surface. `0` = use the connection-level default (from `C2S_CLIENT_FEATURES`).
- `bandwidth` (byte 4): the **most** bits the surface may spend. `0` = server default (from `BLIT_SURFACE_BANDWIDTH`), `1` = low, `2` = medium, `3` = high, `4` = ultra, `5`–`9` reserved, `10`–`255` = an AV1 quantizer used as the floor. The server adapts below this ceiling on its own — there is no `auto` value to ask for and no way to switch adaptation off. What you pick is the best quality the encoder is allowed to produce; congestion moves it cheaper and recovery moves it back.
- `speed` (byte 5): how much encoder time a frame may cost, independent of bandwidth. `0` = server default (from `BLIT_SURFACE_SPEED`), `1` = slow, `2` = medium, `3` = fast, `4` = realtime, `5`–`9` reserved, `10`–`255` = custom (`10` slowest, `255` fastest).

- `width` / `height` (bytes 6–9, LE u16): a fixed encode size in pixels for this client alone. Both nonzero makes the subscription **scaled**: the server encodes a downscale of the surface for this client and excludes it from surface-size mediation, so it never pulls the compositor surface smaller for anyone else. Both zero — or absent — means the client participates in mediation via `SURFACE_RESIZE` like any other viewer. The size is a bounding box, not an aspect: the server inscribes the surface's own aspect ratio inside it and never upscales past native.

  A viewer that is _handed_ a box rather than sizing one — a side-panel thumbnail — is what this is for. Without it such a viewer has only two options, and both are wrong: report its box and shrink the window for every other viewer, or report nothing and decode full-resolution video into a card.

- `max_fps` (bytes 10–11, LE u16): a ceiling for this subscription's source and delivery cadence. `0` or absent means the client's declared display rate. This is independent of the encode size, so a scaled recording can still request full cadence while a live thumbnail can request a cheaper rate.

All the trailing bytes are optional — a 3-byte message uses connection/server defaults — but they are positional, so asking for a size or cadence means sending the earlier fields too (as zeros, if you have no preference). Re-subscribing to an already-subscribed surface updates the supplied values. Codec, bandwidth, speed, and size changes force encoder recreation; a cadence-only change does not.

## WebRTC read-only profile

The base wire protocol does not carry an access bit. A client connected
directly to `blit-server` is therefore not made read-only by the protocol.
Read-only share tokens (`.ro`) are enforced by the producer-side `blit share`
forwarder, after signaling authenticates the token and before a client message
reaches the server. The same filter applies to legacy `"blit"` data channels
and virtual streams on a `"mux"` channel.

The profile is deny-by-default. It forwards only these client operations:

- Connection and delivery accounting: `ACK`, `PING`, `CLIENT_FEATURES`, and
  `CLIENT_METRICS`.
  The client-control family is blocked in full: `KICK` because it is a write,
  and `CLIENT_LIST` / `CLIENT_WATCH` / `CLIENT_UNWATCH` because enumerating the
  other viewers discloses pty and surface ids this consumer was never offered,
  plus a once-a-second sample of another viewer's bandwidth in both
  directions — upstream especially, which tracks how fast that viewer is
  typing or uploading.
- Terminal viewing: `SCROLL`, `FOCUS`, `SUBSCRIBE`, `UNSUBSCRIBE`, `SEARCH`,
  `READ`, and `COPY_RANGE`.
- Surface viewing: `SURFACE_LIST`, `SURFACE_CAPTURE`, `SURFACE_SUBSCRIBE`,
  `SURFACE_UNSUBSCRIBE`, and `SURFACE_ACK`.
- Clipboard reads: `CLIPBOARD_LIST` and `CLIPBOARD_GET`.
- Audio listening: `AUDIO_SUBSCRIBE` and `AUDIO_UNSUBSCRIBE`.
- Media observation: only `MEDIA_CONTROL(MPRIS_SUBSCRIBE)` with an exact
  enabled byte of `0` or `1`. Media capabilities, leases/data, portal replies,
  ScreenCast revocation, and MPRIS actions remain blocked.

Every other opcode, including unknown future opcodes, is silently dropped.
Consequently read-only peers cannot send input, create/restart/kill/close a
PTY, focus or control a Wayland surface, write either clipboard selection,
shut down the server, or access the filesystem, Git, LSP, KV, and network
families.

Read-only peers never participate in shared sizing. `RESIZE` and all create
opcodes are blocked, so they cannot add a PTY view-size constraint.
`SURFACE_RESIZE` is blocked, so they cannot add a surface-size constraint or
raise the compositor output scale. The optional width and height on the
allowed `SURFACE_SUBSCRIBE` are a fixed encode box for that peer alone; such a
scaled subscription is explicitly excluded from surface-size mediation.
Likewise, `CLIENT_FEATURES` can cap that peer's decoder/encoder path but has no
shared sizing input without a `SURFACE_RESIZE` entry.

## Server → Client (S2C)

| Opcode | Name                   | Layout                                                                                                                                                                                                                                                                                                                                      |
| ------ | ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0x00` | `UPDATE`               | `[pty_id:2][lz4-compressed-frame]`                                                                                                                                                                                                                                                                                                          |
| `0x01` | `CREATED`              | `[pty_id:2][tag:N]`                                                                                                                                                                                                                                                                                                                         |
| `0x02` | `CLOSED`               | `[pty_id:2]`                                                                                                                                                                                                                                                                                                                                |
| `0x03` | `LIST`                 | `[count:2][entries…]`                                                                                                                                                                                                                                                                                                                       |
| `0x04` | `TITLE`                | `[pty_id:2][title:N]`                                                                                                                                                                                                                                                                                                                       |
| `0x05` | `SEARCH_RESULTS`       | `[request_id:2][results…]`                                                                                                                                                                                                                                                                                                                  |
| `0x06` | `CREATED_N`            | `[nonce:2][pty_id:2][tag:N]`                                                                                                                                                                                                                                                                                                                |
| `0x07` | `HELLO`                | `[version:2][features:4][boot_generation:8][server_version_len:2][server_version:N]`                                                                                                                                                                                                                                                        |
| `0x08` | `EXITED`               | `[pty_id:2][exit_status:4][reason:1]` — `reason` appended; older servers omit it                                                                                                                                                                                                                                                            |
| `0x09` | `READY`                | (no payload)                                                                                                                                                                                                                                                                                                                                |
| `0x0A` | `TEXT`                 | `[nonce:2][pty_id:2][total_lines:4][offset:4][text:N]`                                                                                                                                                                                                                                                                                      |
| `0x0B` | `PING`                 | _(empty)_ — server keepalive                                                                                                                                                                                                                                                                                                                |
| `0x0C` | `QUIT`                 | _(empty)_ — server shutting down                                                                                                                                                                                                                                                                                                            |
| `0x0D` | `USED_ROWS`            | `[pty_id:2][used_rows:2]`                                                                                                                                                                                                                                                                                                                   |
| `0x0E` | `TERM_CWD`             | `[nonce:2][cwd_len:2][cwd:N]` — reply to `C2S_TERM_CWD`; empty = unknown                                                                                                                                                                                                                                                                    |
| `0x0F` | `TERM_CWD_EVENT`       | `[pty_id:2][cwd:N]` — unsolicited push when the OSC 7-reported cwd changes                                                                                                                                                                                                                                                                  |
| `0x10` | `CREATE_FAILED`        | `[nonce:2][status:1][detail:N]` — refusal of a `CREATE2(WANT_STATUS)`                                                                                                                                                                                                                                                                       |
| `0x11` | `SCROLL_OFFSET`        | `[pty_id:2][offset:4]` — this client's scrolled-back view was re-anchored (see Scrollback)                                                                                                                                                                                                                                                  |
| `0x12` | `CLIENT_LIST`          | `[nonce:2][self_id:8][count:4][client:N]…` — sorted connection records, including the requester                                                                                                                                                                                                                                             |
| `0x13` | `KICK_RESULT`          | `[nonce:2][status:1][detail:N]` — correlated result of `C2S_KICK`                                                                                                                                                                                                                                                                           |
| `0x14` | `KICKED`               | `[reason:N]` — another client kicked this connection; the server closes it after delivery                                                                                                                                                                                                                                                   |
| `0x20` | `SURFACE_CREATED`      | `[surface_id:2][parent_id:2][w:2][h:2][title_len:2][title:N][app_id_len:2][app_id:M]`                                                                                                                                                                                                                                                       |
| `0x21` | `SURFACE_DESTROYED`    | `[surface_id:2]`                                                                                                                                                                                                                                                                                                                            |
| `0x22` | `SURFACE_FRAME`        | `[surface_id:2][timestamp:4][flags:1][w:2][h:2][data:N]`                                                                                                                                                                                                                                                                                    |
| `0x23` | `SURFACE_TITLE`        | `[surface_id:2][title:N]`                                                                                                                                                                                                                                                                                                                   |
| `0x24` | `SURFACE_RESIZED`      | `[surface_id:2][w:2][h:2]`                                                                                                                                                                                                                                                                                                                  |
| `0x25` | `CLIPBOARD_CONTENT`    | `[mime_len:2][mime:N][data_len:4][data:M]`                                                                                                                                                                                                                                                                                                  |
| `0x26` | `SURFACE_LIST`         | `[count:2]` repeated `[surface_id:2][parent_id:2][w:2][h:2][title_len:2][title:N][app_id_len:2][app_id:M]`                                                                                                                                                                                                                                  |
| `0x27` | `SURFACE_CAPTURE`      | `[surface_id:2][width:4][height:4][image_data:N]` — PNG or AVIF                                                                                                                                                                                                                                                                             |
| `0x28` | `SURFACE_APP_ID`       | `[surface_id:2][app_id:N]`                                                                                                                                                                                                                                                                                                                  |
| `0x29` | `SURFACE_CURSOR`       | `[surface_id:2][type:1]` + `[name_len:1][name:N]` if named, nothing if hidden, `[hotx:2][hoty:2][w:2][h:2][png:N]` if custom; `w`/`h` and the hotspot are both **logical** pixels while the PNG keeps the cursor buffer's own resolution, so one scale factor places both. `surface_id` is the surface being _hovered_, not the focused one |
| `0x2A` | `SURFACE_ENCODER`      | `[surface_id:2][name][0x00][codec_string]` — encoder display name + WebCodecs codec string, NUL-separated                                                                                                                                                                                                                                   |
| `0x2B` | `FRAGMENT`             | `[flags:1][chunk:N]` — see [Fragmentation](#fragmentation)                                                                                                                                                                                                                                                                                  |
| `0x2C` | `CLIPBOARD_LIST`       | `[count:2] repeated{ [mime_len:2][mime:N] }`                                                                                                                                                                                                                                                                                                |
| `0x2D` | `SURFACE_ACTIVATED`    | `[surface_id:2]` — the Wayland client asked for its toplevel to be activated (xdg_activation_v1); raise and focus the pane                                                                                                                                                                                                                  |
| `0x2E` | `CLIPBOARD_OWNER`      | `[wayland:1]` — `1` while a Wayland client owns the selection; `0` when empty or externally owned                                                                                                                                                                                                                                           |
| `0x2F` | `SURFACE_TEXT_INPUT`   | `[surface_id:2][flags:1][content_hint:4][content_purpose:4]` — committed `zwp_text_input_v3` state; flags bit 0 is enabled and bit 1 marks a fresh enable request                                                                                                                                                                           |
| `0x30` | `AUDIO_FRAME`          | `[timestamp:4][flags:1][data:N]`                                                                                                                                                                                                                                                                                                            |
| `0x31` | `SURFACE_REMOTE_INPUT` | `[surface_id:2][kind:1][count:1][x:2,y:2]*` — where another viewer is pointing (`kind` 0, one point) or touching (`kind` 1, one per finger); `count = 0` retires the marks and is what the driving viewer receives                                                                                                                          |
| `0x32` | `TRAY_UPDATE`          | `[flags:1][records:LZ4]`; staged normalized tray state, see [tray/notification design](design/tray-notifications.md#server-to-client)                                                                                                                                                                                                       |
| `0x33` | `TRAY_MENU`            | `[tray_id:4][tray_revision:4][menu_revision:4][status:1][nodes:LZ4]`; see [tray/notification design](design/tray-notifications.md#server-to-client)                                                                                                                                                                                         |
| `0x34` | `NOTIFICATION_UPDATE`  | `[flags:1][records:LZ4]`; staged normalized active notifications, see [tray/notification design](design/tray-notifications.md#server-to-client)                                                                                                                                                                                             |
| `0x35` | `MEDIA_CONTROL`        | `[subtype:1][payload:N]`; runtime/privacy state, lease/credit/revocation, portal prompts, and MPRIS updates/results, see [media devices and portals](design/media-devices-portals.md#server-to-client-control)                                                                                                                              |
| `0x40` | `FS_SYNCED`            | `[nonce:2][sync_id:2][status:1][detail_len:2][detail:N]`                                                                                                                                                                                                                                                                                    |
| `0x41` | `FS_UPDATE`            | `[sync_id:2][update_id:4][flags:1][records:LZ4]`                                                                                                                                                                                                                                                                                            |
| `0x42` | `FS_FILE`              | `[nonce:2][status:1][data:LZ4]`                                                                                                                                                                                                                                                                                                             |
| `0x43` | `FS_CLOSED`            | `[sync_id:2][reason:1]`                                                                                                                                                                                                                                                                                                                     |
| `0x44` | `FS_DONE`              | `[nonce:2][status:1][hash:16][mtime_ns:8]` — one per `FS_WRITE`/`FS_OP` ([design/fs-write.md](design/fs-write.md))                                                                                                                                                                                                                          |
| `0x45` | `FS_SEARCH`            | `[nonce:2][status:1][count:2] repeated{ [path_len:2][path:N] }` ([design/fs-search.md](design/fs-search.md))                                                                                                                                                                                                                                |
| `0x46` | `FS_INDEX`             | `[nonce:2][status:1][flags:1][count:4][paths:LZ4]` ([design/fs-search.md](design/fs-search.md))                                                                                                                                                                                                                                             |
| `0x49` | `FS_UPLOAD_BEGIN`      | `[nonce:2][status:1][upload_id:2][hash:16][mtime_ns:8]` — `upload_id` meaningful only on `OK`; `hash` is the current on-disk hash on `CONFLICT`                                                                                                                                                                                             |
| `0x4A` | `FS_UPLOAD_CHUNK`      | `[upload_id:2][status:1][received:8]` — per-chunk ack/progress; `received` is the resume point on `OFFSET_MISMATCH`                                                                                                                                                                                                                         |
| `0x4B` | `FS_UPLOAD_FINISH`     | `[nonce:2][status:1][hash:16][mtime_ns:8]` — the `FS_DONE` payload on success (zeroes otherwise)                                                                                                                                                                                                                                            |

**Notes:**

`S2C_HELLO` is the first message sent on every new connection. `version` is the server's protocol version. `boot_generation` is an opaque little-endian identifier generated once per server process; clients can compare it across reconnects to detect a server restart. `server_version` is the server's release string (its crate version, e.g. `0.40.1`) — informational only: feature negotiation always goes through the feature bits, never a version comparison. Both trailing fields were appended without a protocol bump, so legacy servers omit them and clients must treat a short `HELLO` as valid. `features` is a 4-byte bitmask:

| Bit | Name                 | Meaning                                                         |
| --- | -------------------- | --------------------------------------------------------------- |
| 0   | `CREATE_NONCE`       | Server supports `CREATE2` / `CREATED_N` with nonce correlation  |
| 1   | `RESTART`            | Server supports `C2S_RESTART` to respawn exited PTYs            |
| 2   | `RESIZE_BATCH`       | Server accepts batched resize entries in a single `C2S_RESIZE`  |
| 3   | `COPY_RANGE`         | Server supports range-based text copy                           |
| 4   | `COMPOSITOR`         | Server supports headless Wayland compositor                     |
| 5   | `AUDIO`              | Server supports audio forwarding (PipeWire capture + Opus)      |
| 6   | `FS`                 | Server supports the `FS_*` filesystem sync family               |
| 7   | `GIT`                | Server supports the `GIT_*` git introspection family            |
| 8   | `LSP`                | Server supports the `LSP_*` language intelligence family        |
| 9   | `KV`                 | Server supports the `KV_*` key-value family                     |
| 10  | `NET`                | Server supports the `NET_*` network-relay family                |
| 11  | `EXTENSION`          | Proposed: Wasmi extension lifecycle, events, and commands       |
| 12  | `CHANNEL`            | Proposed: server supports bidirectional named channels          |
| 13  | `PROCESS`            | Server supports native non-PTY child processes                  |
| 14  | `CREATE_STATUS`      | `CREATE2(WANT_STATUS)` receives an explicit failure             |
| 15  | `KILL_MODE`          | `KILL`/`CLOSE` reach the process group; `KILL` takes `flags`    |
| 16  | `PTY_DEADLINE`       | `C2S_DEADLINE`, `CREATE2(HAS_DEADLINE)`, and `EXITED.reason`    |
| 17  | `SCROLL_BY`          | Scrollback holds still: `S2C_SCROLL_OFFSET` and `C2S_SCROLL_BY` |
| 18  | `SURFACE_TOUCH`      | Server accepts direct contacts and exposes `wl_seat.touch`      |
| 19  | `SURFACE_TEXT_INPUT` | Server forwards committed `zwp_text_input_v3` state             |
| 20  | `CLIENT_CONTROL`     | Enumerate connections and kick another client with a reason     |
| 21  | `DESKTOP`            | Compositor tray/notification state bridge and core API are live |
| 22  | `DESKTOP_MEDIA`      | Viewer media, portals, and MPRIS control family is understood   |

Bits 11 and 12 remain proposed for the extension and channel families under
review in [#167](https://github.com/indent-com/blit/pull/167) and
[#173](https://github.com/indent-com/blit/pull/173). Bit 13 is advertised when
native process execution is enabled. Bits 14 and 20 are always advertised.

Bits 11 through 13 are independently omitted when `BLIT_EXT=0`,
`BLIT_CHANNEL=0`, or `BLIT_PROCESS=0`; disabled-family requests are refused as
specified in [design/extensions.md](design/extensions.md#security-posture-and-deployment-controls)
and [design/processes.md](design/processes.md#security-and-deployment).
Bit 14 is not extension-specific and is not controlled by those gates. It is
advertised only after the server implements the negotiated creation outcome
below; the implementation plan updates both shipped clients before enabling it.

### Common status registry

New request/reply families should use this registry for a one-byte `status`
unless their wire definition explicitly declares a message-local table.
Existing message-local tables such as `FS_SYNCED` and `NET_OPENED` are
grandfathered and do not share all of these numeric meanings.

| Value | Name            | Meaning                                                      |
| ----: | --------------- | ------------------------------------------------------------ |
|     0 | `OK`            | Request completed successfully                               |
|     1 | `UNKNOWN_ID`    | Requested identifier or handle is absent or already closed   |
|     2 | `NOT_FOUND`     | Path, object, symbol, or backend does not exist              |
|     3 | `WRONG_TYPE`    | Existing object cannot satisfy this operation                |
|     4 | `PERMISSION`    | Operation is disabled or denied                              |
|     5 | `TOO_LARGE`     | Input or result exceeds a size ceiling                       |
|     6 | `BUDGET`        | A resource budget is exhausted without pagination/truncation |
|     7 | `INVALID`       | Request encoding, flags, or field combination is invalid     |
|     8 | `CANCELLED`     | Operation ended through its cancellation mechanism           |
|     9 | `OTHER`         | Unclassified backend failure; detail should diagnose it      |
|    10 | `WARMING`       | LSP backend is not ready; retry later                        |
|    11 | `CONFLICT`      | A revision, lock, or compare-and-swap precondition failed    |
|    12 | `NO_MERGE_BASE` | Valid Git histories have no common ancestor                  |

Values 0–127 are centrally allocated common statuses; 13–127 are currently
reserved. New family-local allocations use 128–255 and must be defined by the
packet which carries them. Existing message-local tables retain their shipped
values. Consumers render unknown values distinctly from `OTHER`.

`S2C_LIST` entry layout: `[pty_id:2][tag_len:2][tag:N][cmd_len:2][cmd:M]` per
PTY. The trailing command field is a backward-compatible extension; old
entries without it parse as an empty command.

When `HELLO` advertises `CREATE_STATUS`, shipped clients set
`CREATE2.WANT_STATUS`. Once that request's nonce and feature byte are
decodable, it receives exactly one outcome: `CREATED_N` on success or
`CREATE_FAILED` on refusal. `CREATE_FAILED.status` uses the common registry and
`detail` is diagnostic UTF-8, truncated to 1 KiB on a character boundary. In
particular, a projected `LIST` overflow and PTY-ID or configured PTY-cap
exhaustion return `BUDGET`, an unrepresentable tag or command returns
`TOO_LARGE`, malformed fields return `INVALID`, and spawn failure returns
`OTHER`. A tag or command that cannot round-trip `S2C_LIST`'s `u16` length
prefixes is refused rather than silently truncated into a corrupt catalog
frame.

Both halves are enforced at creation, the only point where either can change: a
terminal's `tag` and `command` are fixed once it exists, so the catalog's
encoded size only ever grows by an entry that a create put there. A create whose
own entry is representable but which would push the complete `S2C_LIST` past the
ceiling is refused with `BUDGET`. The ceiling is 64 MiB, the same
`MAX_DECOMPRESSED` every reassembling client already enforces — not the 16 MiB
frame size, which fragmentation makes irrelevant to a logical message (see
"Fragmentation"). The bound is what a client will accept, not what fits in one
frame. Creation is also the only place a refusal can be delivered, so the older
create opcodes, which have no failure reply, refuse to the server log instead of
sending anything.

The projection is computed from the live catalog on each create rather than
carried as a running total, so it cannot drift from what the encoder emits; a
count that disagreed with the bytes would be the corrupt frame the check exists
to prevent. Connecting preflights the same number before building the initial
burst: an over-cap catalog aborts the bootstrap with a server diagnostic, since
sending one would make the client drop the connection with nothing logged at
either end.

The PTY cap is `--max-ptys` / `BLIT_MAX_PTYS`, unlimited by default. It counts
_live_ terminals only — a client that runs a hundred short
commands is not holding a hundred terminals, and counting the exited ones
would refuse it work with nothing actually running.

Exited terminals are bounded separately. Their output stays readable after the
command ends, and nothing but an explicit `CLOSE` used to remove one, so the
server keeps at most `BLIT_MAX_EXITED` of them (default 1024) and evicts the
oldest first. Eviction takes the same path a `CLOSE` would and broadcasts the
same `CLOSED`, so no client change is needed to follow it. `BLIT_EXITED_LINGER`
adds a time bound in seconds; it is off by default, because how long a result
stays interesting is not something the server can know.

A terminal has no deadline unless a client arms one — detaching and coming
back is the point of a multiplexer, so sessions do not expire on their own.
`C2S_DEADLINE` and `CREATE2(HAS_DEADLINE)` opt in, and the server enforces it
whether or not anyone is still connected, which is the difference from every
client-side timeout. `ms` counts from when the server receives the message, so
re-sending refreshes: repeat it on an interval and it becomes a dead-man
switch, killing the terminal roughly one period after the orchestrator stops
checking in. `ms = 0` clears the deadline and stands down an in-flight stop.

On expiry the server sends SIGTERM to the process group, waits 5 s, then sends
SIGKILL, and the resulting `EXITED` carries `reason = 1` (`DEADLINE`). The
`reason` byte exists because a deadline kill is otherwise indistinguishable
from a user's `kill -9`: `0` normal, `1` deadline, `2` lease, `3` gc,
`4` unit-stop. Only `0` and `1` are sent today — `2` and `4` are reserved for
[design/units.md](design/units.md), and `3` is unused because retention
eviction only ever touches a terminal that has already sent its `EXITED`, and
signals itself with `CLOSED`. The byte is
appended and length-gated, like the trailing fields on `HELLO`, so a 7-byte
`EXITED` from an older server reads as `NORMAL`.

`KILL` and `CLOSE` signal the child's process group on Unix and terminate its
job object on Windows. `KILL`'s trailing `flags` byte is optional and armed by
a message length of 8; bit 0 (`LEADER_ONLY`) restores the older behaviour of
signalling the session leader alone, which is what a caller emulating a
keystroke wants. A 7-byte `KILL` gets the group, so a client needs no change
to stop leaking a killed shell's children. Group delivery reaches the leader's
own group and, through `TIOCGPGRP`, the terminal's foreground group; a
backgrounded job sits in neither and survives. Containing that needs a cgroup,
not a signal. On Windows the job carries
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so dropping the handle takes any
survivor with it; if the job cannot be created the PTY still runs and
degrades to a leader-only kill.

`CLOSE`'s hangup escalates the same way an expiry does: SIGHUP to the group,
wait 5 s, then SIGKILL to the group, so a child that ignores SIGHUP — or whose
descendants inherited that disposition — does not outlive its terminal. The
escalation is invisible on the wire. `CLOSED` still arrives immediately and
still means the slot is gone: the terminal leaves the catalog when the hangup
goes out, and the rest is carried by the pid alone rather than by a retained
"closing" entry, which would count against the PTY cap and never reach
retention. A client learns nothing about, and waits for nothing in, the grace.
Reaping beats escalating, not the other way round: once a child has been waited
its pid may name an unrelated process group, so a hangup the child answered
promptly cancels the pending SIGKILL instead of firing it late.

This is opt-in rather than a reinterpretation of `CREATED_N`; a legacy client
cannot mistake an error for PTY zero. `CREATE`, `CREATE_AT`, `CREATE_N`, and
`CREATE2` without negotiated `WANT_STATUS` retain their existing success-only
contract: the server refuses an inadmissible mutation without sending
`CREATED` or `CREATED_N`. A client must not set `WANT_STATUS` unless the server
advertised bit 14. A server must not send `CREATE_FAILED` for a request which
did not set it.

`S2C_EXITED` exit status: `WEXITSTATUS` for normal exits (0, 1, …); negative signal number for signal deaths (-9 = SIGKILL); `i32::MIN` when status is unknown.

### Scrollback

`C2S_SCROLL` (`0x02`) names a position as `offset` lines above the live bottom, per client — one terminal can have several viewers reading different parts of it. That makes the offset a moving target: every line the app pushes off the top of the viewport slides the text under anyone reading the scrollback. A shell is quiet enough for it to go unnoticed; an agent streaming output is not.

The server holds those viewers still. Each tick it asks the driver how many lines actually scrolled (including once the scrollback is full and its depth stops growing, where the number is no longer inferable from the frames), grows every non-zero offset by that much, clamps it to the deepest offset that still has content, and reports the result as `S2C_SCROLL_OFFSET` (`0x11`). Sent only to a client that is scrolled back, only when its offset moved.

A client adopts the value: its own copy of the offset feeds the scrollbar, its selection anchors, and the next scroll request it sends, all of which have to keep naming the same rows the frames do. Because the frame that accompanies the re-anchor deepens the scrollback by the same number of lines, the position on screen does not move — that is the point. A frame is sent for a change in scrollback depth alone, so a client whose content is being held still still learns how deep the history under it now goes.

That leaves the client's own requests. An absolute offset only means what the user intended for as long as the bottom it counts from stays put, and the whole reason this section exists is that under a chatty app it does not: the request is computed from a view that is one round trip old, and lands short by however many lines scrolled while it was in flight. `C2S_SCROLL_BY` (`0x1E`, gated on `FEATURE_SCROLL_BY`) states the motion instead of the destination — the server applies it to whatever offset it currently holds, clamps, and answers with `S2C_SCROLL_OFFSET`. Every incremental gesture uses it: a wheel notch, a page key, a selection drag running off the edge. Absolute `C2S_SCROLL` stays right for the requests that really are absolute — home, end, dragging the scrollbar, and returning to the live tail — and remains the fallback against a server that does not advertise the bit.

### Working directory tracking

Two complementary paths report a PTY's working directory:

- **Push** (`S2C_TERM_CWD_EVENT`, `0x0F`): the server scans PTY output for OSC 7 (`ESC ] 7 ; file://<host><path> (BEL|ST)`), which shell integration emits at every prompt. A report is accepted only when `<host>` names the server machine (empty, `localhost`, or the server's hostname, case-insensitively — a remote-ssh shell reports the _remote_ host, and its path is not a local path), the percent-decoded path is absolute valid UTF-8 without NUL, and it is at most `TERM_CWD_MAX` (4096) bytes. Accepted reports overwrite the per-PTY stored value (last write wins); the event is broadcast to every connected client — the same fan-out as `TITLE` and `USED_ROWS` — and only when the stored value _changes_, so per-prompt re-reports of an unchanged directory produce no traffic. `cwd` is the remainder of the message, with no length prefix.
- Shell-side setup for the emitting sequence lives in [shell-integration.md](shell-integration.md) (fish emits OSC 7 natively; zsh/bash need a hook).
- **Poll** (`C2S_TERM_CWD` `0x1C` → `S2C_TERM_CWD` `0x0E`): request/reply correlated by nonce. The reply prefers the PTY's stored OSC 7 value — it is fresher (the interactive shell's prompt-time cwd, not whichever pid the kernel happens to track) and costs no syscall. When the shell has never reported (no OSC 7 integration), the server falls back to asking the kernel about the PTY child (`/proc/<pid>/cwd` on Linux, `proc_pidinfo` on macOS). The poll therefore remains the fallback for shells without OSC 7; clients with OSC 7-integrated shells see pushes arrive ahead of any poll.

Clients that predate `TERM_CWD_EVENT` are unaffected: consistent with the version-stability rule above (new message types are added under new opcodes), both reference clients drop unrecognized S2C opcodes — `js/core`'s `BlitConnection.handleMessage` dispatch falls through to a no-op `default`, and the CLI's message matches end in a catch-all `_ => {}`.

An opcode which multiplexes a one-byte inner kind also needs a family-defined
skip rule. The proposed extension-command and native-channel families specify
that clients ignore unknown S2C kinds, servers ignore unknown C2S kinds without
changing handle state, and any new request kind which requires a reply is
separately feature-negotiated; see
[design/extensions.md](design/extensions.md#protocol-compatibility).

`S2C_SURFACE_FRAME` flags byte: bit 0 is the keyframe flag; bits 1–2 encode the codec — H.264 (0), AV1 (1), PNG (2). Bit 3 means a `[timestamp_sub_us:2]` field appears between the base header and encoded data. The base `timestamp` is a wrapping monotonic millisecond counter captured at compositor-commit time (not wire-send time); `timestamp_sub_us` is its 0–999 µs fractional part. The server only sends the extended layout when `C2S_CLIENT_FEATURES.client_features` bit 0 is set. Bits 4–7 remain reserved.

Each `(client, surface)` pair runs at most one server-side encoder, sized from that client's view size — or from its scaled subscribe, which overrides it. Multiple mounts on the same client share one subscription, and the size sent on the wire is derived across them: any mount wanting the surface unscaled wins outright, otherwise the largest requested size does. (Shrinking a stream further is cheap; the reverse is lossy.) `S2C_SURFACE_FRAME` is broadcast to every subscribed client.

`S2C_AUDIO_FRAME` carries Opus-encoded audio from the compositor's mixed output. `timestamp` is a sample offset in 48 kHz ticks. `flags` bits 1-2 encode the codec (0 = Opus). Audio is per-compositor (one mixed stream from all apps), not per-surface. Only sent when the `AUDIO` feature bit is set in `S2C_HELLO`.

`C2S_AUDIO_SUBSCRIBE` carries a `bitrate_kbps` field (little-endian u16): the desired Opus bitrate in kbps, e.g. 64 for 64 kbps. `0` means server default. Clients may re-send `AUDIO_SUBSCRIBE` to adjust bitrate without unsubscribing first. When multiple clients are subscribed, the server uses the highest requested bitrate.

### Clipboard

`data` in `C2S_CLIPBOARD_SET` is opaque bytes and `mime` says what they are —
`image/png` for a pasted screenshot as readily as `text/plain;charset=utf-8`
for text. The compositor stores the pair as the external selection and
advertises exactly that type on the `wl_data_offer` it hands to Wayland
clients, so what an app can paste is what the browser actually had.

Text alone picks up the conventional aliases: a selection whose type begins
`text/plain` is additionally offered as `text/plain`,
`text/plain;charset=utf-8` and `UTF8_STRING`, and answers a `receive` for any
of them. No other type aliases, in either direction — an app that asks an
image selection for `text/plain` gets an empty pipe, because bytes delivered
under a type they are not are indistinguishable, to the client, from bytes
that are.

One `CLIPBOARD_SET` replaces the whole selection; there is no way to offer a
second representation of the same copy. A browser holding both a text and an
image form of one clipboard therefore has to choose, and `@blit-sh/core`
sends the text — the picture of a spreadsheet range is rarely what pasting it
is meant to produce. The payload is bounded by the 16 MiB frame ceiling like
any other message, and is not fragmented; the reference client refuses to send
an image above 8 MiB rather than have the frame refused, and cancels the paste
outright instead of letting the keystroke land on a selection it did not
update.

A selection owned by a Wayland client does not take that browser round trip.
The compositor advertises the owner's MIME list to every `wl_data_device` and
splices each accepted `receive` fd directly back to its `wl_data_source`.
This is load-bearing for non-text selections: copying `image/png` in one
streamed app and pasting it in another works without the browser ever reading
or rewriting the image. Ownership is exclusive in either direction; a
`CLIPBOARD_SET` cancels the prior Wayland owner, and a client selection clears
the stored external one. Each offer pins the owner or bytes it advertised, so
a late receive on an old offer cannot read a replacement selection.

`S2C_CLIPBOARD_OWNER` makes that authority explicit to every web client and
is replayed before `READY` on connect. While it is `1`, Ctrl/Cmd+V cancels the
browser's native paste and forwards the shortcut without a `CLIPBOARD_SET`,
preserving the app owner's full MIME set for the direct splice. The web client
marks that status unknown only when the window/tab loses authority or a real
DOM copy/cut occurs; its next paste then reads the browser clipboard, sends
`CLIPBOARD_SET`, and becomes the external owner. Merely moving focus between
streamed surfaces does not invalidate the Wayland owner.

Pasting that selection into a browser-rendered terminal uses its text
representation directly. The web client keeps the unsolicited
`CLIPBOARD_CONTENT` emitted with a Wayland text selection in memory; a client
that connected after the copy, or missed the eager content, obtains the MIME
list with `CLIPBOARD_LIST` and reads the preferred plain-text type with
`CLIPBOARD_GET`. This path does not depend on permission for a background
`navigator.clipboard.writeText`.

### Primary selection

Middle-click paste reads PRIMARY, which has two possible owners.

A Wayland client owns it by setting a `zwp_primary_selection_source_v1`: the
compositor offers it to every bound device and splices a `receive` straight
through to the owner, never buffering the bytes or seeing them. Selecting
text in one app and middle-clicking in another works with the browser out of
the picture entirely.

The browser owns it with `C2S_PRIMARY_SET` (0x33), same framing as
`CLIPBOARD_SET`. The web platform exposes no PRIMARY to read on demand, so
the bytes arrive up front and the compositor serves them itself. The
reference client sends them on the middle press rather than on every
selection change — the way the clipboard is pushed on paste rather than on
copy — because owning PRIMARY continuously would permanently displace
whichever Wayland client the user last selected text in. A middle click with
nothing selected in the page therefore still pastes that client's selection.

Ownership is exclusive: whichever side claims PRIMARY displaces the other,
and a displaced Wayland owner is told with `cancelled` so it stops answering
`receive` from its own buffer. `blit clipboard set --primary` claims it from
the CLI. Pasting _from_ the browser with Ctrl+V remains the clipboard's job.

### Drag and drop

Dragging an OS file from the user's desktop into a streamed app is a real
`wl_data_device` drag session, driven by the compositor with no client
`wl_data_source` behind it (`wl_data_device.enter`'s source is allow-null
for exactly this case). The browser tracks HTML5 drag events over the
surface element and reports them with the `SURFACE_DRAG_*` family; the
server keeps one session per connection and the compositor enters, motions
and drops on the target surface like any other drag. `surface_id`, `x` and
`y` are encoded exactly as in `C2S_SURFACE_POINTER` — LE u16s in the
composited frame's physical pixel space — and go through the same
logical-coordinate conversion and hit-test as pointer motion.

`SURFACE_DRAG_ENTER` (0x35) starts the session and carries the MIME types
the browser can offer; the list reaches the app unchanged on the
compositor-owned `wl_data_offer`. For a file drag it may also append an item
plan: one MIME per file-kind `DataTransferItem`, in item order. The reference
browser sends a plan only when every item exposes a MIME with a useful
extension; WebKit items that are typeless during hover omit it rather than
committing the eventual file to `.bin`. iPad screenshots are the deliberate
exception: WebKit exposes only the `Files` marker until DROP, so the client
sends a one-file PNG plan. That makes the final `0.png` URI available during
hover, giving Chromium time to deliver a file-shaped `dragenter` to the remote
page before release. Once DROP exposes the representation, iPad HEIC/HEIF is
decoded and re-encoded as PNG in the browser to match that plan. Chromium-backed
destinations that do not claim those Apple formats can otherwise navigate to
the staged URI instead of accepting the image drop. If another representation
materializes, or conversion is unavailable, the client sends a replacement
ENTER with the truthful type and name rather than losing the file. The trailer
is optional and append-only, so an ENTER without it is byte-identical to the
original format.
The server derives a staging name for every planned item
(`0.png`, `1.jpg`, `.webp`/`.gif`, HEIF-family formats, TIFF, or BMP; unknown
types use `.bin`), creates the empty files, and can therefore answer
`receive("text/uri-list")` during
hover. Chromium fetches that URI list at Wayland enter before delivering
the page's `dragenter`; answering it immediately lets the remote app show
its drop UI before release.

A second ENTER retargets the session: the old surface gets `leave`, the new
one gets `enter`. `SURFACE_DRAG_MOTION` (0x36) and `SURFACE_DRAG_LEAVE`
(0x37) forward as `motion` and `leave`. `SURFACE_DRAG_CANCEL` (0x39) aborts
— Escape, or the drag leaving the browser window. DROP, CANCEL and
connection close all end the session.

The actual bytes arrive only through the staging upload or inline in
`SURFACE_DRAG_DROP` (0x38), whose payload is a list of items
`[mime_len:2][mime][name_len:2][name][data_len:4][data]`. The distinction
that matters is `name`:

- An item with a non-empty `name` is a file, and its bytes are not in the
  message: `data_len` is 0 and `name` is a path relative to the
  connection's drag staging dir naming a file the client already uploaded
  through the fs family. With an ENTER plan this must be the exact derived
  name already present in the hover URI list; both item count and names are
  checked at DROP. A client without the optional plan may use another safe
  relative name (the original implementation used `0-shot.png`). The
  staging dir is opened with an ordinary `FS_SYNC`
  carrying the `FS_SYNC_STAGING` flag (bit 9): the `path` field is ignored
  (send it empty), the server resolves the sync root to the per-connection
  staging dir — creating it on first use — and the file bytes ride the
  chunked `FS_UPLOAD` path with its pacing and cancel like any other
  upload, so a large drop never queues one giant frame ahead of
  interactive input. `FS_SYNC_STAGING` combined with `FS_SYNC_FROM_PTY` is
  invalid and gets the usual invalid-flag refusal. The dir is removed on
  connection close, never on `FS_STOP`: the staged URIs must outlive the
  drop. A `name` that escapes the staging root (`../x`, absolute) or names
  no uploaded file abandons the drop — the session ends with no offer. The
  server offers `text/uri-list`: RFC 2483 `file://` URIs, percent-encoded
  (spaces and non-ASCII bytes become `%XX`), one per staged item,
  CRLF-terminated. When the drop is a single named item its own mime is
  offered too, served from the staged file's bytes — an app that pastes
  content rather than opening files still gets it.
- A name-less item is dragged content (text, HTML) and is offered directly
  under its own mime with its inline bytes, never touching the staging
  dir. Being small by nature, it is also the only part of a drop the
  16 MiB frame cap still bounds.

The app reads the data the usual Wayland way. A planned `text/uri-list`
receive can complete between `enter` and `drop`; every other early receive
is parked until DROP supplies its payload. Receives after DROP are served
from that payload by MIME (an unoffered MIME gets an empty pipe). The
compositor sends `drop` followed by the terminal `leave`, clearing the
destination's drag UI while the offer remains valid for post-drop reads and
`finish`. With no source there is no
`dnd_drop_performed`/`dnd_finished` — nobody to notify. Client-initiated
drags (a Wayland app starting one via `start_drag`) take the complementary
path: while the physical button is held, browser mouse events are hit-tested
across mounted surface canvases and drive the compositor's implicit drag grab.
Crossing surfaces sends `leave` then a fresh offer/`enter`; `receive` is fd-
spliced to the source, a valid release sends `drop`/`dnd_drop_performed`, and
target `finish` sends `dnd_finished`. Source and target action masks are negotiated;
each fresh target offer receives the source mask before `enter`, and no
selected `action` is announced until that target replies with `set_actions`.
This ordering matters to Chromium/Electron targets: they do not start their
pre-drop MIME fetches from an incomplete v3 offer.
The compositor also advertises `xdg_toplevel_drag_manager_v1`. Chromium uses
that global to start a tab drag before the pointer leaves its source window,
which lets the same cross-surface grab move tabs between browser panes. An
attached toplevel remains a workspace-managed pane rather than following a
compositor-global cursor position, and is excluded from its own drop targets.
An empty mask intersection is announced as `action(NONE)` without leaving the
surface, because the target may renegotiate on a later motion. Release becomes
a drop only with both a non-NONE action and an accepted MIME; otherwise the
target is left and the source is cancelled. The target's `accept(mime)` is
relayed as `wl_data_source.target(mime)` so the
source commits and serves the representation the destination selected;
the source mask declared before `start_drag` is retained on the data source,
as required by the Wayland request order. Releasing over no surface cancels
the source.

### Shared input marks

`S2C_SURFACE_REMOTE_INPUT` mirrors what one viewer is doing to a surface onto the
others watching it, so a shared session shows where the other person is. The
compositor has one seat, so at most one viewer drives it at a time.

`kind = 0` carries a single pointer position, drawn with that surface's current
cursor artwork (see [Cursors](#cursors)). `kind = 1` carries the live touchscreen
contacts, one point per finger on the glass — the whole set every time, not just
the contacts that changed, so a viewer draws what is actually down and a viewer
that subscribed mid-gesture is not left guessing. `count = 0` retires the marks:
that is what the driving viewer itself receives, since its own cursor and its own
fingers are already on its screen, and what everyone receives when input ends.

Coordinates are the same composited-frame pixel space as `C2S_SURFACE_POINTER`.
All these fields are unsigned, so a sender clamps into the surface rather than
letting a position in the letterbox margin wrap to ~65535.

Pointer marks come from `C2S_SURFACE_POINTER` and from `C2S_SURFACE_DRAG_MOTION`
— a browser fires no mouse events while a drag is in flight, so without the
latter the marks would sit frozen for the whole drag. Touch marks come from
`C2S_SURFACE_TOUCH`, so they appear only for a viewer in direct-touch mode. In
pointer compatibility mode a touchscreen is already emulating a pointer, and it
is that pointer which gets mirrored.

Marks are retired when their owner moves to another surface, sends
`C2S_SURFACE_POINTER` with `type = 3` (the pointer left the drawn area), ends a
drag with `SURFACE_DRAG_LEAVE` or `SURFACE_DRAG_CANCEL` (again, no mouse event
will arrive to do it), lifts its last contact, cancels or disables touch,
unsubscribes from the surface, disconnects, or the surface is destroyed.
`SURFACE_DRAG_DROP` is not in that list: it lands inside the surface at a known
position, and ordinary mouse events resume after it.

Only transitions go on the wire: an unchanged mark set, and a `count = 0` repeat
to a client that is already the owner, are both suppressed. Every one of these
messages counts against the same outbox frame budget that gates surface video and
paced terminal output, and the browser sends pointer and touch motion
unthrottled.

### Pointer buttons

The `button` byte in `C2S_SURFACE_POINTER` is DOM `MouseEvent.button`
numbering — 0 left, 1 middle, 2 right, 3 back, 4 forward — and the server
translates to evdev. Back and forward become `BTN_SIDE` and `BTN_EXTRA`, the
codes a physical mouse's thumb buttons actually emit and the ones toolkits
bind to history navigation; `BTN_BACK` and `BTN_FORWARD` exist but are
vestigial and largely unhandled. Unknown button numbers fall back to
`BTN_LEFT`.

### Scroll

`C2S_SURFACE_POINTER_AXIS2` (`0x32`) carries everything `wl_pointer` needs to describe a scroll, because the pieces are not interchangeable:

The message's `surface_id` is its dispatch target, not merely a scale hint. If
the shared Wayland seat has since entered another toplevel, the compositor
re-hit-tests the last pointer position recorded for the named surface before
delivering the axis frame. An unknown, unmapped, or pointerless target drops
the scroll instead of falling through to whichever window held focus.

- `dx`/`dy` — smooth distance ×100, positive = right/down, in the composited frame's pixel space. The server converts to surface-logical pixels using the same ratio it applies to `SURFACE_POINTER`, so a wheel and a drag move content by equal amounts on a scaled surface. Sending both axes in one message keeps a diagonal gesture in a single `wl_pointer.frame`.
- `v120_x`/`v120_y` — discrete travel in 120ths of a detent (`axis_value120`'s convention: 120 = one notch). Zero for devices without detents. Clients bound below `wl_pointer` v8 get the equivalent `axis_discrete`; sub-detent travel reaches them as smooth motion only.
- `flags` bits 0–1 — the device source, matching `wl_pointer.axis_source` (0 wheel, 1 finger, 2 continuous, 3 wheel tilt). Bit 2 marks the source as known; when clear no `axis_source` is emitted, which is what the legacy `0x22` opcode does.
- `flags` bit 3 — stop, sent with zero deltas, becoming `wl_pointer.axis_stop`. Only a finger-sourced sequence sends one: the protocol leaves a `wheel` sequence unterminated and tells clients not to rely on a stop for it.

The source matters more than it looks. `axis_source`'s zero value _is_ `wheel`, so omitting the event does not read as "unknown" to a toolkit — it reads as a notched wheel, and the spec invites clients to treat those as "discrete steps of a number of lines". A trackpad's smooth pixel stream then gets multiplied by a lines-per-click factor. On macOS, where the OS has already applied its own acceleration curve and appended a momentum tail before the browser ever sees the event, that second multiply is what made remote scrolling feel violent and non-linear. Labelling the stream `finger` is what stops the client adding kinetics of its own.

The browser sends the stop after an idle gap deliberately longer than the window a toolkit will regress a fling velocity from — Chromium's is `kFlingStartTimeoutMs`, 200ms — so that macOS's momentum tail does not get a second one grafted onto it, and so that pausing mid-gesture with fingers still down does not fling at the speed you were going before you stopped. Touch drags do want kinetic scrolling and are unaffected: they end their sequence on `touchend` instead of waiting out the timer.

`wl_pointer.axis` carries the distance in surface-local pixels, as the protocol specifies and as Mutter emits — with one exception. Chromium reads that value as detents, dividing by a hardcoded `kAxisValueScale = 10` and multiplying by `kWheelDelta = 120` for every source including `finger`, then handing the result to Blink as precise pixels; a pixel-valued axis scrolls a Chromium or Electron window exactly twelve times too far. The compositor recognises those clients by the runtime payload beside their executable and gives them the same distance in detent units — ten per detent, the convention Weston established and Mutter still emits for wheels. GTK, which reads the value as `GDK_SCROLL_UNIT_SURFACE` pixels, and winit, which hands it to Alacritty as a `PixelDelta`, keep pixels. `v120_*` is never rescaled: its unit is unambiguous and every toolkit agrees on it.

### Surface text input

`S2C_SURFACE_TEXT_INPUT` forwards state committed through
`zwp_text_input_v3.commit`. `enabled` tells the viewer whether the focused
surface currently accepts text input. `requested` is set only for a new
`enable` commit, so a browser may open its virtual keyboard without reopening
one the user already dismissed when cursor metadata changes or a viewer
reconnects. `content_hint` and `content_purpose` retain their Wayland enum
values and let the viewer choose an appropriate HTML input mode. Opening a
platform virtual keyboard remains best-effort because browsers may require a
recent user activation.
The Blit UI honors fresh requests by default; its device-local **Media →
On-screen keyboard** preference can opt out without changing the forwarded
Wayland state or the manual keyboard control.

### Direct touch

`C2S_SURFACE_TOUCH` (`0x3A`) is opt-in through an `ENABLE` control message and
requires feature bit 18. Phases are 0 down, 1 up, 2 motion, 3 cancel, 4 enable,
and 5 disable. Enable, disable, and cancel carry zero contacts. Contact
coordinates are signed, multiplied by 100, and use the composited-frame pixel
space; the compositor applies the same frame-to-logical transform as pointer
input. One contact-bearing transport message becomes one `wl_touch.frame`,
preserving browser `TouchEvent` atomicity; cancel is terminal and needs no
frame.

Wayland contact ids are compositor-local slots and are reused after `up`; they
are not the browser's `Touch.identifier`. Keeping the slot set bounded also
matters to Chromium: an ever-increasing id works for dragging but stops
producing touchscreen flings once it reaches 32.

`time_ms` is the browser's own `TouchEvent.timeStamp`. `C2S_SURFACE_POINTER` and
`C2S_SURFACE_POINTER_AXIS2` carry one for the same reason, and it is not
decoration: clients may derive velocity by differentiating position against
`wl_touch.time`, `wl_pointer.motion` or `wl_pointer.axis` — a fling, a stroke
width, a swipe — so the _spacing_ between events has to be the browser's. Over a
network, arrival times would substitute jitter for real cadence.
`C2S_SURFACE_INPUT` carries one too, so every path that has a browser event uses
it. `0` means "unknown" — the legacy axis opcode, an IME commit's synthesised
keys, the chord and modifier keys built around a real keypress, disconnect
cleanup — and takes the compositor's own clock _without_ disturbing the anchor,
since those interleave with real gestures and restarting the pacing around them
would be worse than not having their own time.

Direct touch also preserves that cadence in wall-clock delivery. Chromium's
Wayland backend currently discards the protocol's millisecond value and stamps
each `wl_touch` event when it is received. If a coalesced iPad burst is merely
given correct `wl_touch.time` values but drained in one pass, Chromium still sees
zero-time motion and suppresses the fling. The compositor therefore schedules
the frames at the browser's inter-event deltas through a small per-sequence
jitter buffer, never earlier than their arrival; steps over 100 ms start
immediately rather than turning a hold or clock jump into input latency. If
render or encode work misses several deadlines, the compositor rebases the
undispatched tail instead of catching every overdue frame up in one flush, so
each frame still reaches Chromium separately. The buffer keeps at most eight
pending motion frames and sheds its oldest motion history when playout would
trail arrival by more than 80 ms. Any contact update absent from the following
frame is merged forward, so current positions are never lost. The retained
motion tail is resampled from the last played contact positions to the newest
source positions; this avoids a discontinuity after sustained compaction that
Chromium treats as scroll motion but excludes from fling velocity. At least two
motion frames survive for velocity. Cancel remains urgent and drops that owner's
undispatched tail.

The client's epoch is its own, so only the deltas are used: the compositor anchors
to its own clock and adds the browser's deltas on top, keeping these timestamps in
one millisecond domain across the seat. It re-anchors after an idle gap, since
nothing needs continuity across a pause and a stale anchor would accumulate the
drift between the two clocks. The result is monotonic, because clients may assert
on time going backwards — but deliberately _not_ clamped to the current instant: a
batch generated before it arrived legitimately spreads across the moment it is
drained, and clamping each event to "now" would flatten the very spacing this
exists to preserve. A client whose clock runs fast re-anchors once its timestamps
get more than a second ahead.

Direct touch keeps that anchor per live browser owner rather than borrowing the
seat's pointer/key anchor. Multiple viewers can share a dev session, and every
page has an unrelated DOM timestamp epoch; using one viewer's pointer clock for
another viewer's touch sequence makes every touch timestamp look invalid and
collapses a queued iPad motion burst back onto compositor drain time.

Down binds a contact to the surface hit at that point. Later motion and up stay
on that surface, matching Wayland's implicit grab even when the contact crosses
a popup or subsurface boundary. The down serial also authorizes
`wl_data_device.start_drag`, so a touch-started drag follows that contact until
up. Starting that drag takes the seat over, so the compositor emits
`wl_touch.cancel` at `start_drag`: the client is told to forget the whole
sequence and receives no further `wl_touch` event for any of its contacts —
including the one still driving the drag, which now speaks through
`wl_data_device`. That is the only consistent option, because `cancel` has no
per-contact form, so a contact the drag swallows cannot be retired on its own;
withholding its `up` instead would leave the client holding it pressed forever.
New downs during the drag are ignored.

A contact's target unmapping, the viewer disconnecting, or direct mode being
disabled cancels the sequence the same way, and also tells the server, which
releases the ownership below rather than waiting for that browser's fingers to
lift.

Only one connection owns a live direct-touch sequence. Another viewer's down
is ignored until the owner raises its last contact or cancels; this prevents
independent browsers from splicing contacts into one Wayland seat. The seat's
touch capability is advertised while at least one connection has direct mode
enabled. Pointer gestures remain the default, and touch input falls back to
that mapping when the feature bit is absent. Trackpad wheel events and pen
pointer events are unaffected.

Contacts are mirrored to the surface's other viewers as
`S2C_SURFACE_REMOTE_INPUT` with `kind = 1`, the same way a pointer position is;
see [Shared input marks](#shared-input-marks).

## Connection lifecycle

On connect, the server immediately sends:

```
S2C_HELLO       (protocol version + feature bits + boot generation + server release)
S2C_LIST        (all existing PTYs)
S2C_TITLE       (one per PTY, if title is set)
S2C_EXITED      (one per exited-but-retained PTY)
S2C_READY       (end of initial burst)
```

After `S2C_READY`, the client can start sending commands. `S2C_UPDATE` frames are not sent until the client subscribes to a PTY with `C2S_SUBSCRIBE`. Each `C2S_SUBSCRIBE`, including one repeated for an already subscribed PTY, starts a fresh diff stream: the next update is a full-state keyframe. Clients use that repeat to recover after discarding or failing to apply a delta.

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
Byte 1 (flags1): inverse | wide | wide_continuation | content_len[3] | link
Bytes 2–4:       fg color (r, g, b) or palette index
Bytes 5–7:       bg color (r, g, b) or palette index
Bytes 8–11:      UTF-8 content (up to 4 bytes)
```

Color type encoding: 0 = default terminal color, 1 = indexed (256-color palette), 2 = RGB true color.

When `content_len == 7`, the cell's text exceeds 4 bytes. Bytes 8–11 hold an FNV-1a hash used for diff comparison; the actual UTF-8 string is transmitted in the `STRINGS_PRESENT` section, keyed by cell index.

`link` (bit 6) marks a cell covered by an OSC 8 hyperlink. The target lives in the hyperlink section below; the bit exists so the renderer can style a link without a side-table lookup, and so a cell gaining or losing a link is visible to the byte-wise cell diff.

**Hyperlink section** — trailing, after the scrollback count:

```
[u16 uri_count]                                     0xFFFF = unchanged, section ends
  uri_count × [u16 link_id][u16 uri_len][uri utf8]
[u16 run_count]
  run_count × [u32 start_cell][u16 run_len][u16 link_id]
```

Like the scrollback count it follows, this section is a backward-compatible extension: a client that predates it stops reading after the scrollback count, and its absence reads as "no hyperlinks" on a new client talking to an old server. No capability negotiation is involved.

`link_id` is frame-local and `0` means "no link", so `0xFFFF` is free to serve as the `unchanged` sentinel — which is what an idle frame costs: two bytes. When the state does change the table is sent in full rather than diffed, because `OP_COPY_RECT` / `OP_FILL_RECT` relocate cells and replaying those transforms against a parallel id array is a correctness trap for a section that is nearly always empty. Keyframes always send the table explicitly rather than claiming "unchanged".

URIs are deduplicated by target, capped at 4096 bytes, and dropped rather than truncated when longer — a truncated URI is a _different_ URI. The cell→id map is run-length encoded because a hyperlink always spans contiguous cells.

The server relays targets verbatim and applies no scheme filtering: OSC 8 deliberately decouples a link's text from its target, and only the client is positioned to show the user that discrepancy. `@blit-sh/core`'s `assessUrl()` classifies every target as `allow` / `confirm` / `deny` before it can be opened — rejecting script-executing schemes and any URI containing invisible or text-reordering codepoints, and escaping every target for display so a preview cannot misrepresent itself.

**Mode bits** (16-bit field in frame header):

- Bits 0–8: cursor style, app cursor keys (`DECCKM`), app keypad, alternate screen, mouse mode (X10/VT200/button-event/any-event), mouse encoding (UTF-8/SGR/pixel)
- Bit 9: PTY echo flag (`tcgetattr ECHO`)
- Bit 10: PTY canonical mode (`tcgetattr ICANON`)

Mode bits are tracked by `ModeTracker` in `blit-alacritty`, which intercepts CSI/DCS sequences from raw PTY output.

## Fragmentation

`S2C_FRAGMENT` (`0x2B`) splits any bulk server message into chunks so small
frames such as audio need not sit behind a multi-megabyte write:

```
[0x2B][flags:1][chunk:N]
```

Flag bit 0 (`FRAGMENT_FLAG_LAST`) marks the final chunk. Chunks carry the
original message's bytes verbatim; its opcode arrives in the first chunk. The
receiver concatenates chunks into one logical message and dispatches it
normally. Fragments of different messages do not interleave, and the protocol
permits only `S2C_AUDIO_FRAME` between fragments.

Chunk size is transport policy and receivers must not depend on one. Splitting
happens wherever the sender can see the link it is writing to:

Both hops split, each measuring its own writes: payloads over 128 KiB on
sight, dropping to 4 KiB chunks once writes are seen to block and recovering
when they stop.

- The **server** writes to a unix socket, which looks free while the gateway is
  keeping up. It is not: the gateway reads one frame at a time into a one-deep
  queue, so a browser that cannot keep up stops the gateway reading, the socket
  buffer fills, and a large write here blocks for as long as the link needs.
- The **gateway** holds the socket to the browser, so its writes measure the
  latency the listener actually hears.

Splitting at only one of the two leaves audio behind a blocking write at the
other.

A sender may re-split a fragment it received. Doing so peels the fragment
header rather than nesting: `FRAGMENT_FLAG_LAST` is carried onto the final
piece only, so the receiver sees one flat sequence either way. Logical messages may
exceed the 16 MiB frame limit. What they may not exceed is `MAX_DECOMPRESSED`
(64 MiB): a receiver aborts a reassembly that grows past it, so that is the real
ceiling on a logical message, and the one `S2C_LIST` is bounded against.

### Proposed bounded reassembly

The extension RFC tightens fragmentation as follows. These rules are **not yet
enforced by every shipped Rust and TypeScript client**; implementing them in
both reference clients and the shared writer is a prerequisite to advertising
the proposed feature bits 11 and 12, and belongs to phase 2 of
[design/extensions.md](design/extensions.md#implementation-plan):

- flag bits 1 through 7 are zero and every chunk is non-empty; a reserved flag
  or empty chunk aborts the connection;
- each fragment remains an ordinary frame, so `chunk` is at most 16 MiB minus
  the two-byte fragment opcode and flags;
- while reassembly is pending, any non-fragment frame other than
  `S2C_AUDIO_FRAME` aborts the connection without dispatching that frame;
- the maximum reconstructed logical message is 64 MiB and one message uses at
  most 16,384 fragments.

The updated sender must not emit a larger logical message or more fragments.
The updated receiver must check cumulative length and count before extending
its buffer, abort an over-bound sequence without dispatching it, and release
pending storage on every connection exit. The proposed logical-message ceiling
is numerically the same as `MAX_DECOMPRESSED` below, but bounds fragment
reassembly rather than the allocation declared inside an LZ4 payload.

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

Files too large for one frame upload in chunks
(`FS_UPLOAD_BEGIN`/`CHUNK`/`FINISH`/`CANCEL`): `BEGIN` names the sync,
root-relative path (same %-encoding and traversal validation as
`FS_WRITE`), mode, total plaintext `size`, and `base` — the same CAS
precondition as `FS_WRITE` (`NO_CAS` overwrites unconditionally, `base` 0
is create-exclusive, anything else must equal the current content hash).
BEGIN flags alias the `FS_WRITE` flag bits exactly: 0x01 `NO_CAS`,
0x02 `MKPARENTS`, 0x04 `DURABLE`, 0x08 `FOLLOW_SYMLINK`; anything else
answers `INVALID`. The precondition is two-phase: BEGIN evaluates it
fail-fast, before any bytes flow (CONFLICT carries the current on-disk
hash, as `FS_DONE` does), and FINISH re-verifies it under the target's
write lock immediately before the rename, so a file changed mid-upload
fails landing with CONFLICT and the now-current hash rather than being
clobbered. The server answers BEGIN with a per-connection `upload_id`.
Chunks append strictly in order — each `offset` must equal the bytes
accepted so far — and each is acked with the cumulative `received` count,
which on `OFFSET_MISMATCH` is the resume point. The engine stages the
bytes in a temp sibling of the target (never mirrored; `.blit-tmp-*`
names are excluded from sync) and `FINISH` verifies `received == size`
(else `SIZE_MISMATCH`), fsyncs when `DURABLE`, and atomically renames
over the target, creating parents under `MKPARENTS`; the success reply
carries the `FS_DONE` hash/mtime payload. `FINISH` terminates the upload
whatever the outcome; `CANCEL`, sync stop, and connection close all drop
the state and remove the temp file. Limits: `BLIT_FS_UPLOAD_MAX`
(1 GiB default, `TOO_LARGE` past it) and `BLIT_FS_UPLOAD_INFLIGHT`
(4 concurrent uploads per connection, `BUDGET` past it). Upload statuses
extend the common registry with family-local values (128–255):
128 `OFFSET_MISMATCH`, 129 `SIZE_MISMATCH`, 130 `UNKNOWN_UPLOAD`.
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
`BlitConnection`/`BlitWorkspace`). Bounded responses carry a `CURSOR`
record naming where they stopped, so every enumeration is resumable;
discovery, blame, reflog and fetch occupy a second opcode block at
`0xB1` through `0xB4` (`GIT_BASE` begins that block at `0xB0`).

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
