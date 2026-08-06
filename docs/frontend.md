# Frontend

The browser-side of blit consists of a Rust WASM module (`blit-browser`) that applies frame diffs and produces GPU-ready vertex data, a GPU renderer (WebGPU with WebGL2 fallback), and a TypeScript layer (`@blit-sh/core`) that handles transports, workspace state, and input.

## Render pipeline overview

```mermaid
graph LR
    WS["WebSocket /\nWebTransport /\nWebRTC"] -->|compressed frame| WASM["blit-browser\n(WASM)"]
    WASM -->|vertex buffers\n(zero-copy)| GL["GPU renderer\n(WebGPU / WebGL2)"]
    GL -->|bg rects + glyphs| OC["offscreen canvas"]
    OC -->|drawImage| DC["display canvas"]
    DC -->|2D overlays| OUT["screen"]
```

## WASM runtime (`blit-browser`)

`blit-browser` compiles to `wasm32-unknown-unknown`. It maintains a `TerminalState` — the grid of 12-byte cells — and applies incoming compressed frame diffs via `feed_compressed()`.

When `prepare_render_ops()` is called for a render pass:

1. Iterates all cells in the grid.
2. Resolves foreground/background colors through the current palette (indexed colors, default colors, dim/bold/dim modifiers).
3. Coalesces adjacent cells with identical background color into merged rectangle operations.
4. For each cell with visible content, creates a `GlyphKey` (UTF-8 bytes + bold/italic/underline/wide flags), ensures the glyph exists in the atlas, and emits 6 vertices (2 triangles) with atlas texture coordinates.
5. Exposes vertex buffers to JavaScript via zero-copy WASM linear memory pointers (`bg_verts_ptr/len`, `glyph_verts_ptr/len`).

## Glyph atlas

The atlas is a **Canvas 2D `HTMLCanvasElement`**, not a GPU texture. It uses row-based bin packing to allocate glyph slots.

When a new glyph is needed:

1. A slot is allocated in the atlas canvas (power-of-two size, 2048–8192 px).
2. The Canvas 2D context sets font style (`"bold italic Npx family"`) and calls `fillText()` to render the codepoint in white.
3. Underlines are drawn with `ctx.stroke()` when the underline attribute is set.
4. The slot coordinates are cached in an `FxHashMap<GlyphKey, GlyphSlot>`.

The atlas canvas is uploaded to a WebGL texture once per frame (skipped if unchanged). The GL shader tints white glyphs with the per-vertex foreground color; color glyphs (emoji) pass through untinted.

## GPU renderer

The browser renderer has three backends, tried in order:

1. **WebGPU** — preferred when available (Chrome 113+, Edge 113+, Firefox Nightly). Async initialisation via `navigator.gpu.requestAdapter()`.
2. **WebGL2** — synchronous fallback, used while the WebGPU probe is in-flight or if WebGPU is unavailable.
3. **Canvas 2D** — software fallback when neither GPU API is available (e.g. headless environments).

All three implement the same `GlRenderer` interface and consume the same vertex buffers produced by the WASM module. `TerminalStore` kicks off the WebGPU probe eagerly in its constructor and transparently promotes the renderer once the probe resolves; frames rendered before that use the WebGL2 fallback.

### WebGPU renderer

Two WGSL render pipelines:

**RECT pipeline** — colored rectangles for cell backgrounds and the cursor.

- Vertex layout: `pos` (float32x2), `color` (float32x4) — 24-byte stride.
- Single draw call per frame (no batching needed; vertex buffer grows on demand).

**GLYPH pipeline** — textured atlas quads with per-vertex coloring.

- Vertex layout: `pos` (float32x2), `uv` (float32x2), `color` (float32x4) — 32-byte stride.
- Fragment shader uses the same gray-detection tinting as WebGL2 (grayscale → tinted, color → passthrough).
- Atlas uploaded via `copyExternalImageToTexture` with premultiplied alpha.

Both pipelines use premultiplied-alpha blending (`src: one, dst: one-minus-src-alpha`).

### WebGL2 renderer

Two shader programs handle all drawing:

**RECT shader** — colored rectangles for cell backgrounds and the cursor.

- Vertex attributes: `position` (vec2), `color` (vec4).
- Uses premultiplied alpha blending.

**GLYPH shader** — textured quads from the atlas.

- Vertex attributes: `position` (vec2), `uv` (vec2), `color` (vec4).
- Fragment shader: grayscale glyphs are tinted with the vertex color; color glyphs (emoji) render directly.

Both programs batch up to 65,532 vertices per draw call.

### Render loop (`BlitTerminalSurface`)

Demand-driven via `requestAnimationFrame`:

```mermaid
graph TD
    RAF["requestAnimationFrame"] --> PREP["WASM prepare_render_ops()"]
    PREP --> VIEW["Float32Array views over WASM memory\n(zero-copy)"]
    VIEW --> DRAW["GPU: bg rects + glyph quads + cursor\n→ offscreen canvas"]
    DRAW --> COMP["ctx.drawImage to display canvas"]
    COMP --> OVL["Canvas 2D overlays:\nselection · URL underlines · emoji · echo · scrollbar"]
```

## Input handling

### Keyboard

Input is captured via a hidden `<textarea>` element. `keyToBytes()` converts `KeyboardEvent` to terminal escape sequences:

| Key             | Sequence                                                          |
| --------------- | ----------------------------------------------------------------- |
| Ctrl+letter     | Control code (e.g. Ctrl+C → `0x03`)                               |
| Arrow keys      | `\x1b[A`–`\x1b[D` (normal) or `\x1bOA`–`\x1bOD` (app cursor mode) |
| Function keys   | `\x1b[15~`–`\x1b[24~`                                             |
| Modifier combos | `\x1b[1;{mod}X` format                                            |
| Alt+key         | `\x1b` prefix                                                     |

IME/composition input is handled via `compositionend` to capture multi-codepoint sequences as a single input event.

### Mouse

Mouse events are sent as `C2S_MOUSE` messages. The server generates the correct escape sequence based on the PTY's current mouse mode and encoding (X10, VT200, SGR, pixel). Client-side text selection (word/line granularity, drag) and clipboard copy are handled independently of terminal mouse mode — the browser intercepts the selection before it reaches the terminal emulator.

### Paste

Pasting into a Wayland surface is not a keystroke, it is a keystroke with a
prerequisite: the app reads the selection the instant it sees Ctrl+V, so
`BlitSurfaceCanvas` holds the V press back until the clipboard has been sent
as `C2S_CLIPBOARD_SET`, then releases press, V release and Ctrl release in
order. A 300 ms safety net gives up rather than delivering V with a stale
selection behind it.

Two reads race to supply the content, because neither is reliable alone:
`navigator.clipboard.readText()` (denied without permission in Chromium and
Brave) and the `paste` event (which browsers won't fire at a focused
non-editable canvas, hence the focus shuffle through the hidden textarea).
Only the `paste` event carries files, so **images arrive by that path only** —
`clipboardImage()` takes the best `image/*` item on the event and forwards its
bytes under their own MIME type, preferring PNG. Reading the blob is
asynchronous, so claiming the paste pushes the safety net out to 3 s and locks
out a `readText()` that resolves meanwhile.

An image only wins when the clipboard has no plain text: rich sources put
several representations on one clipboard, and the text is what pasting a
spreadsheet range is expected to produce. The wire carries one representation
per copy, so this is a choice, not a preference order the app gets to make.

An image over 8 MiB — or a blob that will not read — takes the safety net's
path rather than the flush's: warn, stand the chord down, no V. The frame
ceiling is 16 MiB and an over-length message is refused rather than truncated,
so the bytes are not going anywhere; pressing V anyway would paste whatever the
selection held _before_, which is not what was copied. An empty clipboard is
the one case that still presses V without sending, and deliberately: nothing
was withheld, so the selection the app reads is whichever Wayland client owns
it — copy in one surface, paste into another, browser never in the middle.

Every listener on the event's path (canvas, hidden textarea, and the
document-level capture listener that catches what the canvas misses) runs the
same handler, so the first to see an event marks it; without that a screenshot
would go out once per listener.

### Hyperlinks

Two sources feed one code path in `BlitTerminalSurface`:

- **OSC 8** — the application declared the target explicitly. `Terminal.link_at()` resolves the URI at a cell and `Terminal.link_segments()` returns the link's full extent as `[row, startCol, endCol]` triples, one per screen row. A link that runs past the right edge continues on the next row, so a wrapped link yields several triples and is underlined as one continuous span.
- **Regex fallback** — `https?://…` matched against the visible row text, for applications that emit no OSC 8. Single-row only; its target is its own text.

OSC 8 wins where both apply. Because it lets the target differ from the displayed text, every target is classified by `assessUrl()` (`js/core/src/urlSecurity.ts`) before it can be opened:

| Verdict   | Applies to                                                                         | Behaviour                                 |
| --------- | ---------------------------------------------------------------------------------- | ----------------------------------------- |
| `allow`   | `http`, `https`, `mailto` with nothing deceptive                                   | opens directly                            |
| `confirm` | custom schemes, local `file:`, embedded credentials, punycode/non-ASCII hosts      | prompts, showing the real target          |
| `deny`    | `javascript:`/`data:`/`blob:`/`view-source:`…, remote `file://`, hidden characters | refused; drawn dashed red, not underlined |

The hidden-character check runs _before_ the scheme check, since a leading control byte is exactly what slips a dangerous scheme past a check built on `new URL()`. Scheme extraction never uses `URL` for the same reason. `assessment.display` escapes invisible and text-reordering codepoints to `<U+XXXX>` — render that, never `assessment.raw`.

Embedders hook `surface.onLinkHover()` for a preview and `surface.setLinkActivateHandler()` to replace the default `window.confirm` with an in-app dialog; a custom handler receives the assessment and must honour its verdict.

### Predicted echo

When the PTY is in echo + canonical mode (mode bits 9 and 10), the browser shows typed characters immediately before the server confirms them. This makes typing feel instantaneous over high-latency connections. Predicted characters are displayed with a distinct style and replaced with server-confirmed output on receipt.

## Workspace and connection model

```mermaid
graph TD
    WS["BlitWorkspace"] --> C1["BlitConnection\n(rabbit)"]
    WS --> C2["BlitConnection\n(hound)"]
    C1 --> T1["WebSocketTransport\n/d/rabbit"]
    C2 --> T2["WebSocketTransport\n/d/hound"]
    C1 --> P1["Terminals\nrabbit:1, rabbit:2"]
    C2 --> P2["Terminals\nhound:1"]
```

`BlitWorkspace` manages one or more `BlitConnection` instances, each with its own transport and PTY namespace. Terminal IDs are prefixed by connection name (`"rabbit:1"`) to avoid collisions when multiple servers are open simultaneously.

## Surface video decoding

GUI app surfaces (see [server.md § Headless Wayland compositor](server.md#headless-wayland-compositor)) are decoded in the browser via the **WebCodecs `VideoDecoder` API**:

- Codec is detected per-frame from the `flags` byte in `S2C_SURFACE_FRAME`: bit 0 is the keyframe flag; bits 1–2 encode the codec — H.264 (0), AV1 (1), PNG (2).
- `optimizeForLatency: true` is set on the decoder to minimize decode delay.
- Decoded `VideoFrame`s are rendered to a canvas by `BlitSurfaceView` (React/Solid component).
- Mouse and keyboard events from the surface canvas are forwarded as `C2S_SURFACE_INPUT` / `C2S_SURFACE_POINTER` messages.

### Presentation scheduling

`SurfaceStore` does not draw a frame the moment it decodes. Each surface has a presenter that paints at vsync (`requestAnimationFrame`) in one of two modes:

- **Newest-wins** while the surface is idle or interactive: paint the freshest frame, close the rest. Minimum time-to-pixel, because a repaint there is a response to input and any hold reads as lag.
- **PTS-scheduled** once the surface has delivered `SMOOTHING_ENGAGE_FRAMES` (8) consecutive frames without a gap: each frame is painted on the refresh its capture-time PTS maps to, and frames not yet due stay queued.

The PTS is `S2C_SURFACE_FRAME.timestamp`, stamped at compositor-commit time — the only clock in the pipeline taken before encode and transport, so replaying against it cancels the jitter both add. Encode runs fire-and-forget off the server's tick loop, so per-frame encode latency varies; without scheduling that variance lands directly on screen as an uneven 2-0-1-2 cadence at a nominally perfect frame rate.

Presentation runs at the `PRESENT_QUANTILE` (p95) of the `arrival − pts` offsets seen over the last `OFFSET_WINDOW_MS` (1 s) of stream, capped at `PRESENT_DELAY_MAX_MS` (50 ms) above the `FAST_QUANTILE` (p2) of the same window — so a LAN link, where jitter is near zero, buffers almost nothing.

Both ends come from **one** distribution, which is what makes this robust in both directions without special-case rules. A burst frame — captured later but shipped immediately behind its predecessor, so genuinely faster in transit — is a low outlier; a frame delayed by a stall is a high outlier; a quantile ignores each for the same reason. An earlier design tracked the baseline as a running minimum with an upward leak and a clamped downward step, which needed two constants and still froze the surface for the length of any abrupt path improvement, because the baseline could only descend a few ms per frame while the true offset had already dropped.

A quantile rather than a peak-tracking average, because a peak tracker spends the entire latency budget on outliers it cannot cover anyway: one frame 200 ms late took the old estimator from 0 to 100 in a single sample, pinned the margin at the ceiling, and then decayed at 0.98/frame — about 55 frames, nearly a second at 60 Hz, of maximum latency bought by one event. The quantile sizes to the jitter that recurs and lets the tail fall through to skip-to-newest, which is the correct handling for an outlier regardless. The window is expressed in time, not frames, so the horizon is the same at 24 and 240 fps.

The presentation offset is slewed toward that target, not set to it (`MARGIN_GROW_MS` 2 ms/frame up; down by the larger of `MARGIN_SHRINK_MS` and `MARGIN_SHRINK_FRAC` of the remaining gap). Moving it _is_ a latency change — every future due time shifts with it — so stepping would inject exactly the discontinuity the scheduler exists to remove. Slewing turns it into a sub-perceptual rate nudge instead.

Shrinking is proportional rather than a flat crawl, which matters more than it looks. A flat 0.25 ms/frame took roughly five seconds to unwind a single stall; because video rides a reliable ordered channel, every lost packet _is_ such a stall, so a lossy link would have sat near the latency ceiling permanently — strictly worse than not scheduling at all, and precisely for the users this feature exists to serve. Proportional decay unwinds the same stall in about half a second.

A **PTS** gap over `STREAM_GAP_MS` (250 ms), a backwards PTS (the server's u32 ms counter wrapping), or the tab going hidden all reset the presenter to newest-wins. A frame without a finite PTS never engages scheduling.

The reset keys on capture time, never on arrival time, because the two mean opposite things. A source that went idle stops advancing PTS, and its next frame answers input — that one must paint immediately. A stalled transport kept producing all along; those frames arrive late in a burst with their PTS spacing intact, and scheduling is exactly what should handle them. Since surface video rides a reliable ordered channel, one lost packet head-of-line blocks for at least a round trip, so judging by arrival would disengage scheduling on every loss — permanently, on a high-latency link. PTS spacing survives head-of-line blocking, which makes this correct at any RTT without the client needing an RTT estimate.

The queue depth is derived, not fixed: the frames a margin legitimately spans is `margin / frame_interval`, and the frame interval is learned from PTS deltas rather than assumed. That matters because the server paces a surface at the client's display rate — up to `MAX_DISPLAY_FPS` (480) — so at 240 Hz a 50 ms margin holds 12 frames, not 3. Learning the interval from PTS also means the depth follows the rate the encoder _actually_ sustains, not the rate that was requested.

**Measured on loopback** (`blit surface record --timing`, mpv at 1280×720 into a local server, 471 frames): the capture clock is a clean grid — PTS deltas mean 16.69 ms, p95 19 ms, one 38 ms outlier — and delivery jitter is tiny, p95 − p2 of **2.5 ms**. That is below half a refresh, so on a local link the scheduler cannot hold a frame and is a no-op by construction. Its value is entirely on links with real jitter, which is also the only place it carries risk. Note the recorder ACKs immediately and never sends `C2S_DISPLAY_RATE`, so these numbers are capture + encode + transport jitter — exactly the input the margin absorbs — and say nothing about pacing under backlog.

**Limits.** Jitter beyond `PRESENT_DELAY_MAX_MS` is not absorbed: frames later than the margin are already overdue and paint immediately, so a link whose p95 offset spread exceeds 50 ms still judders on those frames — it degrades toward newest-wins rather than breaking. That ceiling is a flat millisecond count, which is the wrong shape in principle: 50 ms is a savage penalty against a 1 ms round trip and rounding error against a 1 s one. In practice it has not been observed to bind — the margin tracks measured jitter, and on the one link measured so far that is 2.5 ms. Making it relative would need a client-side RTT estimate, and there is none today: `C2S_PING` is a server-side no-op with no reply, so there is nothing to time. The margin is also real added latency, though it tracks measured jitter, so a distant-but-stable link pays almost nothing. A margin below half a refresh cannot hold a frame at all, since the nearest-vsync rounding already covers it. The refresh period itself is learned from rAF deltas between `RAF_DELTA_MIN_MS` and `RAF_DELTA_MAX_MS` (1000 Hz to 10 Hz); outside that band a tick is treated as a stall rather than a cadence. Depth is bounded, but sized so the bound never binds for a real stream: the margin tops out at 50 ms and frames arrive no faster than `MIN_FRAME_INTERVAL_MS` (480 fps, the server's own `MAX_DISPLAY_FPS` ceiling), so 24 frames plus slack covers every rate the pipeline can produce. A degenerate PTS stream is caught by flooring the _interval_, not by clipping depth — so no stream is ever made to drop frames merely for being fast. Depth also follows the rate the encoder actually sustains, since the interval is learned from PTS: a 4K surface encoding at 30 fps holds two frames regardless of the panel's refresh rate. A transport stall no longer disengages scheduling, but the backlog it releases is stale by the length of the stall, and the presenter skips to the newest due frame rather than replaying it — correct for a live desktop, though it reads as a jump rather than a smooth catch-up.

## Font serving

The gateway and CLI serve system fonts to the browser as `@font-face` CSS with base64-encoded font data via the `/font/<name>` route. `blit-fonts` discovers fonts by scanning standard directories (`~/Library/Fonts`, `/usr/share/fonts`, `/System/Library/Fonts`, etc.), falling back to `fc-list`/`fc-match` on Linux. It parses TTF/OTF `name` tables for family/style metadata, `post` tables for monospace detection, and `hmtx` tables for uniform advance width verification.

The browser requests the font list from `/fonts` (JSON array of family names) and fetches individual fonts on demand. This ensures the terminal renders with the same fonts available on the server — useful for icon fonts and coding ligatures.
