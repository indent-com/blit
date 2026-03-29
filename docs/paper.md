# blit: Terminal Streaming for High-Latency Networks

## Abstract

`blit` is a terminal streaming system designed for high-latency networks. It moves terminal parsing to the server, sends compressed frame diffs to clients, and renders in the browser via WebGL. This document describes what it does, how it works, and the tradeoffs involved.

## 1. What Terminals Have to Do

### 1.1 Protocol interpretation

The byte stream from a PTY is a control protocol — escape sequences, mode changes, title updates, cursor shape, bracketed paste, mouse tracking, alternate screens. A terminal interprets these as state transitions.

`blit` does this on the server. `blit-server` feeds PTY output into a wezterm-backed parser that updates an in-memory screen model. Mode tracking covers application cursor, keypad, alternate screen, mouse mode/encoding, cursor visibility/style, bracketed paste, and synchronized output (`DEC ?2026`).

### 1.2 Screen state

Each cell carries foreground/background color, bold, dim, italic, underline, inverse, wide-character state, and up to 4 bytes of UTF-8 content. Longer content goes to a side table keyed by an FNV-1a hash for correct diffs. A frame also includes cursor position, packed mode bits, title, overflow strings, and per-row line-wrap flags.

### 1.3 Unicode

Wide characters, emoji, variation selectors, and multi-code-point grapheme clusters break the "one character per cell" assumption. `blit` marks wide cells and continuations during frame encoding, preserves wide-cell state in the protocol, and renders normal glyphs through a WebGL atlas while routing overflow text through a 2D canvas.

### 1.4 Input encoding

The correct byte sequence for a keypress depends on terminal mode. Arrow keys, function keys, mouse reports, and paste all have mode-dependent encodings. The keyboard encoder translates browser events into terminal bytes with mode awareness. Mouse events are forwarded when the application has enabled mouse mode; otherwise the wheel controls scrollback.

### 1.5 Resize, scrollback, alternate screens

The server computes the minimum bounding size across all subscribed clients and resizes the PTY accordingly. The driver can produce live snapshots or scrollback frames at arbitrary offsets. Each client tracks its own scroll position independently. Per-row wrap flags come from the parser, not heuristics.

### 1.6 Rendering

In the browser, WASM builds ready-to-upload vertex buffers in a single pass over the cell grid. Background rectangles are coalesced; glyph vertices are emitted directly during iteration (no intermediate buffer). The glyph atlas uses FxHashMap and caches its canvas size to avoid DOM access per glyph. JS creates zero-copy Float32Array views into WASM memory and uploads them via bufferData. Rendering is demand-driven — requestAnimationFrame is only scheduled when something changed, and cursor-only repaints reuse cached vertex buffers. The WebGL canvas clamps to the GPU's MAX_RENDERBUFFER_SIZE and handles context loss.

In console mode, the CLI renders the replicated screen as ANSI sequences with attribute tracking and synchronized-output markers.

### 1.7 Selection and search

Selection supports character, word, and logical-line granularity. Word boundaries use `A-Za-z0-9_-./~:@`. Triple-click spans wrapped rows using protocol-level wrap flags. During selection, the terminal freezes — frames are buffered and ACKed but not applied, so text can't shift under the cursor. Copied content includes both plain text and HTML. URLs are detected on hover and opened via Alt-click. Search covers titles, visible content, and scrollback with weighted ranking.

## 2. What's Different About blit

### 2.1 Server-owned terminal state

The server owns PTYs and maintains authoritative state. Clients receive compressed frame deltas, not raw PTY bytes. This is the core architectural choice — everything else follows from it.

### 2.2 Scroll-aware frame protocol

The update protocol has three operation types: COPY_RECT (for detected vertical scroll), FILL_RECT (for cleared regions), and PATCH_CELLS (bitmask-driven dirty cells). Frames are LZ4-compressed. Overflow strings, wrap flags, and title changes have presence bits to avoid overhead when unchanged.

### 2.3 Application-level pacing

The server tracks per-client RTT, in-flight bytes, goodput, jitter, frame sizes, and the client's reported display refresh rate. The client ACKs each frame immediately. When the link can't keep up, intermediate frames are dropped — each delivered frame is still a correct diff from the client's last-acknowledged state.

### 2.4 Lead vs preview scheduling

One PTY is the lead; others appear as live previews in Expose. The server paces them independently — previews get lower cadence to protect lead interactivity. The Expose view sorts by most-recently-used and defaults selection to the previous PTY.

### 2.5 Browser client

The browser client is not an afterthought. It includes a WASM state machine (same Rust crate as the server), a WebGL renderer with zero-copy vertex upload, IME-aware input, mouse protocol forwarding, 13 built-in palettes, configurable font family and size, and selection with HTML copy. DPR changes (browser zoom, monitor switching) trigger re-measurement and atlas invalidation.

### 2.6 Predictive echo

When the PTY is in echo mode and the user types printable characters, the client draws a translucent prediction at the cursor position. The prediction is reconciled as authoritative frames arrive — matching characters are consumed, mismatches discard the buffer.

### 2.7 Cross-PTY search

Search spans titles, visible content, and scrollback across all PTYs, with weighted scoring and scroll offsets for jump-to-result. Typing `>command` in Expose creates a new PTY.

### 2.8 Exited terminal retention

When a subprocess exits, the server sends `S2C_EXITED` and keeps the terminal state. Clients can still scroll and search it. New clients connecting later see the dead terminal in the session list. Only explicit close (`C2S_CLOSE`) dismisses it.

### 2.9 Deployment flexibility

The server and gateway are separate processes — PTY lifetime is decoupled from HTTP serving. The CLI can embed a gateway, optionally on a fixed port. SSH mode forwards a Unix socket for multiplexed browser tabs. The focused PTY ID goes in the URL hash for reload persistence.

At the library level, `blit-react` provides a context provider, transport-agnostic components, explicit WASM module injection, pure-function protocol builders, and separately exported renderer and keyboard encoder.

### 2.10 Callback rendering

`blit-remote` includes a callback renderer that lets programs describe text surfaces (fills, wrapped text, scrolling regions) and transmit them through the same frame diff and transport machinery as PTY-backed terminals.

## 3. How It Fits Together

The design is driven by one goal: usable terminals over high-latency links. That requires server-side parsing, compact diffs, per-client baselines, explicit pacing, and a browser renderer that doesn't depend on native code. Once those pieces exist, features like predictive echo, live previews, cross-PTY search, and terminal retention are straightforward additions rather than bolted-on extras.

## 4. Limitations and Future Work

- The cell iteration in `prepare_render_ops` scales linearly with terminal size. At 4K with small fonts (~75K cells), this dominates frame time. Dirty-row tracking or incremental vertex updates could help.
- The glyph atlas is rebuilt from scratch on font or DPR changes. Incremental atlas updates would reduce the cost of zooming.
- The `S2C_EXITED` state is not yet part of the LIST wire format — it's sent as a separate message, which required a server-side fix for channel backpressure on reconnect.
- Console mode does not benefit from most of the browser rendering optimizations.
- The two-canvas compositing (WebGL + 2D overlay) means the browser composites them as separate layers. A single-canvas approach would eliminate potential inter-layer timing issues.
