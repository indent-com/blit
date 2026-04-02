# We rebuilt our terminal stack. Here's why.

Indent is a coding agent. It runs in a browser. It needs terminals — plural, persistent, sometimes watched by a human, sometimes driven by an AI, sometimes both at once.

We used `xterm.js` for a long time. It worked. Then it didn't.

This is the story of what broke, what we tried, what we built, and what we learned. The project is called `blit`. It is open source and it stands on a lot of existing software — `alacritty_terminal` for VT parsing, WebGL for rendering, LZ4 for compression, WebRTC for sharing. The new part is the layer that ties them together: a server that owns terminal state and streams diffs to each client at the rate it can absorb them.

## The symptom

The first thing that went wrong was reconnects.

A user refreshes a browser tab. An agent session loses its WebSocket. A laptop wakes from sleep. In every case, the browser needs to show what the terminal looks like *right now*.

With our xterm.js setup, "right now" was expensive. The server had raw PTY byte history. The browser had a VT parser. To reconstruct the present, the client had to replay the past — every byte of output the terminal had ever produced, parsed from scratch, to arrive at the current screen.

A quiet shell? Fine. A build that had been running for twenty minutes? The user would watch a blank screen, then a blur of replayed output, then finally the terminal they wanted. Or we'd truncate the history and they'd get a terminal with missing context.

We tried the obvious mitigations. Capping replay length. Snapshotting periodically. Sending the last N bytes. Every fix was a tradeoff between latency and correctness, and none of them made the problem go away. They just made it smaller.

## The diagnosis

The reconnect problem pointed at something deeper.

A terminal is not a byte stream. It is a state machine. It has a cell grid, a cursor, scrollback, colors, styles, alternate screen mode, mouse mode, wide character state, and dozens of other properties that only exist after parsing the escape sequence stream.

In the xterm.js model, that state lives in the browser. The server is a byte pipe. Every client independently derives truth from history.

That works fine when there's one client, it never disconnects, and the session is short-lived. It works less well when:

- Sessions are long-lived (agent workflows that run for hours)
- Multiple clients attach to the same session (human watching an agent work)
- Clients disconnect and reconnect frequently (browser tabs, network blips)
- Background sessions need to remain observable without replaying from zero

We were hitting all four. The architecture was fighting the product.

## The wrong turns

We didn't jump straight to "build a whole new terminal stack." We tried smaller things first.

We tried server-side replay acceleration — pre-parsing history on the server and sending a compact representation. It helped with startup time but didn't solve the flow control problem. We still had a growing queue of stale output for slow clients.

We looked at patching xterm.js to support state snapshots. Possible in theory, but xterm.js (reasonably) treats its internal state as private. Serializing and deserializing the full emulator state was fragile and fought the library's design.

We considered a hybrid: keep xterm.js for rendering, but add a server-side emulator that could produce snapshots. This is closer to where we ended up, but it still meant maintaining two VT parsers that had to agree on every edge case. Unicode width tables, incomplete escape sequences, mode interactions — any divergence between the two parsers meant the snapshot wouldn't match what the browser would have computed on its own.

At some point we realized we were building an increasingly elaborate support structure around a model that didn't fit. The server needed to own the state. The client needed to receive diffs. The byte stream needed to stop being the unit of client communication.

## The core idea

`blit` is built around one structural decision: the server parses PTY output and maintains the full terminal state. Clients don't replay history. They receive diffs against what they last acknowledged.

That's it. Everything else follows from this.

The server runs `alacritty_terminal` as its VT parser. It holds the cell grid, scrollback, cursor, modes — all of it. When a client connects or reconnects, the server computes the difference between "what is true now" and "what this client last confirmed seeing," encodes the delta, compresses it, and sends it.

A reconnecting client gets a snapshot of *the present*. Not twenty minutes of history. Not a truncated suffix. The actual current state, delivered as a compressed diff against an empty baseline.

Session age becomes mostly irrelevant to attach time.

## The second problem we didn't expect

Reconnects were what started the project. But once the server owned terminal state, a second problem became visible — one we'd been living with without fully naming it.

If a client can't consume output as fast as the PTY produces it, something has to give.

In the old model, the answer was "buffers grow." PTY kernel buffers. Server-side send queues. Socket buffers. Browser receive buffers. The client parser and render queue. Somewhere in that chain, stale output accumulated, and the terminal drifted further and further from the present.

We'd seen this in production. A build spraying logs. An agent session with verbose output. A user with a slow connection or a throttled background tab. The terminal was technically connected but practically showing the past. Scroll to the bottom and you'd see output from thirty seconds ago, still catching up.

The worse version: if those buffers fill up all the way, backpressure propagates upstream. The PTY write path blocks. A display problem becomes an execution problem — the build actually slows down because the terminal can't drain fast enough.

Once we understood this, the design requirements got clearer. It wasn't enough to send diffs. The server needed to *pace* each client independently, tracking what each one could actually absorb.

## Flow control for humans

This is where the project got more interesting than we expected.

`blit` keeps per-client congestion state. It tracks RTT from acknowledgements. It estimates delivered bandwidth and goodput. It caps in-flight data using the bandwidth-delay product. And it incorporates client-side signals: display refresh rate, backlog depth, frame apply time, whether the session is focused or backgrounded.

A fast focused terminal runs hot. A slower client gets updates at the rate it can absorb. A background session gets preview-rate updates so it doesn't steal budget from the terminal you're actually looking at.

This matters because different workloads want different things from the same system:

- A shell wants keystroke-to-prompt latency to feel instant
- A build wants to avoid becoming a museum of stale logs
- A full-screen TUI wants smooth rendering in the focused pane
- A background agent session needs to stay observable without demanding full frame rate

The system that ignores these differences treats every client and every session equally, which means it treats all of them worse than it could.

## What this looks like in practice

From the outside, `blit` is a CLI:

```bash
curl -sf https://install.blit.sh | sh
blit open                              # local browser terminal
blit share                             # share over WebRTC
blit open --ssh myhost                 # remote terminal in your browser
```

From the inside, it's a full stack. `blit-server` owns PTYs and state. `blit-gateway` handles browser authentication and proxies frames (statelessly — it can restart without losing sessions). A WASM module in the browser decompresses and applies diffs. WebGL renders the cell grid.

The wire protocol is transport-agnostic. The same binary framing runs over Unix sockets, WebSocket, WebTransport, WebRTC DataChannels, or SSH. That's not a flexibility-for-its-own-sake decision — it's what lets the same terminal session be local, remote, browser-facing, and shareable without changing the core model.

## Terminals as an API, not just a UI

This is the part that matters most for Indent, and the part I think gets underappreciated in "browser terminal" projects generally.

An AI agent doesn't want to manage a terminal emulator session. It wants to:

```bash
ID=$(blit start --cols 200 -t build make -j8)
blit wait "$ID" --timeout 120 --pattern 'BUILD OK'
blit history "$ID" --from-end 40 --limit 40
blit send "$ID" "\x03"
```

Start a process. Wait for a condition. Read what's on screen. Send input. Each subcommand connects, does one thing, and exits. Output is plain text. Exit codes mean something.

This is the same underlying capability that makes reconnects fast and flow control work — the server knows what's on screen — but exposed as a programmatic interface instead of a visual one.

For Indent, that means the human and the agent consume the terminal through the same abstraction. The human gets a rendered surface. The agent gets `blit show`. Both are reading from the same server-side truth. Neither is replaying a byte stream.

That turned out to matter more than we expected. When the terminal is infrastructure that both humans and agents can attach to — rather than a UI widget that agents have to screen-scrape — the product design space opens up. An agent can run a build in the background, a human can attach to watch it, the agent can detach and come back later to check the result, and none of those transitions involve replaying history or rebuilding state.

## What we'd do differently

The scope expanded beyond what we originally planned. We set out to fix reconnects and ended up building a terminal streaming stack with its own wire protocol, flow control system, WebRTC sharing, and embedding libraries.

Some of that expansion was necessary — flow control and reconnects are genuinely the same problem once you look at them clearly. Some of it was probably premature. The WebRTC sharing story, the multiple transport backends, the full embedding API — these are useful, but we could have shipped value to Indent users faster with a narrower first cut.

The VT parser choice was right. Using `alacritty_terminal` rather than writing our own meant we got a battle-tested parser that handles the long tail of terminal edge cases. The tradeoff is that we're coupled to its internal representation, and it wasn't designed to be used as a library in this way. That coupling has cost us time in a few places.

Building the WASM diff-application layer and the WebGL renderer was a bigger investment than expected. It works well, but "apply binary diffs in WASM and render with WebGL" is a lot of machinery compared to "let xterm.js handle it." The payoff is real — no VT parsing in the browser, no parser divergence — but the upfront cost was significant.

## How it compares

Every tool in this space made a reasonable set of tradeoffs.

`xterm.js` is the standard browser terminal library. It's extremely flexible because it's a library, not a product stack. The tradeoff is that state ownership, transport, and flow control are your problem.

`ttyd` and `gotty` are simple and direct — pipe a PTY to a browser. They don't try to solve the state ownership or flow control problems, which keeps them small and easy to deploy.

`Mosh` understood state diffs early and its predictive echo is genuinely clever. No browser story, though.

`Eternal Terminal` solves persistence over flaky links. Again, no browser path.

`blit` is unusual because it's opinionated about the whole stack at once: server-side parsing, binary diffs, per-client pacing, multiple transports, browser rendering, and agent-friendly CLI semantics. The tradeoff is that it's a bigger thing to understand and operate than the simpler browser-terminal servers, and a less blank canvas than xterm.js.

For our use case — long-lived, multi-client, agent-driven terminal sessions in a browser-based product — that tradeoff was worth it.

## Try it

No install needed:

```bash
docker run --rm grab/blit-demo
```

Or install it:

```bash
curl -sf https://install.blit.sh | sh
blit open
blit share
blit open --ssh myhost
```

Open a shared session in two tabs. Watch them update independently. That's per-client pacing made visible.

The code is at [github.com/indent-com/blit](https://github.com/indent-com/blit). Also available via [Homebrew](https://github.com/indent-com/homebrew-tap), [APT](https://install.blit.sh), and [Nix](https://github.com/indent-com/blit#nix).
