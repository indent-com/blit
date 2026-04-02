# blit: terminal state, not terminal replay

*About a 10 minute read.*

If you only have 30 seconds, here is the whole argument:

- Indent previously used `xterm.js`, but in our setup, attaching to a live terminal meant replaying terminal bytes and history to reconstruct current state.
- That created startup-time and latency problems exactly where a remote terminal should feel instant.
- `blit` flips the model: the server owns parsed PTY state, and clients attach to current state plus incremental diffs instead of rebuilding the world from raw output.
- That turns out to be the right primitive for a product where terminals need to work for humans, browsers, and agents at the same time.

Grounded facts for this post:

- Indent previously used `xterm.js`.
- Replaying terminal byte history to reconstruct state caused real startup and latency issues in our setup.
- The current Indent client integrates `blit` over a WebRTC data channel and presents separate interactive and background terminal session views.

Everything else below is an attempt to explain why those facts point toward a very particular architecture.

---

## The problem was not rendering. It was ownership.

A lazy description of a remote terminal goes like this: bytes come out of a PTY, the browser renders them, and keystrokes go back in.

That description is not wrong. It is just missing the actual problem.

A terminal is a state machine. It has a grid of cells, scrollback, cursor position, cursor mode, alternate screen state, mouse modes, color and style attributes, wide character behavior, title changes, and escape-sequence-driven semantics that only exist after parsing the byte stream. If you store only bytes, every new client has to *re-derive* that state. If you store state, new clients can attach to something current.

That distinction sounds academic until the terminal becomes long-lived, shared, remote, multi-session, or agent-driven. At that point the past stops being just history. It becomes part of your startup path.

That was the failure mode for us with `xterm.js`. The issue was not that `xterm.js` is bad. It is not. It is the standard for a reason. The issue was that, in our setup, reconstructing current terminal state by replaying terminal bytes and history was work we kept making fresh clients pay for. That cost startup time, added latency in the wrong places, and made a terminal feel older than the machine it was attached to.

> The past should not be your startup path.

That is the sentence that leads to `blit`.

## What blit actually is

From the outside, `blit` looks pleasantly small:

```bash
curl -sf https://install.blit.sh | sh
blit open
blit share
blit open --ssh myhost
blit start htop
blit show 1
blit send 1 "q"
```

From the inside, it is not small at all, and that is exactly why the outside can be.

`blit` is a terminal streaming stack. At the center is `blit-server`, which owns PTYs, scrollback, parsed terminal state, and per-client frame pacing. Around it sit:

- `blit-gateway` for browser access over WebSocket and WebTransport,
- `blit-webrtc-forwarder` for peer-to-peer sharing over WebRTC DataChannels,
- a CLI that can talk over Unix sockets, TCP, SSH, and WebRTC,
- a browser runtime that applies frame diffs in WASM and renders with WebGL,
- and React/Solid bindings for embedding the terminal surface in your own UI.

The system is built around one idea: **the server should understand what is on screen, not just shuttle bytes**.

Traditional browser terminals usually look like this: the backend reads raw bytes from a PTY and forwards them downstream; the browser runs a terminal emulator, parses VT escape sequences, and reconstructs state for itself. The server is mostly a byte pipe.

`blit` inverts that model. The server parses PTY output through `alacritty_terminal` and maintains the full parsed terminal state: cell grid, colors, styles, cursor, scrollback, title, modes, the lot. When it is time to update a client, the server does not dump the raw byte stream. It diffs the current terminal state against what that specific client last acknowledged, encodes only the delta, LZ4-compresses it, and sends the result.

That changes both startup cost and product shape.

## How a frame moves

The data flow from PTY to pixels looks like this:

![Frame pipeline](../claude/frame-pipeline.png)

1. **PTY emits raw bytes** — program output, escape sequences, the works.
2. **`alacritty_terminal` parses** — the VT state machine converts bytes into a structured cell grid.
3. **Server diffs against last ACK** — for each connected client, the server compares the current grid to what that client last confirmed it received.
4. **Encode + LZ4 compress** — the diff is encoded as three operation types: `COPY_RECT` (scrolled regions), `FILL_RECT` (blank regions), and `PATCH_CELLS` (individual changed cells). Then it is compressed.
5. **Gateway proxies** — the stateless gateway handles browser auth and just forwards binary frames.
6. **WASM decompresses and applies** — a Rust WASM module patches the browser's local cell grid. No VT parsing in browser JavaScript.
7. **WebGL renders** — shader programs draw backgrounds, glyph quads, and cursor state from zero-copy vertex buffers exposed out of WASM memory.
8. **Browser ACKs** — acknowledgements retire in-flight frames and feed RTT estimation.

Each cell is encoded in a compact fixed-width layout with overflow handling for larger Unicode content. The wire format is not protobuf, not JSON, and not a generic schema layered over the terminal. It is a hand-rolled binary protocol because the system wants to send terminal state efficiently, not narrate it politely.

The important detail is not just compression. It is *what* gets compressed. The unit of transmission is not “whatever bytes the PTY just emitted.” It is “what changed in the terminal state for this particular client.”

## Per-client congestion control: the part almost nobody ships

When multiple clients are watching the same terminal, they should not all be forced to move at the speed of the slowest one.

If a developer on a fast machine is actively focused on a session, that experience should stay hot. If another viewer is slow, remote, or just watching a background terminal, that client should get updates paced to what it can actually handle. And if an agent is polling state on a rougher connection, it should not accidentally degrade the human user's live terminal.

`blit` tracks detailed per-client congestion state:

- **RTT estimation** via ACK round-trips,
- **bandwidth estimation** from delivery rates and ACK cadence,
- **frame windows** bounded by the bandwidth-delay product,
- **display pacing** based on the client's actual refresh rate, backlog depth, and frame apply time,
- and **preview budgeting** so focused PTYs get priority while background sessions share leftover bandwidth.

This is the kind of systems work that users mostly notice only by absence. A fast client on localhost gets frames as fast as it can render them. A thinner client gets paced to its capacity. Neither turns into a hidden global bottleneck.

That matters especially for Indent's current terminal shape, because there is a real distinction between interactive and background sessions. `blit`'s focus and visibility model maps directly onto that product reality.

## Five transports, one protocol

The wire protocol is defined in terms of reliable ordered byte streams, not in terms of one transport the system happens to prefer this year.

| Transport | Use case |
|---|---|
| **Unix socket** | Primary. Server ↔ gateway, server ↔ CLI. |
| **WebSocket** | Browser connections through the gateway. |
| **WebTransport** | QUIC/HTTP3 for lower-latency browser connections. Self-signed certs with hash pinning. |
| **WebRTC DataChannel** | Peer-to-peer. `blit share` prints a URL; anyone with the passphrase can connect. Ed25519-signed signaling. |
| **SSH** | `blit open --ssh myhost` tunnels the protocol over SSH. |

The rest of the system is meant to be transport-agnostic. On the Rust side, everything becomes `AsyncRead` + `AsyncWrite`. On the TypeScript side, anything implementing the `BlitTransport` interface can plug into a workspace.

That is one reason `blit` embeds so naturally into products: the transport choice is not the architecture. It is just one layer of it.

## Agent subcommands: terminals as an API

```bash
ID=$(blit start --cols 200 -t build make -j8)
blit wait "$ID" --timeout 120 --pattern 'BUILD OK'
blit history "$ID" --from-end 40 --limit 40
blit send "$ID" ""
blit restart "$ID"
blit close "$ID"
```

This part matters a lot for Indent.

Every subcommand opens a connection, does one thing, and exits. Output is plain text: TSV for `list`, raw terminal text for `show` and `history`. Exit codes are meaningful. Errors go to stderr. There is no custom SDK, no long-lived WebSocket session to manage, and no pretense that screen scraping is an API.

`show` gives you the current viewport — what a human would see right now. `history` gives you scrollback with pagination. `send` pushes keystrokes with C-style escapes. `wait` blocks until the process exits or a regex matches new output.

For AI agents, this is huge. An agent does not necessarily want to speak a terminal protocol directly. It wants to start a process, read the latest output, wait for a condition, send some input, and move on. `blit` turns the terminal into a first-class control surface instead of leaving it as a visual side effect.

## Architecture: stateful server, stateless edges

![System architecture](../claude/architecture.png)

The design falls out of one decision: **the server owns the state**.

`blit-server` hosts PTYs, scrollback, parsed terminal state, and per-client pacing. `blit-gateway` is stateless: it authenticates browser clients and proxies binary messages. That means PTYs survive gateway restarts. The gateway can sit behind a reverse proxy. The CLI can embed a temporary gateway when it wants browser access without requiring a permanent deployment.

For embedding in your own app, `fd-channel` mode lets an external process pass pre-connected client file descriptors into `blit-server` via `SCM_RIGHTS`. Your service can own connection acceptance or auth policy; `blit` owns the terminal machinery.

On the frontend, `@blit-sh/react` and `@blit-sh/solid` are intentionally thin wrappers over `@blit-sh/core`:

```tsx
import { BlitTerminal, BlitWorkspaceProvider } from "@blit-sh/react";
import { BlitWorkspace } from "@blit-sh/core";

<BlitWorkspaceProvider workspace={workspace}>
  <BlitTerminal sessionId={session.id} style={{ width: "100%", height: "100vh" }} />
</BlitWorkspaceProvider>
```

The workspace manages connections, sessions, focus, visibility, and lifecycle. The terminal surface renders a session by ID. That is a clean API boundary for embedding a real remote terminal rather than a demo widget.

## The rendering stack

Most web terminals render with DOM elements or Canvas 2D. `blit` renders with WebGL.

The WASM module builds vertex buffers directly in linear memory. Two shader programs do the drawing: one for cell backgrounds and cursor rectangles, one for glyph quads sourced from a glyph atlas. The atlas itself is a Canvas 2D surface that rasterizes glyphs on demand and uploads them to a GPU texture.

Vertex data crosses from WASM to JavaScript as `Float32Array` views over shared memory — no extra serialization layer, no redundant copies. The renderer batches aggressively.

Predicted echo is another nice detail. The server tracks PTY echo and canonical mode flags via `tcgetattr` and includes those mode bits in frame state, so the browser can tell when local predicted echo is safe instead of guessing.

This is a good example of what makes `blit` feel engineered rather than assembled. Rendering, protocol, and terminal semantics are designed together.

## How it compares

![Feature comparison](../claude/comparison.png)

Every tool in this space made a reasonable set of tradeoffs for its era.

- `ttyd` and `gotty` are wonderfully simple if all you need is one terminal in a browser.
- `Mosh` was ahead of its time in understanding that the server should track terminal state and ship diffs, but it is not a browser-first system.
- `Eternal Terminal` solves session persistence over unreliable links elegantly, again without a browser story.
- `xterm.js` is extremely flexible precisely because it is a library, not a full product stack.

`blit` is unusual because it is opinionated about the whole stack at once: server-side parsing, binary diffs, per-client pacing, multiple transports, browser rendering, and agent-friendly CLI semantics. The tradeoff is that it is a bigger thing to understand than the simplest browser-terminal servers, and a less blank canvas than `xterm.js`. For the use case it targets, that tradeoff is the whole point.

## Why this is the right answer for Indent

Here is the narrow, accurate claim:

For Indent, `blit` is a better primitive than replaying terminal history into browser state.

Why?

Because the terminal in Indent is not serving one audience.

It has to work for:

- a human user who wants fast attach, low input latency, readable output, resizing, and selection,
- a browser UI that needs current state instead of a byte log it must reconstruct from scratch,
- and an agent that benefits from stateless terminal operations like `start`, `show`, `history`, `send`, and `wait`.

That is already enough to eliminate a lot of simpler designs.

It also matters that Indent explicitly distinguishes interactive and background terminal sessions. `blit`'s focus and visibility model fits that reality well. Focused sessions can get the budget. Background sessions can stay alive and observable without being treated as equal-priority rendering work.

And there is one more important point: `blit` is transport-agnostic all the way up the stack. In the current Indent client, the integration uses a WebRTC data channel transport. That is not a bolt-on curiosity. It is a sign that the underlying abstraction is right. The same terminal system can be local, remote, browser-facing, shareable, and agent-driven without changing its core mental model.

That is what makes `blit` feel like infrastructure rather than a widget.

## The reconnect problem, solved

This is worth dwelling on because it is the issue that started everything for us.

With `xterm.js`, when a user reconnects — browser refresh, network blip, new tab — the server has to replay raw PTY byte history so the browser can reconstruct terminal state by parsing every byte from scratch. A build that has been running for twenty minutes means twenty minutes of bytes to replay, or else truncation and lost context. Either way, you are choosing between latency and correctness.

`blit` removes that tradeoff. Because the server holds parsed terminal state, a reconnecting client gets a compressed snapshot of *what is true right now*. Session age is mostly irrelevant. A terminal that has been running for six hours reconnects on the basis of current state, not on the basis of all the bytes that led there.

This also makes consistency structural, not aspirational. When the browser runs its own VT parser, it can drift from the true terminal state around Unicode width edge cases, incomplete escape sequences, or timing-dependent mode changes. In `blit`, the server is the single source of truth and the browser applies patches. If anything diverges, the next delta snaps it back.

## What almost everybody should learn here

Even if you never use `blit`, there is a useful lesson in the architecture.

“Terminal in the browser” is not one problem. It is at least three:

1. terminal emulation,
2. state ownership,
3. transport policy.

If you solve only the first one, you can still end up with a terminal that technically works but product-wise feels slow, heavy, or fragile.

`blit` is interesting because it starts at the second and third questions. Where should truth live? How should clients attach to it? Once that answer is solid, the rest of the stack gets cleaner.

## Try it

No install needed:

```bash
docker run --rm grab/blit-demo
```

This starts a sandboxed container with `blit share`, fish, neovim, htop, and the usual suspects. It prints a URL. Open it — you are in a terminal. Open it in a second tab — both update independently, paced to their own render speed. That is per-client congestion control made visible.

Or install it:

```bash
curl -sf https://install.blit.sh | sh
blit open                              # local browser terminal
blit share                             # share over WebRTC — prints a URL
blit open --ssh myhost                 # remote terminal in your browser
blit start htop && blit show 1         # start a session, read what's on screen
```

Also available via [Homebrew](https://github.com/indent-com/homebrew-tap), [APT](https://install.blit.sh), and [Nix](https://github.com/indent-com/blit#nix). The code is at [github.com/indent-com/blit](https://github.com/indent-com/blit).

## Closing

The best thing about `blit` is not just that it is written in Rust, or uses WASM, or renders with WebGL, or can share a terminal over WebRTC.

Those are all good choices.

The best thing is the underlying decision:

> a terminal session is a stateful product primitive, not a historical byte stream that every client must replay to deserve the present.

For Indent, that is the right answer.
