# Server Internals

`blit server` is a single async Rust binary (tokio runtime). It owns PTYs, terminal state, and per-client frame scheduling. It has no CLI subcommands and no RPC API beyond the binary protocol described in [protocol.md](protocol.md). Configuration is entirely via environment variables.

## Configuration

| Variable                | Default                                            | Purpose                          |
| ----------------------- | -------------------------------------------------- | -------------------------------- |
| `BLIT_SOCK`             | see path cascade in [transports.md](transports.md) | Unix socket listen path          |
| `SHELL`                 | `$SHELL` or `/bin/sh`                              | Shell spawned for new PTYs       |
| `BLIT_SHELL_FLAGS`      | `li` (Unix) / `` (Windows)                         | Shell invocation flags           |
| `BLIT_SCROLLBACK`       | `1000000`                                          | Scrollback buffer rows per PTY   |
| `BLIT_VAAPI_DEVICE`     | `/dev/dri/renderD128`                              | VA-API render node for encoding  |
| `BLIT_CUDA_DEVICE`      | `0`                                                | CUDA device ordinal (NVENC)      |
| `BLIT_FD_CHANNEL`       | unset                                              | fd-channel file descriptor       |
| `BLIT_SURFACE_ENCODERS` | see encoder table                                  | Comma-separated encoder priority |
| `BLIT_SURFACE_QUALITY`  | `medium`                                           | Video quality preset             |

## PTY lifecycle

### Creation

PTYs are created by `C2S_CREATE` or `C2S_CREATE2`. The server:

1. Allocates a PTY pair via `openpty`.
2. Forks. The child sets the slave fd as controlling terminal (`TIOCSCTTY`), drops privileges, sets the working directory, and `exec`s the shell (or custom command from `HAS_COMMAND`).
3. The master fd is registered with the tokio reactor for async I/O.
4. PTY output is fed through the `blit-alacritty` terminal parser.
5. `S2C_CREATED` (or `S2C_CREATED_N` with nonce) is sent to the creating client.
6. All connected clients receive `S2C_LIST` reflecting the new PTY.

### Exit

When the PTY subprocess exits, `waitpid` captures the exit status:

- Normal exit: `WEXITSTATUS` (0, 1, …).
- Signal death: negative signal number (-9 = SIGKILL, -15 = SIGTERM).
- Unknown: `i32::MIN`.

`S2C_EXITED` is broadcast to all subscribed clients. The terminal state is retained — clients can still scroll and read. The PTY slot is marked exited but not freed.

`C2S_RESTART` respawns the shell in the same slot. `C2S_CLOSE` removes the PTY entirely and frees the slot.

### Multi-client state

- **Subscriptions**: clients subscribe per-PTY with `C2S_SUBSCRIBE`. The server only sends `S2C_UPDATE` frames to subscribed clients.
- **Focus**: each client has an independent focused PTY (`C2S_FOCUS`). The focused PTY gets full frame rate; subscribed-but-unfocused PTYs get a capped preview rate.
- **Sizing**: each client reports its desired dimensions per PTY via `C2S_RESIZE`. The effective PTY size is the minimum across all subscribed clients, so the terminal fits every viewer's window.

## Terminal emulation

Terminal parsing is handled by `alacritty_terminal` (v0.25), wrapped by `blit-alacritty` (`crates/alacritty-driver/`). The wrapper adds:

- **Snapshot generation** — converts `alacritty_terminal`'s `Term` into `blit-remote::FrameState` (the 12-byte cell grid). Called once per scheduled frame.
- **Scrollback frames** — generates frames at arbitrary scroll offsets into the scrollback buffer, without modifying the live terminal state.
- **Mode tracking** — a custom `ModeTracker` intercepts CSI/DCS sequences from raw PTY output to detect mode changes: `DECCKM`, `DECSCUSR`, mouse modes (`?9h`, `?1000h`, `?1002h`, `?1003h`), SGR mouse encoding (`?1006h`), synchronized output, etc. These are packed into the 16-bit mode field sent with each frame.
- **Search** — full-text search across visible content, titles, and scrollback, returning scored results with match context and scroll offsets.

The server also polls `tcgetattr` on the PTY master fd to detect echo and canonical mode flags. These are packed into mode bits 9 and 10 so the browser can implement predicted echo (showing keystrokes before the server confirms them).

## Per-client frame pacing

The server maintains detailed per-client congestion state. No client can block another.

### RTT estimation

Each `S2C_UPDATE` increments an in-flight counter. Each `C2S_ACK` retires the oldest in-flight frame and records the one-way delivery time. RTT is tracked as:

- **EWMA RTT** — exponentially weighted moving average.
- **Minimum-path RTT** — the smallest RTT seen, decayed slowly.

### Bandwidth estimation

- **Delivered rate** — EWMA of `frame_bytes / delivery_time`.
- **ACK goodput** — bytes acknowledged per ACK interval.
- **Jitter tracking** — EWMA of frame delivery time variance, with a decaying peak, feeding into a conservative bandwidth floor.

### Frame window

Frames in flight are capped by both:

- A **frame count** — bounded by RTT and display rate.
- A **byte budget** — bounded by the bandwidth-delay product.

The window adapts dynamically. High-latency links get deeper pipelines to stay fully utilized. Low-latency local links pipeline nothing beyond what the client can immediately render.

### Display pacing

The client reports:

- `C2S_DISPLAY_RATE` — the display refresh rate in Hz.
- `C2S_CLIENT_METRICS` — backlog depth, ack-ahead count, frame apply time (in 0.1 ms units).

The server spaces frame sends to match the client's actual render rate. When backlog grows (client falling behind), the server backs off.
The final transport gate is byte-aware: queued bytes and queued message count both feed outbox backpressure, so a couple of tiny terminal diffs do not stall a large surface/video frame. Bulk writes are chunked so audio can interleave while large terminal or video payloads are draining.

### Preview budgeting

Background PTYs (subscribed but not focused) share leftover bandwidth after the focused PTY's needs are met. Preview frame rate is capped to avoid starving the focused terminal.

### Probe and backoff

After a conservative backoff, the server gradually probes with additive window growth. Probe frames are discarded when queue delay rises.

**Result**: a fast client on localhost gets frames at its full display rate. A slow client on a mobile connection gets paced to its actual capacity. Neither blocks the other.

## Frame scheduling flow

```mermaid
sequenceDiagram
    participant PTY
    participant Server as blit server
    participant Client as gateway / browser

    PTY->>Server: raw bytes
    Server->>Server: alacritty_terminal parses VT state
    Server->>Server: tick loop wakes (tokio Notify)
    Server->>Server: snapshot → diff → encode ops → LZ4
    Server->>Client: S2C_UPDATE (compressed frame)
    Client->>Client: decompress + apply diff
    Client->>Client: render (WebGPU / WebGL2)
    Client-->>Server: C2S_ACK
    Server->>Server: retire in-flight frame, update RTT, open window
```

## Headless Wayland compositor (experimental)

The compositor is optionally enabled for terminals that need GUI app support. It is lazily initialized and shared across all PTYs in a connection.

### Initialization

`ensure_compositor()` lazily starts a headless Wayland compositor on a dedicated OS thread, listening on a randomly-chosen `wayland-blit-*` socket. Each compositor gets a monotonic internal ID from a server-side counter, used to identify the audio pipeline instance. Surface messages carry only the `surface_id` assigned by the compositor; the server routes to the correct compositor instance internally.

All PTYs forked after the compositor starts inherit `WAYLAND_DISPLAY` pointing at the shared compositor socket. Any program — shell, TUI, or GUI app — can open Wayland surfaces from any PTY.

### Surface lifecycle

1. The app creates an `xdg_toplevel` surface; the compositor assigns it a `surface_id`.
2. The compositor sends `SurfaceCommit` events carrying a `PixelData` value. With Vulkan Video this is a pre-encoded bitstream (`PixelData::Encoded`); otherwise it is NV12 DMA-BUF data or BGRA pixels for server-side encoding.
3. The server either forwards the pre-encoded bitstream directly (Vulkan Video) or encodes the pixel data via the configured encoder chain (VA-API / NVENC / software).
4. `S2C_SURFACE_CREATED` is broadcast to subscribed clients, followed by `S2C_SURFACE_FRAME` as the app renders.
5. Input events from clients (`C2S_SURFACE_INPUT`, `C2S_SURFACE_POINTER`, `C2S_SURFACE_POINTER_AXIS`) are translated to Wayland keyboard/pointer events via the compositor.
6. When the app closes the surface, `S2C_SURFACE_DESTROYED` is broadcast.

### Frame production pipeline

```mermaid
sequenceDiagram
    participant S as blit server
    participant C as blit-compositor
    participant A as Wayland app
    participant Cl as client

    S->>C: RequestFrame + LoopSignal wake
    C->>A: wl_surface.frame callback
    A->>C: wl_surface.commit (buffer)
    C->>C: GPU composite (Vulkan)
    C->>C: compute BGRA→NV12
    alt Vulkan Video
        C->>C: GPU encode (H.264/AV1)
        C->>S: SurfaceCommit (Encoded bitstream)
    else VA-API / NVENC / Software
        C->>S: SurfaceCommit (NV12 DMA-BUF)
        S->>S: encode
    end
    S->>Cl: S2C_SURFACE_FRAME
```

`RequestFrame` is only sent for surfaces that have subscribers and no pending request, preventing busy-loops when the app hasn't painted yet.

### GPU rendering and encoding

The compositor uses a Vulkan renderer (`VulkanRenderer`) loaded at runtime via `ash` (dlopen `libvulkan.so`). Client surface buffers (SHM or DMA-BUF) are uploaded as persistent GPU textures at `wl_surface.commit` time and reused across frames until the surface commits a new buffer.

#### Output pipeline

The render pipeline has three tiers, chosen at runtime based on hardware capabilities:

**Tier 1 — Vulkan Video (fully on-GPU, preferred):**
When `VK_KHR_video_encode_queue` + `VK_KHR_video_encode_h264` / `VK_KHR_video_encode_av1` are available, the compositor does the entire pipeline in Vulkan: render BGRA → compute shader BGRA→NV12 → Vulkan Video hardware encode → bitstream readback. Returns `PixelData::Encoded` — the server sends the bitstream directly to clients with zero encoding work. No VA-API, no DMA-BUF export/import, no cross-API sync. Supported on AMD (radv, RDNA2+) and Intel (anv).

**Tier 2 — Vulkan compute + VA-API encode (zero-copy NV12):**
VA-API allocates NV12 surfaces and exports them as DMA-BUFs. The compositor imports these into Vulkan as multi-plane `G8_B8R8_2PLANE_420_UNORM` images (handles tiled surfaces on AMD via `VK_EXT_image_drm_format_modifier`). A compute shader converts BGRA→NV12 via `imageStore` on per-plane views. The VA-API encoder reads the surface directly — zero CPU involvement. Returns `PixelData::Nv12DmaBuf` with an `Arc<OwnedFd>` shared between compositor and encoder for fd-based surface lookup.

**Tier 3 — CPU fallback:**
When neither Vulkan Video nor VA-API external outputs are available, the renderer falls back to self-allocated output images with HOST_VISIBLE staging buffers. The composited BGRA frame is copied to a staging buffer and returned as `PixelData::Bgra` for CPU-side encoding.

External outputs and NV12 compute buffers are **per-surface** (`HashMap<u32, Vec<...>>` keyed by surface ID) so multiple surfaces (e.g., Brave + mpv) encode independently without interference.

### Encoder selection

Controlled by `BLIT_SURFACE_ENCODERS` (comma-separated priority list). The server tries each in order and uses the first that succeeds at runtime. Default priority:

```
av1-nvenc, h264-nvenc, av1-vaapi, h264-vaapi, h264-software, av1-software
```

Vulkan Video encoders (`av1-vulkan`, `h264-vulkan`) are available but not in the default list yet as they haven't been tested. Enable via `BLIT_SURFACE_ENCODERS=av1-vulkan,...`.

| Encoder         | Backend            | Notes                              |
| --------------- | ------------------ | ---------------------------------- |
| `av1-vulkan`    | Vulkan Video (GPU) | AV1 via VK_KHR_video_encode_av1    |
| `h264-vulkan`   | Vulkan Video (GPU) | H.264 via VK_KHR_video_encode_h264 |
| `av1-nvenc`     | NVENC (GPU)        | AV1 via CUDA                       |
| `h264-nvenc`    | NVENC (GPU)        | H.264 via CUDA                     |
| `av1-vaapi`     | VA-API (GPU)       | AV1 via libva                      |
| `h264-vaapi`    | VA-API (GPU)       | H.264 via libva                    |
| `h264-software` | openh264 (CPU)     | Software H.264, always available   |
| `av1-software`  | rav1e (CPU)        | Software AV1                       |

`BLIT_SURFACE_QUALITY`: `low`, `medium` (default), `high`, `ultra`.

### Compositor capabilities

- **Protocols**: `wl_compositor` v6, `xdg-shell` v6, `wp_viewporter`, `wp_fractional_scale_manager` v1, `xdg-decoration`, `zwp_linux_dmabuf` v3, `wp_presentation`, `zwp_pointer_constraints` v1, `zwp_relative_pointer_manager` v1, `xdg-activation` v1, `wp_cursor_shape_manager` v1.
- **Buffer types**: SHM (shared memory) and DMA-BUF (GPU). DMA-BUF accepted via `zwp_linux_dmabuf_v1`; client buffers imported via Vulkan external memory extensions (`VK_EXT_external_memory_dma_buf`) and composited as Vulkan textures.
- **Pixel formats**: ARGB8888, XRGB8888, ABGR8888, XBGR8888 with linear modifier or `DRM_FORMAT_MOD_INVALID` (treated as linear).
- **Screenshots**: `blit surface capture <surface_id>` uses the Vulkan renderer to composite the surface tree and reads back RGBA pixels. Output format: PNG or AVIF, inferred from file extension.

Chrome/Electron work with `--ozone-platform=wayland`. mpv works with `--vo=gpu-next` (Vulkan WSI submits DMA-BUFs via `zwp_linux_dmabuf`).

## Audio

Audio capture, encoding, and playback are handled by a PipeWire-based pipeline.

### Architecture

```mermaid
graph LR
    subgraph "Server (blit server)"
        App["Wayland/PTY app"] -->|"audio output"| PW["PipeWire<br/>blit-sink"]
        PW -->|"monitor source"| PWCAT["pw-cat --record<br/>48kHz f32 stereo"]
        PWCAT -->|"raw PCM pipe"| ENC["Opus encoder<br/>20ms frames"]
        ENC -->|"OpusFrame"| RING["ring buffer<br/>10 frames"]
    end
    subgraph "Client (browser)"
        RING -->|"S2C_AUDIO_FRAME"| DEC["WebCodecs<br/>AudioDecoder"]
        DEC -->|"f32 PCM"| WK["AudioWorklet<br/>jitter buffer"]
        WK -->|"output"| SPK["speakers"]
    end
```

### Capture

`AudioPipeline::spawn()` (`crates/server/src/audio.rs`) starts a private, isolated PipeWire stack per compositor instance:

| Process           | Role                                                      |
| ----------------- | --------------------------------------------------------- |
| `dbus-daemon`     | Private D-Bus session (required by PipeWire modules)      |
| `pipewire`        | Core daemon with a null sink (`blit-sink`, 48 kHz stereo) |
| `wireplumber`     | Minimal session manager (hardware monitors disabled)      |
| `pipewire-pulse`  | PulseAudio compatibility socket                           |
| `pw-cat --record` | Captures `blit-sink` monitor, writes raw PCM to stdout    |

Child processes inherit `PIPEWIRE_REMOTE` and `PULSE_SERVER` pointing at the private sockets. Audio availability is gated by `pipewire_available()` (checks for required binaries on PATH) and can be disabled with `BLIT_AUDIO=0`.

### Encoding

`reader_encoder_task()` is an async tokio task that reads the `pw-cat` stdout pipe:

1. Buffers raw PCM and frames it into 20 ms chunks (960 samples/channel, stereo).
2. Encodes each chunk with libopus at the current bitrate (default 64 kbps, adjustable per-subscriber via `C2S_AUDIO_SUBSCRIBE`).
3. Timestamps each frame using the same epoch as video frame timestamps, enabling A/V sync on the client.
4. Sends frames through an mpsc channel (capacity 20). Frames are dropped if the channel is full to avoid stalling PipeWire's realtime thread.

A ring buffer of 10 recent frames (200 ms) provides catch-up delivery when new clients subscribe.

### Transport

| Message                        | Direction       | Layout                                              |
| ------------------------------ | --------------- | --------------------------------------------------- |
| `C2S_AUDIO_SUBSCRIBE` (0x30)   | client → server | `[opcode][bitrate_kbps:u16 LE]`                     |
| `C2S_AUDIO_UNSUBSCRIBE` (0x31) | client → server | `[opcode]`                                          |
| `S2C_AUDIO_FRAME` (0x30)       | server → client | `[opcode][timestamp:u32 LE][flags:u8][opus_data:N]` |

On subscribe, the server sends ring-buffer catch-up frames and recomputes the Opus bitrate as the maximum of all subscribers' requested bitrates.

### Playback

`AudioPlayer` (`js/core/src/AudioPlayer.ts`) handles decode and render in the browser:

1. **Decode**: WebCodecs `AudioDecoder` with `codec: "opus"`, 48 kHz stereo. Decoded `AudioData` frames (f32 planar PCM) are transferred to the worklet via `MessagePort`.
2. **Render**: An `AudioWorkletProcessor` maintains a jitter buffer (target 100 ms / 4800 samples). Outputs silence until the buffer fills; re-enters buffering on underrun.
3. **A/V sync**: The worklet reports its consumed-sample position. The main thread maps this to a server timestamp via a recorded timeline, computes drift against video timestamps, and steers the playback rate within +/-2% to converge. Rate changes are exponentially smoothed (alpha 0.15) to prevent audible wow/flutter.

```mermaid
sequenceDiagram
    participant S as blit server
    participant C as browser client

    S->>C: S2C_AUDIO_FRAME (Opus)
    C->>C: AudioDecoder → f32 PCM
    C->>C: AudioWorklet jitter buffer
    Note over C: A/V sync: drift = audioMs - videoMs
    C->>C: adjust playback rate ±2%
```
