# Why We Built blit: terminal state, not terminal replay

*About a 10 minute read.*

Here is the whole argument in four bullets.

- Indent previously used `xterm.js`.
- In our setup, attaching to a live terminal meant replaying terminal bytes and history to reconstruct current state.
- That hurt startup time, but it also created a second problem: if a client could not keep up, stale output piled into buffers, latency got worse, and eventually the display path could start pushing back on execution itself.
- `blit` fixes both problems by keeping parsed PTY state on the server and sending each client the current state plus incremental diffs at the rate that client can actually absorb.

That is why `blit` is not just “a nicer browser terminal.” It is a different terminal architecture.

## The bug was architectural

Imagine the worst possible moment to refresh a browser tab.

A build has been streaming output for twenty minutes. Another terminal is running interactively. Logs are noisy. You reconnect. And instead of attaching to the terminal as it exists *now*, the browser has to earn the present by chewing through the past.

That was the problem for us.

A remote terminal is not just a byte pipe with text rendering bolted onto the end. A terminal is a state machine. It has a grid of cells, scrollback, cursor position, cursor mode, alternate screen state, mouse modes, colors, styles, wide character behavior, title changes, and a pile of escape-sequence-driven semantics that only exist after parsing the stream.

If you store only bytes, every new client has to re-derive that state. If you store state, new clients can attach to something current.

That distinction stops being abstract the moment the terminal is long-lived, shared, remote, multi-session, or agent-driven. Then the past stops being just history. It becomes part of your startup path.

That was the failure mode for us with `xterm.js`. The issue was not that `xterm.js` is bad. It is the standard for a reason. The issue was that, in our setup, reconstructing current terminal state by replaying terminal bytes and history was work we kept making fresh clients pay for.

> The past should not be your startup path.

That sentence is the shortest explanation of why `blit` exists.

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

From the inside, it is a full terminal streaming stack.

At the center is `blit-server`, which owns PTYs, scrollback, parsed terminal state, and per-client frame pacing. Around it sit:

- `blit-gateway` for browser access over WebSocket and WebTransport
- `blit-webrtc-forwarder` for peer-to-peer sharing over WebRTC DataChannels
- a CLI that can speak Unix sockets, TCP, SSH, and WebRTC
- a browser runtime that applies frame diffs in WASM and renders with WebGL
- React and Solid bindings for embedding the terminal surface in your own UI

The whole system is built around one idea: **the server should understand what is on screen, not just shuttle bytes**.

Traditional browser terminals usually look like this: the backend reads raw bytes from a PTY and forwards them; the browser runs a terminal emulator, parses VT escape sequences, and reconstructs the state for itself. The server is mostly a byte pipe.

`blit` inverts that model. The server parses PTY output through `alacritty_terminal` and maintains the full parsed terminal state: cell grid, styles, cursor, scrollback, title, modes, the lot. When it is time to update a client, the server does not dump the raw byte stream. It diffs the current terminal state against what that specific client last acknowledged, encodes only the delta, LZ4-compresses it, and sends the result.

That changes both startup cost and steady-state behavior.

## How a frame moves

The PTY-to-pixels path looks like this:

![Frame pipeline](../claude/frame-pipeline.png)

1. **PTY emits raw bytes** — program output, escape sequences, the works.
2. **`alacritty_terminal` parses** — the VT state machine converts bytes into a structured cell grid.
3. **Server diffs against last ACK** — for each connected client, the server compares the current grid to what that client last confirmed it received.
4. **Encode + LZ4 compress** — the diff is encoded as `COPY_RECT`, `FILL_RECT`, and `PATCH_CELLS`, then compressed.
5. **Gateway proxies** — the stateless gateway handles browser auth and forwards binary frames.
6. **WASM decompresses and applies** — a Rust WASM module patches the browser's local cell grid. No VT parsing in browser JavaScript.
7. **WebGL renders** — shader programs draw backgrounds, glyph quads, and cursor state from zero-copy vertex buffers exposed out of WASM memory.
8. **Browser ACKs** — acknowledgements retire in-flight frames and feed RTT estimation.

Each cell is encoded in a compact fixed-width layout with overflow handling for larger Unicode content. The wire format is not protobuf, not JSON, and not an external schema layered over the terminal. It is a hand-rolled binary protocol because the system wants to ship terminal state efficiently, not narrate it politely.

The important detail is not just compression. It is *what* gets compressed. The unit of transmission is not “whatever bytes the PTY just emitted.” It is “what changed in the terminal state for this particular client.”

That is the difference between replaying history and attaching to truth.

## Startup is only half the story

Replay-heavy terminal architectures fail in two different ways.

The first failure is obvious: attach time gets worse because every new client has to reconstruct state from history.

The second failure is nastier: if a client cannot consume terminal output as fast as the PTY produces it, the system starts building a queue of stale output.

That queue lives in real places:

- PTY and kernel-adjacent buffers
- server-side send queues
- socket and TCP buffers
- browser-side receive buffers
- client-side parse and render work queues

If the producer is faster than the consumer, one of two things happens:

- If those buffers are allowed to grow, latency grows with them.
- If those buffers are bounded, you eventually hit backpressure.

And backpressure here is not just “the UI is a little behind.” It can become “the terminal session itself is being affected.”

Once the downstream path stops draining quickly enough, writes stop being free. If the PTY-side path fills up, a chatty process can block on terminal output. That means a display problem has turned into an execution problem.

This is the part that matters for real workloads:

- long-running builds
- test suites with noisy logs
- compilers and linters that dump huge bursts of output
- continuously repainting TUIs
- multiple viewers attached to the same session

In a raw byte-stream model, a slow consumer can become a giant queue for stale history. That is bad enough for freshness. It is worse when it starts feeding back into the process that is still running.

## Adaptive flow control is how you stay in the present

This is where `blit` stops looking like “a terminal renderer” and starts looking like a real transport system.

The goal is not maximum throughput in the abstract. The goal is minimum useful latency: keep each client as close to the present as its network, device, and renderer can sustain, without letting stale output turn into an ever-growing queue.

That is why `blit` keeps separate congestion state for each client.

It tracks things like:

- RTT from ACKs
- delivered bandwidth and goodput
- in-flight byte and frame windows bounded by the bandwidth-delay product
- client display rate
- backlog depth
- frame apply time
- focused versus background session priority

Then it paces each viewer accordingly.

A fast focused terminal can run hot. A slower client gets updates at the rate it can actually absorb. Background sessions are throttled into previews so they do not steal budget from the terminal the user is actively looking at.

That matters because different workloads want different things.

A quiet shell wants instant keystroke-to-prompt response.
A noisy build wants to avoid falling minutes behind.
A full-screen TUI wants smoothness in the focused view without forcing background sessions to consume the same budget.
A slow laptop or throttled browser tab needs the system to recognize that rendering is part of the bottleneck, not just the network.

This is a much better control loop than “ship bytes as fast as possible and hope the client keeps up.”

The user-facing version is simple: a fast client on localhost gets frames as fast as it can render them; a thinner client gets paced to what it can handle; neither secretly becomes the global bottleneck.

## Five transports, one protocol

The wire protocol is defined in terms of reliable ordered byte streams, not in terms of one favored network API.

| Transport | Use case |
|---|---|
| **Unix socket** | Primary. Server ↔ gateway, server ↔ CLI. |
| **WebSocket** | Browser connections through the gateway. |
| **WebTransport** | QUIC/HTTP3 for lower-latency browser connections. Self-signed certs with hash pinning. |
| **WebRTC DataChannel** | Peer-to-peer. `blit share` prints a URL; anyone with the passphrase can connect. |
| **SSH** | `blit open --ssh myhost` tunnels the protocol over SSH. |

That transport flexibility matters because it means the rest of the system can stay stable while the access shape changes.

On the Rust side, the transport becomes `AsyncRead` + `AsyncWrite`. On the TypeScript side, anything implementing the `BlitTransport` interface can plug into a workspace. The transport is a layer, not the architecture.

One wording point worth getting exactly right: WebRTC signaling is signed, and it is encrypted in transit by `wss://`, but it is not application-layer encrypted end-to-end. The hub can verify and read signaling payloads. The terminal stream itself then runs over the encrypted WebRTC DataChannel.

## Agent subcommands: terminals as an API

This is one of the most distinctive parts of `blit`, and one of the biggest reasons it fits Indent well.

```bash
ID=$(blit start --cols 200 -t build make -j8)
blit wait "$ID" --timeout 120 --pattern 'BUILD OK'
blit history "$ID" --from-end 40 --limit 40
blit send "$ID" "\x03"
blit restart "$ID"
blit close "$ID"
```

Every subcommand opens a connection, does one thing, and exits. Output is plain text: TSV for `list`, raw terminal text for `show` and `history`. Exit codes are meaningful. Errors go to stderr. There is no custom SDK, no long-lived browser client to manage, and no pretense that screen scraping is an API.

`show` gives you the current viewport — what a human would see right now. `history` gives you scrollback with pagination. `send` pushes keystrokes with C-style escapes. `wait` blocks until the process exits or a regex matches new output.

For AI agents, this is huge. An agent does not necessarily want to speak terminal protocol directly. It wants to start a process, read the latest output, wait for a condition, send some input, and move on. `blit` turns the terminal into a first-class control surface instead of leaving it as a visual side effect.

That is a different ambition than “terminal in browser.” It is closer to “terminal as infrastructure.”

## More than a renderer: the full feature set

One reason `blit` is compelling is that it is not a point solution. It covers the terminal lifecycle end to end.

- **Local browser terminal** via `blit open`, with gateway-backed browser access over WebSocket or WebTransport.
- **Shareable sessions** via `blit share`, using WebRTC DataChannels, signed signaling, and STUN/TURN-assisted NAT traversal.
- **Remote access over SSH** without changing the user model.
- **Multiple PTYs as first-class objects**, with focus, subscriptions, visibility, and independent resizing.
- **Search, readback, and precise copy behavior**, including scrollback access and copy-range semantics.
- **Embedding** through `@blit-sh/core`, `@blit-sh/react`, and `@blit-sh/solid`.
- **A reference web app** with layouts, overlays, palette and font controls, disconnected-state UX, and debug views.
- **Operational hooks** like systemd socket activation, Homebrew services, Debian service templates, Nix modules, and `fd-channel` embedding.
- **Packaging that respects reality**, including curl install, Homebrew, APT, Nix, Windows, and static Linux binaries via musl.

There are also quieter features that matter once a system graduates from demo to infrastructure: predicted echo informed by PTY mode bits, server font serving to the browser, careful Unicode handling, and an explicit audit document for the non-obvious unsafe code in the project.

That last detail is worth calling out. A repo with an `UNSAFE.md` that documents invariants is the kind of repo that expects the code to matter operationally.

## Diagram and interactive ideas

These are ideas for the post package, not implemented features.

### Diagram ideas

1. **Replay versus attach timeline**
Show two horizontal timelines side by side. The old model spends time on “fetch history,” “replay bytes,” and “reconstruct state” before the user reaches “interactive.” The `blit` side goes almost straight from “connect” to “current snapshot” to “interactive.”

2. **Where backlog lives**
Draw a waterfall of buffers: PTY, server queue, socket buffers, browser receive buffer, parser/render queue. Show what happens when a client falls behind and why that can eventually create backpressure on execution.

3. **Per-client pacing control loop**
A closed loop diagram showing frame send, ACK, RTT estimate, client metrics, pacing decision, next frame budget. This would make the adaptive flow-control story concrete.

4. **Focused versus background bandwidth budget**
One large highlighted pane and several dimmed background panes, with arrows showing most budget going to the focused session and previews going to the rest.

5. **Workload matrix**
A matrix comparing quiet shell, noisy build, full-screen TUI, reconnect, and multi-viewer scenarios. Each cell explains what “best experience” means for that workload and how `blit` approaches it.

6. **Transport and trust boundaries**
A diagram separating Unix socket, gateway, WebRTC signaling hub, and DataChannel. This is the cleanest place to explain signed signaling, TLS to the hub, and encrypted DataChannel traffic without muddy wording.

7. **Stateful server, stateless edges**
A cleaner editorial version of the architecture diagram that emphasizes where truth lives and why restarts at the edge do not destroy PTY state.

### Interactive ideas

1. **Session age slider**
A small demo where the user drags a slider from “5 seconds” to “6 hours” and sees the old replay model grow more expensive while the `blit` attach model stays nearly flat.

2. **Slow-client backlog simulator**
A toy animation where one client consumes output slower than production. The user can watch queue depth rise in the replay model and watch `blit` converge toward current state instead of accumulating a giant stale buffer.

3. **Frame diff explorer**
A side-by-side terminal grid where users can scrub through one update and see `COPY_RECT`, `FILL_RECT`, and `PATCH_CELLS` highlighted visually.

4. **Focused-session budget toggle**
A multi-pane demo where the reader switches focus between panes and sees the frame-rate or bandwidth budget move with it.

5. **Workload selector**
Buttons for “shell,” “build,” “htop,” and “reconnect.” Each mode changes the explanatory copy and highlights why low latency means something different in each case.

6. **Transport selector**
A simple UI that lets the reader click Unix socket, WebSocket, WebTransport, WebRTC, or SSH and see what changes and what stays constant in the stack.

7. **Signaling trust explainer**
A small interactive card explaining: signed, encrypted in transit to hub, not application-layer encrypted, encrypted DataChannel after connect. This would prevent readers from over-reading “Ed25519-signed signaling.”

8. **Input-latency meter**
A lightweight animation showing the difference between “rendering old output that no longer matters” versus “staying near the present.” Not as a benchmark claim, but as an intuition builder.

## Architecture: stateful server, stateless edges

![System architecture](../claude/architecture.png)

The design falls out of one decision: **the server owns the state**.

`blit-server` hosts PTYs, scrollback, parsed terminal state, and per-client pacing. `blit-gateway` is stateless: it authenticates browser clients and proxies binary messages. That means PTYs survive gateway restarts, the gateway can sit behind a reverse proxy, and the CLI can embed a temporary gateway when it wants browser access without requiring a permanent deployment.

For embedding in another service, `fd-channel` mode lets an external process pass pre-connected client file descriptors into `blit-server` via `SCM_RIGHTS`. Your service can own auth and connection acceptance; `blit` owns the terminal machinery.

On the frontend, the framework bindings are intentionally thin:

```tsx
import { BlitTerminal, BlitWorkspaceProvider } from "@blit-sh/react";
import { BlitWorkspace } from "@blit-sh/core";

<BlitWorkspaceProvider workspace={workspace}>
  <BlitTerminal sessionId={session.id} style={{ width: "100%", height: "100vh" }} />
</BlitWorkspaceProvider>
```

The workspace manages connections, sessions, focus, visibility, and lifecycle. The terminal surface renders a session by ID. That is a clean API boundary for embedding a real remote terminal rather than a toy widget.

## Why this is the right answer for Indent

For Indent, `blit` is a better primitive than replaying terminal history into browser state.

Because the terminal in Indent is not serving one audience.

It has to work for:

- a human user who wants fast attach, low input latency, readable output, resizing, and selection
- a browser UI that needs current state instead of a byte log it must reconstruct from scratch
- an agent that benefits from stateless terminal operations like `start`, `show`, `history`, `send`, and `wait`

That is already enough to eliminate a lot of simpler designs.

It also matters that Indent explicitly distinguishes interactive and background terminal sessions. `blit`'s focus and visibility model fits that reality well. Focused sessions can get the budget. Background sessions can stay alive and observable without being treated as equal-priority rendering work.

And there is one more important point: `blit` is transport-agnostic all the way up the stack. In the current Indent client, the integration uses a WebRTC data channel transport. That is not a bolt-on curiosity. It is a sign that the underlying abstraction is right. The same terminal system can be local, remote, browser-facing, shareable, and agent-driven without changing its core mental model.

That is what makes `blit` feel like infrastructure rather than a widget.

## How it compares

![Feature comparison](../claude/comparison.png)

Every tool in this space made a reasonable set of tradeoffs. `ttyd` and `gotty` are simple and direct. `Mosh` understood state diffs early, but has no browser story. `Eternal Terminal` solves persistence over flaky links, again without a browser story. `xterm.js` is extremely flexible precisely because it is a library, not a full product stack.

`blit` is unusual because it is opinionated about the whole stack at once: server-side parsing, binary diffs, per-client pacing, multiple transports, browser rendering, and agent-friendly CLI semantics. The tradeoff is that it is a bigger thing to understand than the simplest browser-terminal servers, and a less blank canvas than `xterm.js`. For the use case it targets, that tradeoff is the point.

## The reconnect problem, solved

This is the issue that started everything for us, and it is still the cleanest proof that the architecture matters.

With `xterm.js`, when a user reconnects — browser refresh, network blip, new tab — the server has to replay raw PTY byte history so the browser can reconstruct terminal state by parsing every byte from scratch. A build that has been running for twenty minutes means twenty minutes of bytes to replay, or else truncation and lost context. Either way, you are choosing between latency and correctness.

`blit` removes that tradeoff. Because the server holds parsed terminal state, a reconnecting client gets a compressed snapshot of *what is true right now*. Session age is mostly irrelevant. A terminal that has been running for six hours reconnects on the basis of current state, not on the basis of all the bytes that led there.

This also makes consistency structural, not aspirational. When the browser runs its own VT parser, it can drift around Unicode width edge cases, incomplete escape sequences, or timing-dependent mode changes. In `blit`, the server is the single source of truth and the browser applies patches. If anything diverges, the next delta snaps it back.

## What almost everybody should learn here

Even if you never use `blit`, there is a useful lesson here: “terminal in the browser” is not one problem. It is terminal emulation, state ownership, and transport policy. If you solve only the first one, you can still end up with something that technically works but product-wise feels slow or fragile. `blit` is interesting because it starts with the second and third questions. Where should truth live? How should clients attach to it? Once that answer is solid, the rest of the stack gets cleaner.

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
