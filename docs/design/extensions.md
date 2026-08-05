# RFC: Wasmi extensions, native channels, and processes

- **Status:** Proposed
- **Date:** 2026-08-05
- **Companion to:** [../protocol.md](../protocol.md),
  [kv.md](kv.md), [net.md](net.md)

## Summary

Blit should execute Rust extensions compiled to WebAssembly inside the server:

```bash
blit run --on prod extension.wasm arg1 arg2
```

The client addresses the module by its full BLAKE3 digest. The server starts
it immediately when that digest is cached and asks for the module bytes only
on a cache miss. Uploaded modules are verified, validated, and stored in an
immutable persistent content-addressed cache.

An extension may have a restart policy. The server supervises successive
Wasm attempts with bounded exponential backoff. With `--persist`, the desired
extension definition is durable and an attempt which was meant to be running
is launched again after a blit server restart.

An extension is an **in-process logical blit client**. It exchanges ordinary
blit packets with the same packet dispatcher as a network client, but does
not open a socket and does not use transport framing. Its host ABI is packet
send, blocking packet receive, and a direct clock read. It gets the same
initial `HELLO` / `LIST` / `READY` sequence and can use every blit protocol
family exposed to ordinary clients by that server.

This RFC also adds native **channels**: named connection points carrying
reliable bidirectional messages without terminal semantics.

A named persistent extension may advertise a discoverable command tree. The
CLI exposes it under an unambiguous `@name` namespace and carries each command
invocation over a normal channel.

It also adds a native **process** family for spawning non-PTY child processes,
streaming their stdout and stderr, writing stdin, and controlling their
lifecycle. Process operations are blit packets rather than Wasm host imports.

These are blit packet families, not Wasm-specific host functions. Browser,
CLI, native, and Wasmi clients all see the same semantics. RPC, streamed
results, notifications, and actor mailboxes are libraries over channels.

## Motivation

Blit already exposes terminals, compositor surfaces, filesystem sync, Git,
LSP, KV, and network relay through one version-stable protocol. Recreating
those operations as a large WASI-like host API would produce two public
interfaces and two dispatch implementations. They would inevitably differ
in validation, cancellation, resource ownership, and new feature coverage.

Putting a real or loopback socket between the server and an embedded runtime
would avoid that duplication but preserve overhead and failure modes which
have no purpose in one process: transport framing, socket buffers, connection
setup, authentication, and kernel scheduling.

The packet is the useful boundary. A Wasm linear-memory crossing already
requires bounded bytes; using those bytes as a normal blit packet gives exact
client parity and reuses all existing codecs. The server can dispatch the
packet directly.

Terminals are not a suitable coordination primitive for extensions. Their byte
stream has presentation state, escape sequences, process semantics, and no
message boundaries. Extensions need named discovery, bidirectional messages,
cancellation, and backpressure without pretending to be processes attached to
PTYs.

## Goals

- Run Rust-produced core Wasm modules using Wasmi in the blit server.
- Upload a module only when the selected server lacks its BLAKE3 object.
- Supervise failed or completed attempts under an explicit restart policy.
- Optionally persist desired extension state across blit server restarts.
- Give an extension the same protocol surface as an equivalent remote client.
- Add server-native bidirectional channels.
- Let named persistent extensions contribute discoverable, namespaced CLI
  commands.
- Let clients spawn and control non-PTY server processes with flow-controlled
  stdin, stdout, and stderr streams.
- Keep the Wasm host ABI very small and versioned.
- Give each running extension attempt its own named OS thread.
- Keep untrusted guest execution and slow consumers from exhausting the server.
- Make extension disconnect cleanup identical to client disconnect cleanup.
- Preserve the existing protocol rule: new feature bits and opcodes; no
  reinterpretation of old messages.

## Non-goals

- **No parallel WASI API for blit operations.** Standard WASI facilities may
  optionally provide the extension's own arguments, stdio, clocks, and randomness.
  Subprocess spawning uses the blit process family, not a private WASI
  extension.
- **No Component Model requirement.** The first Rust SDK targets a small core
  Wasm ABI. A future component adapter may wrap the same packet endpoint.
- **No live-instance checkpointing.** Persistent extensions start a fresh
  Wasmi instance after a server restart. Linear memory, stacks, open handles,
  channels, and in-flight requests are not snapshotted.
- **No server-native state or pubsub.** Retained shared data remains KV's job;
  live protocols and fan-out are libraries over channels.
- **No unbounded reliable queues.** Reliability always has an explicit byte
  window, timeout, or disconnection outcome.
- **No requirement that channel payloads use JSON.** Payloads are opaque
  bytes with descriptive metadata.
- **No client-side extension code.** Extension commands execute on the selected
  server. Their descriptors provide discovery and help, not a local executable
  or client-side argument validator.

## Architecture

```mermaid
flowchart LR
    Network["Network client"] -->|frames| Decoder["Transport decoder"]
    Decoder -->|packet| Endpoint["Logical client endpoint<br/>identity · outbox"]
    Extension["Wasmi extension"] -->|host send / recv| Endpoint
    Endpoint --> Dispatcher["Shared packet dispatcher"]
    Dispatcher --> Existing["Terminal / FS / Git / LSP / …"]
    Dispatcher --> Fabric["Channel / process fabric"]
```

The current connection loop combines transport reading, client lifecycle,
and family dispatch. Implementation of this RFC first extracts a reusable
packet endpoint:

```rust
struct ClientEndpoint {
    client_id: u64,
    outbox: BoundedOutbox,
    cancellation: CancellationToken,
}

async fn dispatch_packet(
    state: &AppState,
    endpoint: &mut ClientEndpoint,
    packet: &[u8],
) -> DispatchOutcome;
```

A network connection reads one framed message and calls `dispatch_packet`.
An extension's `blit_send` copies one message from linear memory into a
bounded in-process queue whose consumer calls the same function. Neither
path dispatches while holding a transport, extension, or global registry
lock.

Every endpoint owns connection-scoped family state: filesystem syncs, Git
repositories, LSP attachments, KV subscriptions, relayed sockets, and native
channels. Dropping the endpoint invokes the same cleanup path
regardless of its adapter.

### Nonblocking dispatch and slow consumers

`dispatch_packet` must never wait for capacity in any endpoint outbox. Every
delivery uses a nonblocking `try_enqueue`; transport writers and extension threads
drain their own queues independently. Channel and process data additionally
obey their credit windows.

If a packet still cannot be enqueued within the endpoint's aggregate byte
bound, the endpoint is a slow consumer and is closed. Closing is an
out-of-band state transition: it sets cancellation, closes both in-process
queues or the network transport, and runs normal endpoint resource cleanup. A
best-effort family-specific closed or lifecycle packet carrying
`SLOW_CONSUMER` may be sent when space exists, but correctness never depends
on fitting that final packet. Lifecycle packets such as `EXT_EXIT` and
protocol errors follow this same rule rather than waiting for space. For a
extension's own endpoint, this closes the attempt with the `SLOW_CONSUMER` exit
reason; connected channel and process peers observe their normal closed event.

`EXT_EVENT` fan-out performs one independent `try_enqueue` per attached
client after recording the event in the bounded retained ring when retention
is enabled. One slow follower therefore closes only that follower and never
stalls the extension or other followers. If that follower is the owner of a
non-detached extension, the ordinary owner-disconnect rule then cancels the
extension; detached extensions keep running.

This policy also breaks the apparent full-duplex deadlock. An extension blocked in
`blit_v1.send` is not calling `recv`, but the endpoint task continues draining
the extension-to-endpoint queue. If dispatch of one of those packets needs to send
back to a full extension outbox—including through a channel connected to the same
extension—`try_enqueue` closes the endpoint instead of blocking. Closing wakes
the blocked `send` with `-1` and cancels the attempt. No dispatcher, registry
lock, or peer endpoint waits for that extension to call `recv`.

### Packet parity

An extension may form any valid C2S packet. The dispatcher validates and handles it
exactly as if it came from a network endpoint. If an operation is available to
an ordinary client, it is available to an extension; this includes existing
administrative operations. Changing the access model for all blit clients is
separate work and must not create a Wasm-only path.

## Lifecycle model

An **extension** is the stable supervised object created by
`EXT_RUN`. It has a 64-bit randomly allocated `extension_id`, a module hash,
arguments, restart policy, desired state, definition revision, and optional
durable name. A transient ID is process-local; a persistent extension retains
its ID, revision, and name across server restarts.

An **attempt** is one Wasmi instantiation of that extension. Attempts are
numbered monotonically from one. A running attempt has its own 32-bit
process-local `task_id` and logical client endpoint. Destroying an attempt
therefore closes all of its terminals, subscriptions, relays, listeners, and
channels before the supervisor considers another attempt.

The distinction prevents a crash from changing the object clients follow:
attachments, status, retained event output, and control target the stable
`extension_id`; events additionally identify the attempt and task which
produced them.

Restart policies are:

| Value | CLI          | Meaning                                            |
| ----- | ------------ | -------------------------------------------------- |
| 0     | `never`      | One attempt only                                   |
| 1     | `on-failure` | Restart non-zero returns, traps, and host failures |
| 2     | `always`     | Restart every returned or failed attempt           |

Explicit stop, disable, removal, invalid/corrupt module state, and server
shutdown are not attempt failures. `on-failure` treats a zero return as
successful and transitions to stopped. Execution failures retain their
backoff; retrying does not change the invocation.

`PERSIST` stores the extension definition and desired state. It implies
`DETACH` and requires a unique durable name. Persistence does not itself alter
the restart policy: the common cross-server daemon form is
`--restart always --persist --name NAME`. If the server shuts down while a
persistent extension is desired-running, the shutdown ends its current
attempt without incrementing failure counters and a fresh attempt is launched
after the next server has initialized its registries.

## Wasm contract

### Module shape

The initial SDK targets `wasm32-unknown-unknown`. A module:

- exports linear memory as `memory`;
- exports `blit_main: () -> i32`;
- imports `send`, `recv`, and `clock` from module `blit_v1`.

Returning from `blit_main` ends the attempt. Its `i32` is the attempt exit
code. A trap, invalid host call, or rejected packet ends the attempt with a
structured failure reason. The supervisor then applies the extension's restart
policy.

### Host ABI

```text
blit_v1.send(ptr: i32, len: i32) -> i32
blit_v1.recv(ptr: i32, capacity: i32) -> i32
blit_v1.clock(kind: i32) -> i64
```

`send` validates the range in exported memory and copies one complete blit
packet. It returns `0` when accepted, `-1` when the endpoint is closed or
closes while the call is pending, or `-2` for a zero-length or over-cap
packet. It never accepts transport framing: the first copied byte is the blit
opcode. A negative length, invalid pointer, integer overflow, or an
out-of-bounds linear-memory range is guest ABI misuse and traps the attempt
rather than returning an error code.

`recv` operates on the next complete server packet:

- zero means the logical endpoint is closed;
- a positive result `N <= capacity` means an `N`-byte packet was copied and
  removed from the mailbox;
- a positive result `N > capacity` means the packet needs `N` bytes; nothing was
  copied and the packet remains queued.

When no packet is available, `recv` parks the dedicated extension thread. A
negative capacity, integer overflow, or an invalid destination range traps the
attempt. The SDK keeps a reusable buffer and retries with `N` bytes only when
needed. `recv` never returns a negative value.

`clock(0)` returns signed nanoseconds since the Unix epoch. `clock(1)` returns
nanoseconds from a monotonic clock with an unspecified origin. Realtime may
jump when the host clock is adjusted; monotonic values are suitable only for
differences and must not be persisted across server restarts. Any other kind
traps the attempt. A clock read is a direct synchronous host call: it does not
construct or dispatch a packet.

`send` may block until its whole packet fits in the endpoint's inbound mailbox.
This preserves ordering and backpressure without busy polling.

Each in-process mailbox has a 16 MiB byte capacity, equal to the maximum Blit
logical-message size. Packets are never split, and every valid packet therefore
fits in an empty mailbox. There is no separate oversized-packet case.

No separate argument, logging, or process imports are needed. Guest logs and
structured output are `EXT_EVENT` packets. Child processes and other Blit
facilities remain ordinary protocol operations.

### Bootstrap identity and arguments

Before the attempt becomes externally reachable, its endpoint enqueues the
normal `HELLO` / `LIST` / `READY` burst followed immediately by exactly one
`EXT_INFO(INIT)`. No channel acceptance, command invocation, or other routed
packet may precede `INIT`.

`INIT` supplies the `extension_id`, definition revision, attempt, process-local
task ID, module hash, invocation name when present, attachment and persistence
flags, and the exact UTF-8 argument vector from `EXT_RUN`. No synthetic module
path or `argv[0]` is inserted. For a transient extension, revision is always 1
and the ID lasts for this server process. For a persistent extension, ID and
revision survive server restarts. Attempt increases on every Wasmi
instantiation; task ID is meaningful only in the current server process.

The `blit-guest` entry-point wrapper consumes the handshake and `INIT` before
calling user code, then exposes them as an immutable context:

```rust
let context = blit.context();
let id = context.extension_id;
let version = (context.definition_revision, context.module_hash);
let args: &[String] = &context.args;
let started = blit.monotonic_now();
let wall_time = blit.realtime_now();
let elapsed = blit.monotonic_now() - started;
```

The context also exposes `attempt`, `task_id`, `name: Option<String>`,
`detached`, and `persistent`. Protocol features come from the preceding
`HELLO`, just as they do for a network client. Identity and arguments therefore
require no extra host calls. The clock wrappers return a typed wall-clock time
and an opaque monotonic instant whose subtraction yields a duration; neither
requires a dispatcher round trip.

### Optional WASI

WASI is useful for conventional guest self-environment: arguments, the guest's
stdin/stdout/stderr, clocks, and randomness. It is not another API for blit
operations. The initial `wasm32-unknown-unknown` SDK needs no WASI imports; a
later `wasm32-wasip1` target may link
[`wasmi_wasi`](https://docs.rs/wasmi_wasi/latest/wasmi_wasi/) for those
facilities. A WASI-enabled SDK backs its clocks with `blit_v1.clock`, so the
core and WASI targets observe the same host clocks. Version 1 defines no WASI
filesystem preopens or sockets. If the adapter exposes WASI arguments, it uses
the same `INIT` vector without inventing a server-side path; the SDK context is
the canonical source on both targets.

Standard WASI does not define arbitrary child-process spawning. Its CLI
proposal covers environment variables, arguments, stdio, and the current
process's exit, not `spawn` or `exec`; see the
[WASI proposal list](https://wasi.dev/releases). A private `proc_spawn`
import would be possible, but would duplicate blit's lifecycle and streaming
protocol. Extensions therefore launch child processes through `PROCESS_*`
packets, just like any other client.

### Rust SDK

`blit-guest` wraps the three imports and re-exports the protocol codec surface:

```rust
#[blit::main]
fn main(mut blit: blit::Client) -> Result<(), Error> {
    let args = &blit.context().args;
    let request = blit_remote::msg_create2(/* ... */);
    blit.send(&request)?;
    let created = blit.recv_matching(/* ... */)?;
    // The same client can open native channels or any other blit family.
    Ok(())
}
```

Higher-level typed wrappers are libraries over packets. They are not host
bindings and can evolve independently:

```rust
let mut peer = blit.channels().connect("com.example.builder")?;
peer.send(postcard::to_allocvec(&request)?)?;
let reply = postcard::from_bytes(&peer.recv()?)?;
```

The low-level API remains available so an extension is never blocked on the SDK
having wrapped a newly added blit opcode.

### Dedicated execution thread

Each running extension attempt owns exactly one OS thread, never shared with
another extension. The async supervisor creates a fresh thread for each attempt;
autorestarts therefore get a new OS thread with the same extension-derived name.
An extension in `QUEUED`, `BACKOFF`, or terminal `STOPPED`, or one which is
disabled or removed, owns no thread. At most one attempt and thread exist for
an extension at a time.

Wasmi execution never occurs on a server async executor thread and never
occurs under a server lock. The async logical endpoint and synchronous extension
thread communicate through two bounded in-process queues:

1. the extension thread instantiates the attempt and calls `blit_main`;
2. `blit_v1.send` copies a packet into the extension-to-endpoint queue, blocking
   under its byte-window backpressure;
3. `blit_v1.recv` blocks the extension thread on the endpoint-to-extension queue
   until a packet arrives or cancellation closes it;
4. fuel exhaustion returns control to the thread driver, which checks
   cancellation before replenishing the next slice;
5. completion, trap, or cancellation destroys the attempt endpoint, reports
   `EXT_EXIT`, and ends the thread; the async supervisor then stops, waits in
   backoff, or creates a fresh thread for the next attempt.

An empty receive parks the dedicated thread without consuming CPU; restart
backoff uses the async supervisor and consumes no extension thread. The server
reserves the thread and Wasmi resources before marking an attempt running. A
reservation or thread-spawn failure reports a structured host failure and never
panics the server.

### Thread names

Extension thread names are diagnostic, not identity. The full logical name is:

```text
blit-ext:<label>#<short-extension-id>
```

`label` is chosen from the explicit invocation or durable name, then the module
hash prefix. User-controlled labels are converted to printable ASCII,
separators are collapsed, and path components, control characters, and secrets
are never included. The stable extension ID suffix distinguishes concurrent
transient instances of the same module.

A shared helper compacts that logical name for each platform while retaining
the component prefix and ID suffix. For example, a Linux-sized name might be
`blit-e:bui-7f2a`, while a platform allowing longer names can expose
`blit-ext:builder#7f2a`. Failure to set a descriptive OS name falls back to
`blit-ext` and does not prevent execution. The full logical name remains
available in server diagnostics and logs.

The same helper and `blit-<component>[-<role>][-<short-id>]` convention should
be used throughout blit. Long-lived explicit threads must be created with
`std::thread::Builder::name`; Tokio runtimes should use `thread_name` or
`thread_name_fn`. Existing named threads can retain compatible names, while
unnamed runtime workers and ad-hoc filesystem, Git, LSP, CLI-input, and child
reaper threads should be migrated as a mechanical follow-up. Thread names
must remain observational: protocol behavior never depends on them.

## Content-addressed execution

### Identity

A module object ID is the full 32-byte BLAKE3 digest of the exact Wasm
module bytes. It is rendered as 64 lowercase hexadecimal digits in paths and
human output. Truncation is not permitted for storage or wire identity.

Arguments and attachment mode are invocation properties. They do not affect
the module object hash.

### Cache

Raw modules are persisted under `$BLIT_WASM_CACHE`, otherwise the platform
cache directory followed by `blit/wasm/objects`. A conceptual path is:

```text
objects/ab/cdef...<remaining 62 hex digits>.wasm
```

Insertion is atomic:

1. stream into a temporary file under the cache directory;
2. enforce the declared and actual size caps;
3. hash the received bytes and compare all 32 bytes;
4. validate the module and its allowed imports;
5. fsync when configured, rename into the object path, then acknowledge.

An existing valid object makes upload idempotently successful. Corrupt cache
entries are quarantined or ignored and are never executed.

Wasmi's translated `Module` is cached in memory by:

```text
(object_hash, blit_host_abi_version, wasmi_engine_configuration)
```

Running attempts pin their raw and translated objects. An enabled persistent
extension also pins its raw object, so a restart never depends on the original
uploader returning. An LRU may evict other translated modules and raw objects.

### Miss and race behavior

`EXT_RUN` is the cache probe. For creation, a hit creates the extension. A miss
returns `NEED_OBJECT` and records a bounded pending extension. The miss is
encoded as `EXT_STATUS(status = OK, phase = NEED_OBJECT)`; `NEED_OBJECT` is a
run phase, not a status code. The client uploads chunks and does not resend
`EXT_RUN` after a successful final chunk; the server creates and starts the
pending extension automatically.

An update hit commits the replacement. An update miss also returns
`NEED_OBJECT`, but records no pending update and changes neither the definition
nor its current attempt. The client uploads the object, refreshes the current
ID and revision, and retries `EXT_RUN(UPDATE)` with a fresh nonce. This prevents
a slow upload from later overwriting a concurrent update.

Uploads are single-flight per object hash. If several clients miss the same
object, the first accepted uploader supplies it and every compatible pending
extension starts after verification. A client whose `EXT_PUT` races after the
object has committed receives `ALREADY_HAVE` and stops sending. While another
upload is still in progress, a second `BEGIN` receives `CONFLICT`; its pending
creation waits for the owner rather than uploading duplicate bytes. An update
client simply retries its probe. If the owner fails or expires, waiting creates
receive a fresh `NEED_OBJECT` update and may compete to become the next
uploader. That update is
`EXT_INFO(STATUS, phase = NEED_OBJECT)`, keyed by `extension_id`, rather than a
second reply to the original run nonce. Pending extensions and partial uploads
expire.

## Extension wire family

Feature bit **11** (`FEATURE_EXTENSION`) advertises this family. It occupies
the free direction-local `0x90` through `0x94` block before Git's `0xA0`
block.

All integers are little-endian. `hash` is always 32 raw bytes. Strings are
UTF-8. Unless otherwise stated, `detail` is UTF-8 and consumes the remainder.

### Client to server

| Opcode | Name          | Layout                                                                                                                                                 |
| ------ | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `0x90` | `EXT_RUN`     | `[nonce:2][flags:1][restart:1][expected_extension_id:8][expected_definition_revision:8][hash:32][name_len:2][name:N][argc:2] repeated{[len:4][arg:M]}` |
| `0x91` | `EXT_PUT`     | `[nonce:2][flags:1][hash:32][offset:8][total_size:8][data:N]`                                                                                          |
| `0x92` | `EXT_CONTROL` | `[nonce:2][extension_id:8][action:1]`                                                                                                                  |
| `0x93` | `EXT_EVENT`   | `[extension_id:8][attempt:8][task_id:4][kind:1][data:N]` — only from the matching running attempt                                                      |
| `0x94` | `EXT_COMMAND` | `[kind:1][nonce:2][body...]`                                                                                                                           |

`EXT_RUN.flags`: bit 0 `DETACH`, bit 1 `PERSIST`, bit 2 `UPDATE`. `restart` is the
restart-policy value from [§ Lifecycle model](#lifecycle-model). `PERSIST`
requires `DETACH` and a non-empty, unique `name`. Without `PERSIST`, `name` may
be empty and is descriptive only. Unknown flags or restart values are
`INVALID`.
Argument count is capped at 1024, each argument at 64 KiB, and their combined
UTF-8 bytes at 1 MiB. NUL has no special meaning.

For a new extension, `UPDATE` is clear and `expected_extension_id` must be
zero, as must `expected_definition_revision`. Creating a persistent extension
whose name already exists is `CONFLICT`. For an update, `DETACH`, `PERSIST`,
and `UPDATE` are all set; the supplied name, non-zero expected ID, and non-zero
expected revision must identify the current persistent definition. The hash,
arguments, and restart value describe its complete replacement definition.
Update behavior is specified in
[§ Instances and module versions](#instances-and-module-versions).

`EXT_PUT.flags`: bit 0 `BEGIN`, bit 1 `FINAL`. The first chunk has
`BEGIN`, offset zero, and begins a new upload. `total_size` is present on every
chunk to keep decoding fixed; it must be non-zero, match the first chunk, and
fit the module cap before any data is accepted. Chunks are contiguous and
cumulatively acknowledged. `FINAL` requires `offset + data.len() ==
total_size`, then triggers hash verification, validation, atomic cache
insertion, and pending-run start. A one-packet object sets both flags. The
maximum module size is 16 MiB; clients should use 1 MiB chunks.
An upload may also follow an update miss without a pending create; in that case
successful insertion only primes the CAS for the client's next update probe.

`EXT_CONTROL.action`:

| Value | Name      | Meaning                                                             |
| ----- | --------- | ------------------------------------------------------------------- |
| 1     | `CANCEL`  | Stop the extension, suppress restarts, then cancel its attempt      |
| 2     | `ATTACH`  | Subscribe this connection to retained and future events             |
| 3     | `DETACH`  | Stop this connection following without changing desired state       |
| 4     | `STATUS`  | Request the current supervisor and attempt lifecycle record         |
| 5     | `RESTART` | End the current attempt and schedule a new one immediately          |
| 6     | `ENABLE`  | Set a retained persistent extension to desired-running              |
| 7     | `DISABLE` | Durably clear desired-running before cancelling the current attempt |
| 8     | `REMOVE`  | Durably remove a disabled persistent definition and retained events |
| 9     | `LIST`    | List visible extensions; requires `extension_id = 0`                |

Every control other than `LIST` receives one `EXT_STATUS` carrying the
request nonce. `LIST` receives `EXT_INFO(LIST)` below. A CLI name is resolved
through `LIST`; wire control continues to use unambiguous 64-bit IDs.

`EXT_EVENT.kind` reserves 1 for stdout bytes, 2 for stderr bytes, and
3 for a UTF-8 log record. These are convenience event streams, not terminals.
Structured application communication should use channels.

Client `EXT_COMMAND.kind` values:

| Kind | Name       | Body                                              |
| ---- | ---------- | ------------------------------------------------- |
| 1    | `REGISTER` | `[listener_id:4][descriptor_len:4][descriptor:N]` |
| 2    | `DISCOVER` | `[directory_revision:8][cursor:8]`                |

`REGISTER` is valid only from the running endpoint of a named persistent
extension, and `listener_id` must name a live channel listener owned by that
same endpoint. One extension has at most one advertised command listener; a
new registration atomically replaces the prior one. A zero-length descriptor
with `listener_id = 0` unregisters it. Registration receives
`EXT_INFO(COMMAND_REGISTERED)`.

Any logical client may send `DISCOVER`. A first request uses revision and
cursor zero. A continuation repeats the returned non-zero revision and cursor.
It receives one page in `EXT_INFO(COMMANDS)`; `next_cursor = 0` means the list
is complete. A directory mutation between pages returns `CONFLICT`, and the
client restarts at zero. The revision is process-local and exists only to make
pagination and client caching coherent.

### Server to client

| Opcode | Name             | Layout                                                                                                                                                   |
| ------ | ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0x90` | `EXT_STATUS`     | `[nonce:2][status:1][phase:1][flags:1][restart:1][extension_id:8][definition_revision:8][attempt:8][task_id:4][next_start_unix_ms:8][hash:32][detail:N]` |
| `0x91` | `EXT_PUT_STATUS` | `[nonce:2][status:1][hash:32][received:8][detail:N]`                                                                                                     |
| `0x92` | `EXT_INFO`       | `[kind:1][body...]`                                                                                                                                      |
| `0x93` | `EXT_EVENT`      | `[extension_id:8][definition_revision:8][attempt:8][task_id:4][sequence:8][kind:1][data:N]`                                                              |
| `0x94` | `EXT_EXIT`       | `[extension_id:8][definition_revision:8][attempt:8][task_id:4][reason:1][code:4][next_start_unix_ms:8][detail:N]`                                        |

Server `EXT_INFO.kind` values:

| Kind | Name                 | Body                                                                                                                                   |
| ---- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | `INIT`               | `[extension_id:8][definition_revision:8][attempt:8][task_id:4][flags:1][hash:32][name_len:2][name:N][argc:2] repeated{[len:4][arg:M]}` |
| 2    | `LIST`               | `[nonce:2][status:1][count:2] repeated{extension_record}`                                                                              |
| 3    | `STATUS`             | `[extension_id:8][definition_revision:8][phase:1][flags:1][restart:1][attempt:8][task_id:4][next_start_unix_ms:8][hash:32][detail:N]`  |
| 4    | `COMMAND_REGISTERED` | `[nonce:2][status:1][extension_id:8][definition_revision:8][detail:N]`                                                                 |
| 5    | `COMMANDS`           | `[nonce:2][status:1][directory_revision:8][next_cursor:8][count:2] repeated{command_record}`                                           |

`EXT_INFO(INIT).flags` and the flags stored in status and list records use bit
0 `DETACH` and bit 1 `PERSIST`; all other bits are zero. `UPDATE` describes an
`EXT_RUN` operation and is never part of an attempt's identity.

An `extension_record` is:

```text
[extension_id:8][definition_revision:8][phase:1][flags:1][restart:1]
[attempt:8][task_id:4]
[next_start_unix_ms:8][hash:32][name_len:2][name:N]
```

A `command_record` is:

```text
[extension_id:8][definition_revision:8][hash:32][name_len:2][name:N]
[listener_name_len:2][listener_name:M][descriptor_len:4][descriptor:D]
```

Only a live, successfully registered listener appears in `COMMANDS`. Its
namespace is the extension's unique durable `name`; the descriptor cannot
override it. Closing the listener, ending the attempt, disabling or removing
the extension, or dropping its endpoint removes the record and increments the
directory revision. The next attempt must register again. The server does not
retain a stale descriptor or queue invocations during restart or backoff.

`EXT_INFO(INIT)` is injected only into the extension's in-process endpoint
after `READY`; a network client never receives it merely by attaching.

`EXT_RUN` receives exactly one nonce-correlated `EXT_STATUS`. A create allocates
and returns a new `extension_id` even on a cache miss; an update returns the
existing ID and current definition revision. The phase in a cache-miss reply
describes the pending create or update operation, not a replacement lifecycle
for an attempt which is still running. That reply releases the 16-bit nonce.
For `UPDATE`, the correlated reply's hash is the requested hash and its attempt
and task fields are zero: old-attempt exit and new-attempt start are reported by
their subsequent ID-keyed events. A cache-miss reply carries the current
revision and creates no later status event; a committed cache hit carries the
new revision.
Later validation, definition commit, queue, attempt, backoff, and stop
transitions are uncorrelated `EXT_INFO(STATUS)` events keyed by `extension_id`;
attached clients follow the ID and do not keep the original run nonce reserved. Each
non-`LIST` `EXT_CONTROL` likewise receives exactly one `EXT_STATUS`
snapshot with its own request nonce, after which later changes are ID-keyed
events. `EXT_PUT` nonces live for one chunk acknowledgement, and the `LIST`
nonce lives through its single `EXT_INFO(LIST)` reply. On a given endpoint,
the correlated reply is enqueued before any `EXT_INFO` or `EXT_EXIT`
caused by that request.

Each `EXT_COMMAND` nonce likewise lives through exactly one
`COMMAND_REGISTERED` or `COMMANDS` reply. Later directory changes are not
pushed; discovery is an explicit snapshot operation.

Run phases:

| Value | Name          | Meaning                                                  |
| ----- | ------------- | -------------------------------------------------------- |
| 1     | `NEED_OBJECT` | Object absent; one uploader should send it               |
| 2     | `VALIDATING`  | Bytes complete; hash and import validation               |
| 3     | `QUEUED`      | Valid attempt waiting for an execution slot              |
| 4     | `RUNNING`     | `task_id` is live and its logical client exists          |
| 5     | `BACKOFF`     | Supervisor will start another attempt at the stated time |
| 6     | `STOPPED`     | No attempt is running or scheduled                       |
| 7     | `BLOCKED`     | Permanent condition requires object or operator work     |

Exit reasons distinguish returned, trapped, cancelled, updated, slow consumer,
protocol violation, host failure, and server shutdown. `UPDATED` means a newer
definition replaced this attempt; it is not an attempt failure and does not
apply restart backoff. `code` is the `blit_main` return only for `RETURNED`; it
is zero for other reasons.
`next_start_unix_ms` is non-zero only when the supervisor has scheduled another
attempt.

Common family status values are reused: `OK`, `NOT_FOUND`, `TOO_LARGE`,
`INVALID`, `CANCELLED`, `OTHER`, and `CONFLICT`.
`EXT_PUT_STATUS` additionally defines value 12 as `ALREADY_HAVE`. It means
the verified object is already committed; `received` is its stored total size,
pending creations proceed, and the client must stop uploading. `OK` reports the
cumulative accepted `received` bytes. `CONFLICT` means another uploader owns
the still-uncommitted single flight and does not claim the object exists yet;
it reports `received = 0`.

### Attached lifecycle

Without `DETACH`, the initiating connection owns the extension. Disconnecting
or sending `CANCEL` stops the supervisor, suppresses any pending restart, and
cancels its current attempt. Ctrl-C in `blit run` sends `CANCEL`, waits a
short grace period for `EXT_EXIT`, and then closes.

With `DETACH`, phase `RUNNING` in either the correlated `EXT_STATUS` or a
later `EXT_INFO(STATUS)` is sufficient for the command to return
successfully. The extension remains server-owned until its restart policy stops
it, it is explicitly cancelled, or the server exits. Its event log is a bounded
byte ring across attempts, so a later `ATTACH` receives a retained suffix
followed by live events. Extension attempts have no wall-clock deadline; attached
and detached execution differ only in ownership and event following.

Every attempt has a 32-bit process-local `task_id`. Task IDs are not durable;
`extension_id` and `attempt` are the stable coordinates followed by clients.

### Restart backoff

Automatic restarts use full-jitter exponential backoff: 250 ms base, doubling
through a 30 second cap. An attempt which remains running for 60 seconds
resets the consecutive-failure counter. `RESTART` is an explicit operator
action and schedules immediately; it does not erase historical attempt
records. A persistent supervisor stores its failure count and next eligible
wall-clock start time, so restarting blit cannot be used to bypass crash-loop
backoff.

Failures which cannot improve by retrying transition to `BLOCKED` rather than
looping: missing or corrupt pinned object, unsupported host ABI, or a
deterministic instantiation/import error. An object repair, definition update,
or explicit `ENABLE` causes revalidation.

### Persistence across server restarts

Persistent definitions are durable desired state, separate from the Wasmi
instance. The server transactionally stores:

- stable extension ID and unique name;
- definition revision, object hash, arguments, and restart policy;
- enabled/desired-running state;
- attempt counter, consecutive-failure count, and next eligible start time.

Definitions live in `$BLIT_EXTENSION_PATH`, otherwise the platform state
directory followed by `blit/extensions.redb`. This is authoritative state,
not an evictable cache. The raw Wasm object remains in the separate
content-addressed cache but is pinned by every enabled definition.

The module object is made durable before the definition can commit. On server
startup, definitions are loaded and validated before any attempt starts.

`DISABLE` and `REMOVE` commit their durable state before cancelling an
attempt, so a crash cannot resurrect something the operator just stopped.
Normal server shutdown preserves desired-running without recording an attempt
failure. Abrupt server death is treated the same at the next boot because an
attempt has no durable successful exit record.

Cross-restart execution is consequently **at least once**, not exactly once.
The server can die after an extension performs an external side effect but before
it durably records the attempt's exit. Persistent extensions must make side
effects idempotent or store their own progress transactionally, for example in
KV. Blit does not checkpoint Wasm memory or try to infer whether a side effect
committed.

Arguments are stored verbatim. They should not contain secrets unless the
extension store gains an explicit encrypted-secret mechanism; references through
a separate secret facility are preferable. Retained stdout/stderr events are
not durable in the first version.

## Instances and module versions

Three identifiers answer different questions:

| Identifier                | Identifies                                        | Lifetime                 |
| ------------------------- | ------------------------------------------------- | ------------------------ |
| module hash               | exact Wasm bytes                                  | immutable CAS object     |
| `extension_id`            | one supervised installation and its configuration | stable for the extension |
| `(extension_id, attempt)` | one Wasmi instance                                | one execution attempt    |

Every non-`UPDATE` `EXT_RUN` creates a distinct extension and ID, even when
the hash, arguments, and descriptive name are identical. The same module
object can therefore back any number of isolated extensions without another
upload. Each extension has at most one running attempt; v1 has no replica-count
setting. Operators create replicas as separate extensions, for example
`worker-1`, `worker-2`, and `worker-3`, and manage them independently.

```bash
blit run --on prod --restart always --persist --name worker-1 worker.wasm queue-a
blit run --on prod --restart always --persist --name worker-2 worker.wasm queue-b
blit ext list --on prod
blit ext restart --on prod worker-1
```

Transient names are descriptive and need not be unique, so transient instances
are controlled by ID. Persistent names are unique durable handles and are the
normal operator-facing identity. Extensions which derive channel names or KV
prefixes per instance should include their `extension_id`; replicas must not
assume that a shared module hash implies shared identity.

Blit assigns no semantic version to a module and reads no version manifest.
The full module hash is its exact version identity. Different hashes coexist in
the CAS, and `blit ext list` and `blit ext status` show the full current hash and
`definition_revision`. Revision starts at 1, survives server restarts, and
increments whenever a persistent extension's hash, arguments, or restart
policy changes. Attempts report the revision they execute, so events and exits
remain attributable when an update overlaps observation of the old attempt.

To run two versions concurrently, create two persistent extensions with
different names, such as `builder` and `builder-canary`. To replace one durable
extension in place, use:

```bash
blit ext update --on prod builder ./builder-v2.wasm arg1 arg2
```

This sends `EXT_RUN(UPDATE)` with the name, ID, and definition revision observed
by the CLI. The expected ID prevents an update from crossing a
remove-and-recreate race, and the expected revision prevents concurrent updates
from silently overwriting one another. An absent name is `NOT_FOUND`; an ID or
revision mismatch is `CONFLICT`. The server rechecks both in the same
transaction which commits the new definition, including after a cache miss.
The client and server handle an update as follows:

1. The client probes with `EXT_RUN(UPDATE)`. On `NEED_OBJECT`, it uploads and
   validates the replacement while the current attempt continues, refreshes
   the extension record, and retries the update.
2. The server atomically checks the expected ID and revision, stores the new
   hash, arguments, and restart policy, and
   increments the definition revision. Enabled and desired-running state are
   preserved.
3. If an attempt is running and the definition changed, it exits with
   `UPDATED`; the supervisor clears failure backoff and immediately starts the
   new revision. A disabled or stopped extension merely records the new
   definition.

Submitting the exact current hash, arguments, and restart policy is an
idempotent success: it neither increments revision nor restarts an attempt. A
failed upload or validation leaves the old definition and attempt unchanged.
The old attempt's channels, processes, command listener, and endpoint close
normally before the new attempt becomes reachable. Its command advertisement
is removed as part of the definition commit, before the old attempt is asked to
exit. Command calls are never retried across that boundary.

V1 keeps no definition history and performs no automatic rollback. Rollback is
an ordinary update naming older Wasm bytes; it avoids upload only if that hash
is still in the CAS. Durable names cannot be renamed in place, because that
would break command namespaces and operator references; create the new name and
remove the old extension instead.

## Native channel family

Feature bit **12** (`FEATURE_CHANNEL`) advertises channels. The family uses the
direction-local `0x95` opcode with a one-byte sub-operation. `0x96` remains
free.

Channel IDs are 32-bit and scoped to one logical client.
Client-created IDs have bit 0 clear; server-created accepted-channel IDs have
bit 0 set. An ID is not reused until its `CLOSED` event has been observed.

Names are UTF-8, non-empty, at most 255 bytes, contain no control or NUL
characters, and are process-global. They are compared byte-for-byte without
Unicode normalization; clients should therefore use ASCII reverse DNS names
such as `com.example.builder`. Metadata and payloads are opaque bytes.

Version 1 has no namespace reservation or per-name access rules. Any endpoint
may `LISTEN` or `CONNECT`; the first listener owns the name until it closes, and
a second listener receives `CONFLICT`. First-listener ownership is routing
state, never proof of identity. Peers use the server-supplied `peer` identity
rather than trusting the channel name.

### Bidirectional channels (`0x95`)

Every message begins `[0x95][kind:1][channel_id:4]`.

Client-to-server kinds:

| Kind | Name      | Body                                                        |
| ---- | --------- | ----------------------------------------------------------- |
| 1    | `LISTEN`  | `[flags:1][name_len:2][name:N][metadata_len:4][metadata:M]` |
| 2    | `CONNECT` | `[flags:1][name_len:2][name:N][metadata_len:4][metadata:M]` |
| 3    | `DATA`    | `[payload:N]`                                               |
| 4    | `ACK`     | `[bytes:8]` cumulative consumed payload bytes               |
| 5    | `CLOSE`   | `[reason:1]`                                                |

Server-to-client kinds:

| Kind | Name       | Body                                                                    |
| ---- | ---------- | ----------------------------------------------------------------------- |
| 1    | `OPENED`   | `[status:1][window:8][detail_len:2][detail:N]`                          |
| 2    | `ACCEPTED` | `[listener_id:4][window:8][peer_len:2][peer:N][metadata_len:4][meta:M]` |
| 3    | `DATA`     | `[sequence:8][payload:N]`                                               |
| 4    | `ACK`      | `[bytes:8]` cumulative consumed payload bytes                           |
| 5    | `CLOSED`   | `[reason:1][detail:N]`                                                  |

A listener owns a name until closed. `CONNECT` either fails once with
`OPENED(status != OK)` or produces `OPENED(OK)` for the connector and one
`ACCEPTED` on the listener endpoint. Thereafter the two channel IDs are a
full-duplex message pair. Blit preserves each `DATA` message boundary and
orders messages per direction.

`peer` is a server-assigned logical-client identity suitable for display, not a
self-asserted string from metadata. Passing more identity claims requires
explicit server support.

Flow control is a cumulative byte window per direction, using payload bytes.
`OPENED` / `ACCEPTED` grants window `W`. An `ACK(C)` means the receiver has
consumed `C` total payload bytes, so the sender may advance its cumulative
sent count through `C + W`; exceeding that closes the channel. The receiver
acks only after delivering or intentionally discarding messages. Default
window and maximum payload: 4 MiB. Metadata is capped at 64 KiB.

There is no implicit broadcast and no retained channel data. Higher-level RPC
uses request IDs inside the opaque payload and cancellation by a normal
message or channel close.

## Extension-provided CLI commands

A running named persistent extension may contribute a command tree under its
durable name. The `@` prefix keeps remote extension commands separate from
Blit's built-in grammar. Transient extensions cannot advertise commands because
their descriptive names are neither unique nor durable:

```bash
blit ext commands --on prod
blit --on prod @builder --help
blit --on prod @builder build --release app
```

Connection and Blit-wide options must precede `@builder`. Every token after the
namespace is the command argument vector and is delivered verbatim, including
tokens beginning with `-`; no `--` separator is required. The sole exception is
a final `--help` following an advertised command path, which the CLI renders
from the descriptor without opening an invocation channel. A persistent extension
named `builder-canary` independently contributes `@builder-canary`, so
concurrent versions do not contend for one CLI namespace.

The control and data paths are deliberately separate:

```mermaid
sequenceDiagram
    participant E as "Wasmi extension"
    participant S as "Blit server"
    participant C as "CLI client"
    E->>S: "CHANNEL LISTEN (fresh listener name)"
    E->>S: "EXT_COMMAND REGISTER (listener + descriptor)"
    C->>S: "EXT_COMMAND DISCOVER"
    S-->>C: "@name records at directory revision"
    C->>S: "CHANNEL CONNECT (selected listener)"
    S-->>E: "CHANNEL ACCEPTED"
    C->>E: "INVOKE argv"
    E-->>C: "STDOUT / STDERR / RESULT / EXIT"
```

Registration is live advertisement, not an install manifest. The extension
first listens on a fresh name, conventionally
`blit.cli.<extension_id>.<attempt>`, then registers that listener and its
descriptor through `EXT_COMMAND`. The server derives `@name`, extension ID,
definition revision, and module hash from the authenticated endpoint; the
descriptor cannot claim them. An unrelated endpoint may still squat on a raw
channel name, but it cannot register that listener as another extension's CLI
surface.

The descriptor is UTF-8 JSON, capped at 64 KiB like channel metadata, with this
initial shape:

```json
{
  "protocol": "blit.cli.v1",
  "summary": "Build and publish this workspace",
  "commands": [
    {
      "path": ["build"],
      "summary": "Build one target",
      "usage": "build [--release] TARGET",
      "options": [
        {
          "names": ["-r", "--release"],
          "takes_value": false,
          "help": "Build optimized artifacts"
        }
      ]
    }
  ]
}
```

`protocol`, `summary`, and `commands` are required. A command `path` is an
array of literal subcommand tokens; an empty path describes the namespace
root. `summary`, `usage`, `options`, and option `help` are presentation data.
Unknown fields are ignored so the descriptor can grow compatibly. The CLI
sanitizes control characters and never evaluates descriptor text or installs
shell code. It uses the descriptor for listing, help, and static shell
completion, but not for client-side argument validation: the extension remains
the authority on its arguments and errors.

The server rejects invalid UTF-8, invalid JSON, a protocol value other than
`blit.cli.v1`, or missing required fields with `INVALID`. It validates only the
discovery envelope and ownership; it does not interpret application options or
execute descriptor content.

`blit ext commands` discovers and prints the live directory. Root help
(`blit --help`) remains local and does not unexpectedly contact a server;
explicit `@name --help`, `blit ext commands`, and shell completion query the
selected server. A client may cache discovery by `(boot_generation,
directory_revision)`. A server restart or revision change invalidates that
cache. V1 completion covers advertised namespaces, command paths, and option
names only; dynamic, extension-executed completion is future work.

After discovery, the CLI connects to the advertised listener. Each accepted
channel carries one invocation using `blit.cli.v1`. Every channel `DATA`
payload begins with a one-byte kind.

Client-to-extension payloads are:

| Kind | Name        | Body                                               |
| ---- | ----------- | -------------------------------------------------- |
| 1    | `INVOKE`    | `[flags:1][argc:2] repeated{[len:4][UTF-8 arg:N]}` |
| 2    | `STDIN`     | `[data:N]`                                         |
| 3    | `STDIN_EOF` | empty                                              |
| 4    | `CANCEL`    | empty                                              |

`INVOKE` must be the first payload. Its arguments are exactly the tokens after
`@name`; flag bit 0 means stdin will be streamed and all other bits are
reserved. Without that flag, stdin is closed from the start. With it, the CLI
sends zero or more `STDIN` messages followed by one `STDIN_EOF`.

Extension-to-client payloads are:

| Kind | Name     | Body                                           |
| ---- | -------- | ---------------------------------------------- |
| 1    | `STDOUT` | `[data:N]`                                     |
| 2    | `STDERR` | `[data:N]`                                     |
| 3    | `LOG`    | `[level:1][UTF-8 message:N]`                   |
| 4    | `RESULT` | `[content_type_len:2][content_type:N][data:M]` |
| 5    | `EXIT`   | `[code:4][UTF-8 detail:N]`                     |

Output is not a PTY stream: the channel has no terminal state, resize, or input
mode and performs no escape-sequence interpretation. The CLI may copy `STDOUT`
and `STDERR` bytes to its own corresponding streams. `LOG.level` values 0
through 4 mean trace, debug, info, warning, and error. An invocation may emit
any number of stream or log messages and at most one structured result, then
exactly one `EXIT`; no payload follows `EXIT`. The signed `i32` code has the
same native-CLI truncation caveat as `blit run`. `--json` exposes these frames
as structured CLI events rather than changing what the extension sends.

An unknown `blit.cli.v1` payload kind or malformed body closes that invocation
channel as a protocol error; compatibility for a future command protocol uses
a new descriptor `protocol` value.

Normal channel windows provide backpressure in both directions. Closing the
client side is cancellation even if `CANCEL` could not be delivered. If the
extension attempt or listener disappears, the invocation fails and is never
automatically retried against a restarted attempt. One attempt may accept many
invocation channels, but it still has one Wasmi thread; its SDK event loop must
multiplex them or deliberately serialize work.

## Process family

Feature bit **13** (`FEATURE_PROCESS`) advertises non-PTY child-process
execution. The family occupies the free direction-local `0xC0` through
`0xC5` block. Git reserves `0xB5` through `0xBF`, so this RFC does not consume
that space.

This is a normal blit family. A Wasmi extension reaches it through `blit_v1.send`
and `blit_v1.recv`; a network client sends the same packets over its existing
transport. The server implementation is shared. When `FEATURE_PROCESS` is
advertised, every logical client may use it.

Process IDs are client-allocated 32-bit integers scoped to one logical
endpoint. An ID cannot be reused until a failed `PROCESS_STARTED` or final
`PROCESS_EXIT` has been received. Integers are little-endian. Arguments and
environment values are arbitrary bytes without NUL; environment keys also
cannot contain `=`. Paths are server-native byte paths on Unix and valid UTF-8
paths on Windows. Stream payloads are unrestricted bytes.

### Client to server

| Opcode | Name              | Layout                                                                                                                                                                     |
| ------ | ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `0xC0` | `PROCESS_SPAWN`   | `[nonce:2][process_id:4][flags:1][cwd_kind:1][src_pty_id:2][cwd_len:4][cwd:N][argc:2] repeated{[len:4][arg:M]}[envc:2] repeated{[key_len:2][key:K][value_len:4][value:V]}` |
| `0xC1` | `PROCESS_STDIN`   | `[process_id:4][offset:8][data:N]`                                                                                                                                         |
| `0xC2` | `PROCESS_ACK`     | `[process_id:4][stream:1][bytes:8]`                                                                                                                                        |
| `0xC3` | `PROCESS_CONTROL` | `[nonce:2][process_id:4][action:1][value:4]`                                                                                                                               |

`PROCESS_SPAWN` executes `argv[0]` directly. It never invokes a shell;
clients which want shell parsing must explicitly run a shell with an argument
such as `-c`. `argc` must be non-zero. Argument count and
combined bytes use the same caps as extension arguments. `envc` is capped at 256,
each environment key at 255 bytes, each value at 64 KiB, and combined key and
value bytes at 1 MiB. Duplicate keys are `INVALID`.

Spawn flags are bit 0 `MERGE_STDERR` and bit 1 `CLEAR_ENV`. By default the
child receives a small server-defined baseline such as `PATH`, locale, and a
temporary directory, plus the explicit environment entries. `CLEAR_ENV`
removes that baseline. The server never implicitly forwards credentials, file
descriptors, or `BLIT_*` variables. Explicit environment entries replace
baseline entries.

`cwd_kind` is 0 for the server's default directory, 1 for the explicit
`cwd`, and 2 for the current directory of `src_pty_id`. Fields unused by the
selected kind must be empty or zero. Resolving a terminal directory happens
atomically during spawn and does not attach the new process to that terminal.
For `cwd_kind = 2`, an unknown terminal or one without a current directory,
including an exited terminal, refuses the spawn with
`PROCESS_STARTED(status = NOT_FOUND)` and zero windows. The server must not
fall back to its default directory or interpret the empty relative path as an
absolute root.

`PROCESS_ACK.stream` is 1 for stdout and 2 for stderr. It acknowledges total
payload bytes delivered to the application, not merely received by a socket.
When stderr is merged, `PROCESS_STARTED.stderr_window` is zero, the server sends
merged bytes only as `PROCESS_STDOUT`, and it rejects stderr ACKs.

Control actions are:

| Value | Name          | Meaning                                                    |
| ----- | ------------- | ---------------------------------------------------------- |
| 1     | `CLOSE_STDIN` | Deliver EOF after all accepted stdin bytes                 |
| 2     | `TERMINATE`   | Request portable graceful termination of the process tree  |
| 3     | `KILL`        | Force termination of the process tree                      |
| 4     | `SIGNAL`      | Send the platform signal in `value`, or report unsupported |

`value` must be zero except for `SIGNAL`. `TERMINATE` and `KILL` are the
portable operations; signal numbers are deliberately platform-specific.
The initial family has no detach operation.

### Server to client

| Opcode | Name                 | Layout                                                                                          |
| ------ | -------------------- | ----------------------------------------------------------------------------------------------- |
| `0xC0` | `PROCESS_STARTED`    | `[nonce:2][status:1][process_id:4][stdin_window:8][stdout_window:8][stderr_window:8][detail:N]` |
| `0xC1` | `PROCESS_STDOUT`     | `[process_id:4][offset:8][data:N]`                                                              |
| `0xC2` | `PROCESS_STDERR`     | `[process_id:4][offset:8][data:N]`                                                              |
| `0xC3` | `PROCESS_ACK`        | `[process_id:4][bytes:8]` — cumulative consumed stdin bytes                                     |
| `0xC4` | `PROCESS_EXIT`       | `[process_id:4][reason:1][code:4][detail:N]`                                                    |
| `0xC5` | `PROCESS_CONTROLLED` | `[nonce:2][status:1][process_id:4][detail:N]`                                                   |

`PROCESS_STARTED` is the single reply to `PROCESS_SPAWN`. On failure,
`status != OK`, the windows are zero, no `PROCESS_EXIT` follows, and the ID is
immediately reusable. Every `PROCESS_CONTROL` receives one
`PROCESS_CONTROLLED`; accepted control is serialized with process exit so the
reply precedes an exit caused by that action.

Stdout and stderr each preserve byte order but have no relative ordering with
one another. They are raw bytes, not UTF-8 and not line-framed. Offsets begin
at zero. The server reads both OS pipes concurrently, delivers all accepted
output, observes both EOFs and the child wait result, and only then emits the
single terminal `PROCESS_EXIT`; no stream data follows it. Exit reasons
distinguish returned, signalled, killed, protocol violation, and host failure.
`code` is meaningful only for a normal return or a platform signal.

### Flow control and ownership

The three stream windows are independent cumulative byte windows. The client
may send stdin through `acked_stdin + stdin_window`; the server may send each
output stream through its client ACK plus that stream's negotiated window.
An incorrect offset, decreasing ACK, or window overrun is a protocol
violation for that process. Normal backpressure stops reading or writing the
corresponding OS pipe and lets the child block; it never creates an unbounded
server queue.

Every child belongs to its creating logical endpoint and, for an extension, to
the current attempt. Endpoint close, attempt cancellation, or trap closes
stdin, gracefully terminates the process group or Windows job, waits a short
server-defined grace period, and force-kills the remainder.
The server reaps every child. A restarted extension attempt gets no handles to
the previous attempt's children. Persistent extensions must therefore assume
that an interrupted subprocess side effect can be repeated after restart.

Children run with the blit server's OS identity. Wasm isolation does not
sandbox a native child; deployments which need that separation must sandbox
the blit server or its process runner externally.

Programs requiring a controlling terminal continue to use the terminal
family. `PROCESS_*` is intentionally pipe-based; adding PTY flags here would
create two subtly different terminal APIs.

### SDK surface

The guest SDK presents Rust-shaped convenience without changing the host ABI:

```rust
let mut child = blit
    .process()
    .command("rg")
    .arg("needle")
    .cwd("/workspace")
    .spawn()?;

while let Some(event) = child.recv()? {
    match event {
        ProcessEvent::Stdout(bytes) => consume(bytes)?,
        ProcessEvent::Stderr(bytes) => report(bytes)?,
        ProcessEvent::Exit(status) => return status.into_result(),
    }
}
```

The SDK multiplexes process events with every other server packet and sends
ACKs only after the application consumes data. It does not attempt to make
`std::process::Command` work transparently on a core Wasm target.

## Failure isolation

There are no per-extension resource settings, execution budgets, or wall-clock
deadlines. `EXT_RUN` carries no execution-tuning fields.

Fixed packet sizes, byte windows, and bounded outboxes are protocol and
dispatcher invariants described with their respective families. Wasmi must
also be configured so guest memory, tables, and instances cannot exhaust the
server. Fuel metering supplies yield points for cancellation, not a total
execution budget. These containment details are not extension-visible or
configurable per invocation.

Cancellation marks the endpoint first, wakes a blocked receive, and refuses
new sends. A running fuel slice reaches cancellation at its next yield. Host
panics are caught at the extension thread boundary and reported as `HOST_FAILURE`;
they must not unwind into server code.

The server must validate all extension packets exactly as it validates
network packets. In-process origin is not trusted origin.

## CLI behavior

```bash
blit run --on prod extension.wasm arg1 arg2
blit run --on prod --restart on-failure extension.wasm arg1
blit run --on prod --restart always --persist --name builder extension.wasm arg1
```

The command grammar is `blit run [RUN_OPTIONS] FILE [ARGS...]`. Every token
after `FILE` is passed verbatim as an extension argument, including tokens
beginning with `-`; no `--` separator is required. Blit run options such as
`--detach`, `--restart`, `--persist`, `--name`, and connection options such as
`--on` must therefore appear before `FILE`.

The CLI:

1. reads the file under a configurable local size cap;
2. computes its full BLAKE3 digest;
3. sends `EXT_RUN`;
4. on `NEED_OBJECT`, uploads acknowledged chunks;
5. streams attached stdout/stderr/log events without allocating a PTY;
6. exits with the module code for `RETURNED`, or non-zero for other reasons.

`EXT_EXIT` and `--json` preserve the full signed `i32` module code. The CLI
passes a returned code to the native process-exit API, whose observable range
is platform-specific; Unix shells see only the low eight bits (`0` through
`255`). Callers which need the full value must consume the structured event
rather than the CLI process status.

`--restart` accepts `never` (the default), `on-failure`, or `always`.
`--persist` requires `--name`, implies `--detach`, and stores desired state for
future blit server processes. `--json` emits supervisor, attempt, and event
records as NDJSON envelopes.

An attached `on-failure` or `always` command follows successive attempts and
does not exit merely because one attempt failed. It exits when the supervisor
reaches `STOPPED`, is cancelled, or the connection fails. `--detach` returns
after `RUNNING`. The management surface is:

```bash
blit ext list
blit ext status NAME_OR_ID
blit ext attach NAME_OR_ID
blit ext update [UPDATE_OPTIONS] NAME FILE [ARGS...]
blit ext restart NAME_OR_ID
blit ext enable NAME_OR_ID
blit ext disable NAME_OR_ID
blit ext remove NAME_OR_ID
blit ext commands
```

`blit extension` is an alias for `blit ext`.

`list` reports the ID, durable or descriptive name, definition revision, full
module hash, desired state, phase, attempt, and restart policy. Persistent
names are accepted wherever `NAME_OR_ID` appears; ambiguous transient names
must be addressed by ID. `update` is restricted to persistent names and uses
the replacement semantics in
[§ Instances and module versions](#instances-and-module-versions). Its options
and connection flags precede `NAME`, while every token after `FILE` is a new
stored extension argument. It preserves the current restart policy unless an
update option explicitly replaces it. `commands` lists the live `@name`
surfaces described in
[§ Extension-provided CLI commands](#extension-provided-cli-commands).

The local pathname is never sent as module identity. It may appear in local
diagnostics. Servers and peers see the invocation name when one was supplied
and the full content hash.

## Protocol compatibility

Clients check feature bits 11, 12, and 13 before sending these families. Older
clients ignore their S2C opcodes. Older servers do not advertise them, and
`blit run` reports an upgrade requirement rather than attempting an upload.
`blit ext commands` and `@name` dispatch require both extension bit 11 and
channel bit 12.

Kind-multiplexed envelopes have an explicit skip rule. Clients ignore an
unknown S2C kind under `EXT_INFO` or `0x95` as one complete packet. Servers
likewise ignore one complete packet with an unknown C2S kind under
`EXT_COMMAND` or `0x95`; it is not a connection-level protocol violation and
changes no handle state. A new C2S request kind which requires a reply must
have a new feature bit or other explicit negotiation, so a client never waits
on a server which can only skip it. A malformed payload for a known kind
remains `INVALID` or a family-local protocol violation as specified by that
family.

Gateways, mux, proxy, WebRTC, WebSocket, and WebTransport forward the new
packets unchanged. Only the upstream blit server interprets them. The Wasm host
ABI is versioned independently through its import module name (`blit_v1`),
while the guest observes ordinary protocol features through `HELLO`.

The native channel IDs are per logical connection, so forwarding requires no
gateway-global allocation or rewriting.

## Rejected alternatives

### Per-feature Wasm/WASI bindings

Bindings for terminals, FS, Git, LSP, KV, network relay, and every
future family duplicate the wire protocol and make Wasm support lag normal
clients. They also encourage runtime-specific validation and cleanup.
Packets give exact parity through the two packet imports; the only additional
version-1 import is the direct clock read.

### WASI or WASIX subprocess spawning

Standard WASI currently describes the guest's CLI environment and exit, not
portable child-process creation. Adopting a runtime-specific `proc_spawn`
extension would couple extensions to that runtime, expose process execution only
to Wasm guests, and still require blit-specific lifecycle glue.
The process packet family provides the same streaming operation to all clients
without adding another Wasm import. Optional WASI remains useful for the
extension's own constrained environment.

### Loopback or in-memory fake socket

A duplex stream could feed the existing connection handler and is a useful
prototype, but it preserves framing, handshake transport machinery, and
writer tasks solely to move data within one process. Extracting packet
dispatch is the intended architecture. The extension still receives the
normal logical handshake from its endpoint.

### Typed internal API exposed directly to Wasm

The server should have typed internal handlers, but exposing their Rust shape
as the guest ABI couples extensions to server implementation details and still
requires a serialization schema. The stable packet protocol is already that
schema. Rust SDK types can wrap it without becoming host ABI.

### JSON RPC as the universal boundary

JSON is convenient for debugging but expensive for bulk bytes, ambiguous for
integer widths, and a second protocol. Channel application payloads may use
JSON voluntarily; core client operations remain binary blit packets.

### Shared extension worker pool

A shared pool uses fewer native stacks for mostly idle extensions, but makes
thread-level profiling, crash attribution, debugger inspection, and resource
ownership less direct. Dedicated named threads are intentionally simpler to
operate. Blocked receives park without consuming CPU; restart backoff owns no
extension thread at all.

## Implementation plan

1. **Thread naming.** Add the platform-aware shared naming helper, name blit's
   Tokio workers and currently unnamed explicit threads, and test sanitizing,
   compaction, and stable ID suffixes.
2. **Packet endpoint refactor.** Extract logical client creation, packet
   dispatch, bounded outbox, identity propagation, and common disconnect.
3. **Native channels.** Implement the `0x95` channel registry, flow control,
   identity, cleanup, and codecs.
4. **Processes.** Implement the `0xC0` through `0xC5` process family,
   per-stream flow control, concurrent pipe draining, process-tree cleanup,
   codecs, and protocol tests from a network client.
5. **Module objects.** Implement BLAKE3 run probe, chunk upload, validation,
   persistent CAS, pending-run single-flight, and cache eviction.
6. **Supervisor.** Add stable extension/attempt identity, definition revisions,
   atomic update, restart policy, backoff, durable desired state, startup
   restoration, and crash-safe control.
7. **Wasmi host.** Add one named thread per running extension attempt, bounded
   endpoint queues, Wasmi containment, fuel-based cancellation yielding,
   direct clocks, bootstrap context, attempt lifecycle, and event retention.
8. **Command directory.** Implement `EXT_COMMAND` registration and discovery,
   descriptor validation, live-listener ownership, revisioned pagination, and
   the `blit.cli.v1` channel protocol.
9. **Rust SDK and CLI.** Add `blit-guest`, a Rust example extension, `blit run`,
   process and command-provider wrappers, extension control and update commands,
   `@name` dispatch, help, listing, and static completion.

Each phase has a vertical protocol test with at least two logical clients.
The extension phases additionally verify cache hit (no upload), cache miss,
nonce release before later ID-keyed status changes, hash mismatch, invalid
imports, runaway-loop cancellation, cleanup after a trap, restart policy,
backoff persistence, crash-safe disable, restoration after a fresh server
process, INIT ordering and contents, direct realtime and monotonic clocks,
multiple extensions using one hash, persistent-name conflict, and update
cache-hit, cache-miss, no-op, expected-ID and expected-revision races, revision,
cleanup, and rollback cases. Multiplexed-family tests send unknown kinds in both directions.
Command tests cover registration ownership, descriptor parsing, revisioned
pagination, disappearance and re-registration across attempts, invocation
framing, output ordering, backpressure, cancellation, and the no-retry rule.
Process tests additionally cover binary output, independent stdout/stderr
ordering, backpressure, stdin EOF, missing `cwd_kind = 2` context, merged-stderr
window negotiation, spawn failure, signals where supported, and
process-tree cleanup on endpoint loss.

## Open questions

- Should persistent object eviction be automatic by default or operator-only
  until access-time accounting is proven reliable across crashes?

None of these questions changes the central boundary: extensions are logical
clients, and their host ABI exchanges ordinary blit packets.
