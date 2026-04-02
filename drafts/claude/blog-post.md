# Why We Built blit: Terminal Streaming for Browsers and AI Agents

**TL;DR:** blit is a single Rust binary that hosts PTYs, tracks full parsed terminal state, computes per-client binary diffs, and ships only the delta — LZ4-compressed, WebGL-rendered in the browser. Per-client congestion control means a phone on 3G doesn't stall a workstation on localhost. Agent subcommands let AI drive terminals programmatically.

---

## The problem: terminals in a browser shouldn't feel like a compromise

[Indent](https://indent.com) is an AI coding assistant that lives in the cloud. Users spin up remote sandboxes, run builds, watch logs, and debug — all through a browser. The terminal is load-bearing infrastructure. If it stutters, lags, or drops output, the entire experience feels broken.

We had xterm.js. It's the standard — battle-tested, widely used, and it works. But xterm.js is a client-side terminal emulator backed by a dumb byte pipe. When a user reconnects or opens a new tab, the server has to replay the full PTY history so the browser can reconstruct the terminal state by parsing every byte from scratch. That meant startup latency that grew with session age, and bandwidth costs that scaled with history size rather than what actually changed on screen.

The alternatives each solve part of the problem. ttyd and gotty put a terminal in a browser, but give you one PTY per process and no delta updates. Mosh does state-synchronization diffs, but has no browser support. Eternal Terminal keeps sessions alive across disconnects, but again, no browser.

None of them think about the terminal as a *streamable, observable surface* that multiple consumers — humans, browsers, AI agents — can tap into concurrently. That's the problem blit was built to solve.

## What blit actually is

blit is a terminal multiplexer that speaks natively to browsers. One binary. No configuration. Install it and run `blit open` — it starts a server, opens your browser, and you're in a terminal. That's it.

```bash
curl -sf https://install.blit.sh | sh
blit open
```

But underneath that simplicity is a system designed around a specific insight: **the server should understand what's on screen, not just shuttle bytes**.

Traditional web terminals work like this: the backend reads raw bytes from a PTY and forwards them to the browser. The browser runs a JavaScript terminal emulator (usually xterm.js) that parses VT escape sequences and renders them. The server is dumb pipe.

blit inverts this. The server parses PTY output through `alacritty_terminal` — the same parser that powers the Alacritty terminal emulator — and maintains the full parsed terminal state: a grid of cells, each with its character, foreground color, background color, and style attributes. Cursor position. Scroll offset. Mouse mode. Alt screen. Everything.

When it's time to send a frame to a connected client, the server doesn't dump the raw byte stream. It diffs the current terminal state against what that specific client last acknowledged receiving, encodes only the changed cells, LZ4-compresses the result, and sends the delta.

## How a frame moves

The data flow from PTY to pixels looks like this:

![Frame pipeline](frame-pipeline.png)

1. **PTY emits raw bytes** — program output, escape sequences, the works.
2. **alacritty_terminal parses** — VT state machine converts bytes into structured cell grid state.
3. **Server diffs against last ACK** — for each connected client, the server compares the current grid to what that client last confirmed it received.
4. **Encode + LZ4 compress** — the diff is encoded as three operation types: `COPY_RECT` (scrolled regions — just a source offset), `FILL_RECT` (blank regions — one cell value), and `PATCH_CELLS` (individual changed cells via bitmask). Then LZ4'd.
5. **Gateway proxies** — the stateless gateway handles WebSocket/WebTransport auth and just forwards binary frames.
6. **WASM decompresses and applies** — a Rust WASM module decompresses the delta and patches it into the browser's local cell grid. No JS terminal emulator. No VT parsing in the browser.
7. **WebGL renders** — two shader programs draw colored rectangles (backgrounds) and textured quads from a glyph atlas. Zero-copy vertex buffers from WASM linear memory.
8. **Browser ACKs** — the client sends an ACK that retires the in-flight frame and feeds into RTT estimation.

Each cell is exactly 12 bytes: flags, foreground RGB, background RGB, and up to 4 bytes of UTF-8. If a codepoint overflows 4 bytes (emoji, complex scripts), it's stored in an overflow table keyed by cell index. The wire format is not protobuf, not JSON, not any external schema. It's a hand-rolled binary protocol where every byte is accounted for.

## Per-client congestion control (the part nobody else does)

When you have multiple clients watching the same terminal — say, a developer on a MacBook and an AI agent polling over a spotty connection — you need them to receive updates independently. If client A is fast and client B is slow, client A should get full-speed frames. Client B should get paced to what it can actually handle. Neither should block the other. And the focused terminal should get priority over background tabs.

blit tracks detailed per-client congestion state:

- **RTT estimation**: EWMA and minimum-path RTT measured from ACK round-trips
- **Bandwidth estimation**: delivered payload rate and ACK-window goodput with jitter tracking
- **Frame window**: in-flight frames capped at the bandwidth-delay product — adapts to latency and display rate
- **Display pacing**: the client reports its actual refresh rate, backlog depth, and frame apply time. The server matches its send rate to what the client can actually render
- **Preview budgeting**: background PTYs share leftover bandwidth after the focused session's needs are met

This is TCP congestion control, but for terminal frames. A fast client on localhost gets frames as fast as it can render them. A phone on cellular gets paced to its capacity. Neither knows the other exists.

This matters for any product where AI agents and human users share the same terminal sessions. An agent consuming output over a slow poll loop shouldn't throttle a developer's live view. Without per-client pacing, the slow consumer blocks the fast one, and everything feels laggy.

## Five transports, one protocol

The wire protocol is defined purely in terms of byte streams. blit currently supports five transports:

| Transport | Use case |
|---|---|
| **Unix socket** | Primary. Server ↔ gateway, server ↔ CLI. |
| **WebSocket** | Browser connections through the gateway. |
| **WebTransport** | QUIC/HTTP3 for lower-latency browser connections. Auto-generated self-signed certs with hash pinning. |
| **WebRTC DataChannel** | Peer-to-peer. `blit share` prints a URL; anyone with the passphrase can connect. Ed25519-signed signaling. |
| **SSH** | `blit open --ssh myhost` tunnels the protocol over SSH — console mode pipes through `nc -U`, browser mode uses `ssh -L` socket forwarding. |

The transport is invisible to the rest of the system. On the Rust side, everything splits into `Box<dyn AsyncRead> + Box<dyn AsyncWrite>`. On the TypeScript side, anything implementing the `BlitTransport` interface (connect, send, close, events) plugs in.

## Agent subcommands: terminals as an API

```bash
ID=$(blit start --cols 200 make -j8)   # start a build, get session ID
blit wait "$ID" --timeout 120           # block until it finishes
blit history "$ID" --from-end 0 --limit 50  # read the last 50 lines
blit close "$ID"                        # clean up
```

Every subcommand opens a connection, does one thing, and exits. Output is plain text — tab-separated for `list`, raw terminal text for `show` and `history`. Exit codes are meaningful. Errors go to stderr. There's no SDK, no client library, no WebSocket subscription to manage. It's just CLI.

`show` gives you the current viewport — exactly what a human would see. `history` gives you the full scrollback buffer with pagination. `send` pushes keystrokes with C-style escapes (`\n` for enter, `\x03` for Ctrl+C, `\x1b[A` for arrow up). `wait` blocks until the process exits or a regex matches in the output.

For AI agents, this is huge. An LLM doesn't need a persistent WebSocket. It needs to run a command, check if it's done, read the output, maybe send some input, and move on. blit makes terminals a first-class API surface without requiring the agent to understand VT escape sequences. The server already parsed those — you just get text.

## Architecture: stateful server, stateless everything else

![System architecture](architecture.png)

The design falls out of one decision: **the server owns all state**.

`blit-server` hosts PTYs, scrollback, parsed terminal state, and per-client pacing. `blit-gateway` is stateless — it authenticates browser clients and proxies binary messages. This means PTYs survive gateway restarts. The gateway can sit behind a reverse proxy. And the CLI can embed a temporary gateway when it needs browser access without a persistent deployment.

For embedding in your own app, the `fd-channel` mechanism lets an external process pass pre-connected client file descriptors to the server via `SCM_RIGHTS`. Your service owns the lifecycle; blit owns the terminals.

On the frontend, `@blit-sh/react` and `@blit-sh/solid` are thin wrappers over `@blit-sh/core`:

```tsx
import { BlitTerminal, BlitWorkspace, BlitWorkspaceProvider } from "@blit-sh/react";

<BlitWorkspaceProvider workspace={workspace}>
  <BlitTerminal sessionId={session.id} style={{ width: "100%", height: "100vh" }} />
</BlitWorkspaceProvider>
```

The workspace manages connections, sessions, and focus. The terminal component renders a session by ID. That's the entire API surface for embedding a fully-functional remote terminal with WebGL rendering, per-client pacing, and multi-transport support into a React app.

## The rendering stack

Most web terminals render with DOM elements or Canvas 2D. blit renders with WebGL.

The WASM module builds vertex buffers directly in linear memory. Two shader programs handle all drawing: a RECT shader for cell backgrounds and cursors, and a GLYPH shader for textured quads from a glyph atlas. The atlas itself is a Canvas 2D element that rasterizes glyphs on demand with row-based bin packing — each unique glyph (codepoint + bold + italic + underline + wide) gets one slot, then the atlas is uploaded to a GPU texture once per frame.

Vertex data crosses from WASM to JS as `Float32Array` views over shared memory — no serialization, no copying. The renderer batches up to 65,532 vertices per draw call.

Predicted echo — showing keystrokes before the server confirms them — is implemented client-side using the PTY's echo and canonical mode flags, which the server detects via `tcgetattr` and packs into each frame's mode bits. The browser knows *whether the terminal is in a state where local echo makes sense* without guessing.

## How it compares

![Feature comparison](comparison.png)

Every tool in this space made a reasonable set of tradeoffs for its era. ttyd and gotty are dead simple — one binary, one terminal in a browser, done. If that's all you need, they're hard to beat. Mosh pioneered the idea that the server should understand terminal state and send diffs, but it predates WebRTC and has no browser story. Eternal Terminal solves the "SSH disconnects kill my session" problem elegantly, but again, no browser. xterm.js is the most flexible — it's a library, not a product — but that flexibility means you're building the server, the multiplexing, the transport, and the pacing yourself.

blit is opinionated about the full stack: server-side parsing, binary diffs, per-client pacing, WebGL rendering, and agent-friendly CLI — all in one binary. The tradeoff is that it's a bigger thing to understand than ttyd, and a less flexible thing to customize than xterm.js. For the use case it targets — browser-accessible terminals with AI agent support and multiple concurrent sessions — the tradeoffs are worth it.

## The reconnect problem, solved

This is worth dwelling on because it's the issue that started everything for us.

With xterm.js, when a user reconnects — browser refresh, network blip, new tab — the server has to replay the raw PTY byte history so the browser can reconstruct terminal state by re-parsing every byte from scratch. A build that's been running for twenty minutes means twenty minutes of bytes to replay, or you truncate and the user loses context. Either way, you're choosing between latency and correctness.

blit doesn't have this tradeoff. Because the server holds the parsed terminal state, a reconnecting client gets a single compressed snapshot: here's what's on screen right now. Session age is irrelevant. A terminal that's been running for six hours reconnects in the same time as one that started five seconds ago.

This also means consistency is structural, not aspirational. When the browser runs its own VT parser, it can diverge from the true terminal state — Unicode width edge cases, incomplete escape sequences, timing-dependent mode changes. With blit, the server is the single source of truth and the browser just applies patches. If anything drifts, the next delta snaps it back.

## Try it

No install needed:

```bash
docker run --rm grab/blit-demo
```

This starts a sandboxed container with `blit share`, fish, neovim, htop, and the usual suspects. It prints a URL. Open it — you're in a terminal. Open it in a second tab — both update independently, paced to their own render speed. That's per-client congestion control in action.

Or install it:

```bash
curl -sf https://install.blit.sh | sh
blit open                              # local browser terminal
blit share                             # share over WebRTC — prints a URL
blit open --ssh myhost                 # remote terminal in your browser
blit start htop && blit show 1         # start a session, read what's on screen
```

Also available via [Homebrew](https://github.com/indent-com/homebrew-tap), [APT](https://install.blit.sh), and [Nix](https://github.com/indent-com/blit#nix). The code is at [github.com/indent-com/blit](https://github.com/indent-com/blit).
