# blit

Terminal multiplexer and experimental Wayland compositor for browsers and AI agents. Nothing to configure, no required dependencies.

We publish a [computer agent skill](https://install.blit.sh/SKILL.md).

Try it now — no install needed:

```bash
docker run --rm grab/blit-demo
```

Or install and run locally:

```bash
curl -sf https://install.blit.sh | sh
blit open # opens a browser
```

Share a terminal over WebRTC:

```bash
blit share # prints a URL anyone can open
```

Manage named remotes and connect to them:

```bash
blit remote add rabbit ssh:rabbit          # save a named remote
blit remote add prod ssh:alice@prod.co     # another one
blit remote list                           # show all remotes
blit remote set-default rabbit             # make rabbit the default

blit open                                  # local + all configured remotes
blit terminal list                         # lists terminals on rabbit
blit --on prod terminal list               # one-off override
blit --on ssh:newhost terminal list        # full URI also works
```

The default remote is stored in `~/.config/blit/blit.conf` as `blit.target = rabbit`
and can also be set via the `BLIT_TARGET` environment variable. Named remotes
are stored in `~/.config/blit/blit.remotes` (mode 0600). `blit open` reads this
file and shows all remotes in the browser's Remotes dialog (Cmd+K). SSH remotes
are auto-installed on first connection.

Control terminals programmatically:

```bash
blit terminal start htop # start a terminal, print its ID
blit terminal show 1     # dump current terminal text
blit terminal send 1 q   # send keystrokes
```

Run GUI apps — on Linux, every terminal includes an experimental headless Wayland compositor:

```bash
blit terminal start foot    # launch a Wayland terminal emulator
blit surface list           # list graphical windows
blit surface capture 1      # screenshot a surface
blit surface click 1 100 50 # click at (x, y)
blit surface type 1 "hello{Return}" # type into a GUI window
```

The server auto-starts when needed.

## Supported platforms

| Platform | Arch          | Wayland compositor | Notes                 |
| -------- | ------------- | ------------------ | --------------------- |
| Linux    | x86_64, arm64 | Yes                | Full features         |
| macOS    | arm64         | No                 | PTY multiplexing only |
| Windows  | x86_64        | No                 | PTY multiplexing only |

SSH remotes are auto-installed on first connection. Requirements on the remote:
`curl` or `wget`, CA certificates, and a supported OS/arch.

The embedded SSH client authenticates via ssh-agent (`SSH_AUTH_SOCK`) or key files
(`~/.ssh/id_{ed25519,ecdsa,rsa}`), and resolves `~/.ssh/config` for Hostname,
User, Port, and IdentityFile.

## Install

```bash
curl -sf https://install.blit.sh | sh
```

### Windows (PowerShell)

```powershell
irm https://install.blit.sh/install.ps1 | iex
```

This downloads `blit.exe` to `%LOCALAPPDATA%\blit\bin` and adds it to your user `PATH`. Set `BLIT_INSTALL_DIR` to override the install location on Windows.

## How it works

`blit` hosts PTYs and tracks full parsed terminal state. For each connected browser it computes a binary diff against what that browser last saw and sends only the delta — LZ4-compressed, with scrolling encoded as copy-rect operations. WebGL-rendered in the browser.

On Linux, every blit server includes an experimental headless Wayland compositor shared by all terminals. GUI applications launched inside any terminal (anything that speaks the Wayland protocol — terminal emulators, browsers, editors, media players) automatically connect to it. Surfaces are captured, encoded as H.264 or AV1 video, and streamed to connected browsers in real time. No X server, no display, no GPU required — rendering uses GPU compositing (Vulkan via dlopen) when available, with a CPU software fallback. Encoding uses openh264/rav1e (with optional NVENC or VA-API hardware acceleration on Linux). The compositor is available on Linux only.

Each client is paced independently based on render metrics it reports back: display rate, frame apply time, backlog depth. A phone on 3G doesn't stall a workstation on localhost. The focused terminal gets full frame rate; background terminals throttle down. Keystrokes go straight to the PTY — latency is bounded by link RTT.

`blit open` opens the browser with an embedded gateway. For persistent multi-user browser access, `blit gateway` is a standalone proxy that handles passphrase auth, serves the web app, and optionally enables QUIC. `blit server` can also run standalone for headless/daemon use. For embedding in your own app, [`@blit-sh/react`](EMBEDDING.md) and [`@blit-sh/solid`](EMBEDDING.md) provide framework bindings.

`blit proxy-daemon` is a connection pool that makes remote connections feel local. It runs as a persistent background daemon per user session, maintaining pre-warmed connections to each upstream target so browser tabs connect instantly without paying SSH negotiation or TCP handshake cost. The proxy auto-starts transparently on Unix and Windows — set `BLIT_PROXY=0` to opt out.

For wire protocol details, frame encoding, and transport internals, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Configuration

| Variable                | Default                                                                                                                | Purpose                                                                                                                                                                                                                                   |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BLIT_SOCK`             | `$TMPDIR/blit.sock`, `/tmp/blit-$USER.sock`, `/run/blit/$USER.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit.sock` | Unix socket path                                                                                                                                                                                                                          |
| `BLIT_TARGET`           | unset                                                                                                                  | Default remote: a URI or named remote (overrides `target` in `blit.conf`)                                                                                                                                                                 |
| `BLIT_REMOTES`          | `~/.config/blit/blit.remotes`                                                                                          | Gateway remotes file path (overrides default location)                                                                                                                                                                                    |
| `BLIT_SCROLLBACK`       | `10000`                                                                                                                | Scrollback rows per PTY                                                                                                                                                                                                                   |
| `BLIT_HUB`              | `hub.blit.sh`                                                                                                          | Signaling hub URL for WebRTC sharing. On `blit gateway`, sets the default hub for `share:` remotes when `BLIT_GATEWAY_WEBRTC=1`.                                                                                                          |
| `BLIT_GATEWAY_WEBRTC`   | unset                                                                                                                  | Set to `1` on `blit gateway` to proxy `share:` remotes via WebRTC. The gateway connects as a WebRTC consumer and bridges terminals to browsers over WebSocket/WebTransport. Without this, `share:` entries in `blit.remotes` are ignored. |
| `BLIT_PREFIX`           | `/usr/local` or `~/.local` (Unix)                                                                                      | Override install prefix (`bin/`, `lib/`, `share/` go under this)                                                                                                                                                                          |
| `BLIT_INSTALL_DIR`      | `%LOCALAPPDATA%\blit\bin` (Windows)                                                                                    | Override install location (Windows PowerShell installer)                                                                                                                                                                                  |
| `BLIT_SURFACE_ENCODERS` | see below                                                                                                              | Comma-separated encoder priority list (see below)                                                                                                                                                                                         |
| `BLIT_SURFACE_QUALITY`  | `medium`                                                                                                               | Video quality preset: `low`, `medium`, `high`, `lossless`                                                                                                                                                                                 |
| `BLIT_VAAPI_DEVICE`     | `/dev/dri/renderD128`                                                                                                  | VA-API render node for hardware-accelerated encoding                                                                                                                                                                                      |
| `BLIT_CUDA_DEVICE`      | `0`                                                                                                                    | CUDA device ordinal for NVENC hardware encoding                                                                                                                                                                                           |

### Surface video encoders

Set `BLIT_SURFACE_ENCODERS` to a comma-separated priority list of encoders.
The server tries each in order and uses the first that works.

```bash
# Default priority (hardware before software):
# av1-nvenc,h264-nvenc,av1-vaapi,h264-vaapi,h264-software,av1-software

# Force software AV1 only:
BLIT_SURFACE_ENCODERS=av1-software

# Prefer NVENC, fall back to software:
BLIT_SURFACE_ENCODERS=av1-nvenc,h264-nvenc,h264-software
```

| Value           | Codec | Backend          | Notes                                           |
| --------------- | ----- | ---------------- | ----------------------------------------------- |
| `av1-nvenc`     | AV1   | NVIDIA NVENC     | RTX 40+ series; fastest AV1 encode              |
| `h264-nvenc`    | H.264 | NVIDIA NVENC     | Requires proprietary NVIDIA driver              |
| `av1-vaapi`     | AV1   | VA-API           | Intel/AMD GPU                                   |
| `h264-vaapi`    | H.264 | VA-API           | Intel/AMD GPU; max 3840×2160                    |
| `h264-software` | H.264 | openh264         | Max 3840×2160; lowest CPU but worst compression |
| `av1-software`  | AV1   | rav1e (software) | No resolution limit; CPU-heavy at high res      |

The browser automatically detects the codec from each frame and configures
its WebCodecs decoder accordingly. Clients can also advertise which codecs
they support; the server skips encoders the client can't decode.

For `blit gateway` configuration, running as a systemd/launchd service, and Nix module setup, see [SERVICES.md](SERVICES.md) and [`nix/README.md`](nix/README.md).

### Optional dependencies

blit has no required dependencies — software H.264 and AV1 encoders are statically linked, and the CPU software renderer works everywhere. GPU acceleration and audio are enabled automatically when the right libraries or binaries are present. All GPU libraries are loaded at runtime via `dlopen`; missing ones are silently skipped.

**Video — GPU compositing and hardware encoding (Linux)**

| Library                                 | Packages (Debian/Ubuntu)                             | Used for                                         |
| --------------------------------------- | ---------------------------------------------------- | ------------------------------------------------ |
| `libvulkan.so.1`                        | `libvulkan1`, `mesa-vulkan-drivers` or NVIDIA driver | GPU compositing, Vulkan Video encode             |
| `libva.so.2`, `libva-drm.so.2`          | `libva2`, `libva-drm2`, `va-driver-all`              | VA-API hardware encode (Intel/AMD)               |
| `libgbm.so.1`                           | `libgbm1`                                            | DMA-BUF allocation for zero-copy VA-API encoding |
| `libcuda.so.1`, `libnvidia-encode.so.1` | NVIDIA proprietary driver                            | NVENC hardware encode                            |

Without any of the above, the compositor falls back to CPU rendering and software encoding. No configuration needed.

**Audio (Linux)**

| Binary           | Packages (Debian/Ubuntu)       | Used for                                         |
| ---------------- | ------------------------------ | ------------------------------------------------ |
| `pipewire`       | `pipewire`                     | Audio daemon (private instance per compositor)   |
| `pipewire-pulse` | `pipewire-pulse`               | PulseAudio compatibility for apps                |
| `pw-cat`         | `pipewire` or `pipewire-utils` | Monitor source capture                           |
| `dbus-daemon`    | `dbus`                         | Private D-Bus session (required by PipeWire)     |
| `wireplumber`    | `wireplumber`                  | Session manager (optional, started if available) |

Audio is disabled automatically when PipeWire is not installed, or explicitly with `BLIT_AUDIO=0`.

## How it compares

|                          | blit                                | ttyd                | gotty               | Eternal Terminal      | Mosh                  | xterm.js + node-pty  |
| ------------------------ | ----------------------------------- | ------------------- | ------------------- | --------------------- | --------------------- | -------------------- |
| Architecture             | Single binary                       | Single binary       | Single binary       | Client + daemon       | Client + server       | Library (BYO server) |
| Multiple PTYs            | ✅ First-class                      | ❌ One per instance | ❌ One per instance | ❌ One per connection | ❌ One per connection | ⚠️ Manual            |
| Browser access           | ✅                                  | ✅                  | ✅                  | ❌                    | ❌                    | ✅                   |
| Delta updates            | ✅ Only changed cells               | ❌                  | ❌                  | ❌                    | ✅ State diffs        | ❌                   |
| LZ4 compression          | ✅                                  | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Per-client backpressure  | ✅ Render-metric pacing             | ❌                  | ❌                  | ⚠️ SSH flow control   | ❌                    | ❌                   |
| WebGL rendering          | ✅                                  | ❌                  | ❌                  | ❌                    | ❌                    | ⚠️ Addon             |
| Transport                | WS, WebTransport, WebRTC, Unix      | WebSocket           | WebSocket           | TCP                   | UDP                   | WebSocket            |
| Embeddable (React/Solid) | ✅                                  | ❌                  | ❌                  | ❌                    | ❌                    | ✅                   |
| Wayland compositor       | ✅ Built-in headless (experimental) | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| GUI app streaming        | ✅ H.264 / AV1                      | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Agent / CLI subcommands  | ✅                                  | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| SSH tunneling built-in   | ✅                                  | ❌                  | ❌                  | ✅                    | ✅                    | ❌                   |

## Browser tips

### Disable Ctrl+W tab close (Chrome / Brave / Edge)

When using blit in the browser, `Ctrl+W` closes the browser tab instead of
reaching your terminal. Chromium-based browsers let you disable this:

1. Navigate to `chrome://settings/system/shortcuts`
   (or `brave://settings/system/shortcuts` in Brave)
2. Find the **Close Tab** shortcut and remove or reassign it

This frees `Ctrl+W` for terminal use (e.g. deleting a word in bash/zsh).

## Contributing

Building from source, running tests, dev environment setup, code conventions, and release process are all covered in [CONTRIBUTING.md](CONTRIBUTING.md). CI/CD pipelines, the install site, and the signaling hub are documented in [SERVICES.md](SERVICES.md). The crate and package map is in [ARCHITECTURE.md](ARCHITECTURE.md).

## Docker sandbox

The `grab/blit-demo` image runs unprivileged and launches `blit share` on startup. It includes `blit` itself, plus fish, busybox, htop, neovim, git, curl, jq, tree, ncdu, and Wayland GUI apps (foot, mpv, imv, zathura, wev).

To build locally:

```bash
nix build .#demo-image
docker load < result
docker run --rm grab/blit-demo
```
