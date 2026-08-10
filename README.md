# blit

Terminal multiplexer and experimental Wayland compositor for browsers and AI agents. Nothing to configure, no required dependencies.

We publish a [computer agent skill](https://install.blit.sh/SKILL.md).

Try it now — no install needed:

```bash
docker run --rm --shm-size=1g grab/blit-demo
```

Or install and run locally:

```bash
curl -sf https://install.blit.sh | sh
blit open # opens a browser
```

Share over WebRTC:

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

Forward ports to whatever the server can reach — `ssh -L` over any blit
transport, plus UDP:

```bash
blit forward 8080:localhost:3000                # local 8080 → server's :3000
blit forward 8080:localhost:3000 5432:db:5432    # a list, over one connection
blit forward udp/5353:resolver.internal:53       # UDP too
blit forward add web 8080:localhost:3000         # remember it
blit forward --all                               # start every saved forward
```

Or proxy everything the server can reach through one port — `ssh -D`:

```bash
blit socks 1080                                  # SOCKS5 on 127.0.0.1:1080
curl -x socks5h://localhost:1080 http://api.internal/
```

Names are resolved on the server, so `socks5h://` (or a browser set to proxy
DNS) reaches hosts your machine cannot look up.

Listeners bind to loopback unless you name a bind address. The relay reaches
whatever the server reaches; restrict it with
`blit server --allow-forward 'host[:ports]'`. Saved forwards live in
`~/.config/blit/blit.forwards` (mode 0600). See
[docs/design/net.md](docs/design/net.md).

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

The default binary is MIT-licensed (software H.264 via openh264). On Linux
you can opt into a GPL build that uses x264 (GPL-2.0-or-later) for better
software H.264 instead:

```bash
curl -sf https://install.blit.sh | BLIT_GPL=1 sh
```

Every binary prints its exact terms with `blit --license`.

### Windows (PowerShell)

```powershell
irm https://install.blit.sh/install.ps1 | iex
```

This downloads `blit.exe` to `%LOCALAPPDATA%\blit\bin` and adds it to your user `PATH`. Set `BLIT_INSTALL_DIR` to override the install location on Windows.

## How it works

`blit` hosts PTYs and tracks full parsed terminal state. For each connected browser it computes a binary diff against what that browser last saw and sends only the delta — LZ4-compressed, with scrolling encoded as copy-rect operations. WebGL-rendered in the browser.

On Linux, every blit server includes an experimental headless Wayland compositor shared by all terminals. GUI applications launched inside any terminal (anything that speaks the Wayland protocol — terminal emulators, browsers, editors, media players) automatically connect to it. Surfaces are captured, encoded as H.264 or AV1 video, and streamed to connected browsers in real time. No X server, no display, no GPU required — rendering uses GPU compositing (Vulkan via dlopen) when available, with a CPU software fallback. Encoding uses openh264 or x264 (a build-time choice, see Install) and rav1e, with optional NVENC or VA-API hardware acceleration on Linux. The compositor is available on Linux only.

Each client is paced independently based on render metrics it reports back: display rate, frame apply time, backlog depth. A phone on 3G doesn't stall a workstation on localhost. The focused terminal gets full frame rate; background terminals throttle down. Keystrokes go straight to the PTY — latency is bounded by link RTT.

`blit open` opens the browser with an embedded gateway. For persistent multi-user browser access, `blit gateway` is a standalone proxy that handles passphrase auth, serves the web app, and optionally enables QUIC. `blit server` can also run standalone for headless/daemon use. For embedding in your own app, [`@blit-sh/react`](EMBEDDING.md) and [`@blit-sh/solid`](EMBEDDING.md) provide framework bindings.

`blit proxy-daemon` is a connection pool that makes remote connections feel local. It runs as a persistent background daemon per user session, maintaining pre-warmed connections to each upstream target so browser tabs connect instantly without paying SSH negotiation or TCP handshake cost. The proxy auto-starts transparently on Unix and Windows — set `BLIT_PROXY=0` to opt out.

For wire protocol details, frame encoding, and transport internals, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Configuration

| Variable                 | Default                                                                                                                | Purpose                                                                                                                                                                                                                                   |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BLIT_SOCK`              | `$TMPDIR/blit.sock`, `/tmp/blit-$USER.sock`, `/run/blit/$USER.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit.sock` | Unix socket path                                                                                                                                                                                                                          |
| `BLIT_EXPORT_SOCK`       | unset                                                                                                                  | Set to `1` (or pass `--export-sock` to `blit server`) to export the server's socket path as `BLIT_SOCK` in spawned terminals, so `blit` commands inside them target that server                                                           |
| `BLIT_INJECT_PATH`       | unset                                                                                                                  | Set to `1` (or pass `--inject-path` to `blit server`) to append the server binary's directory to `PATH` in spawned terminals, so `blit` itself is callable inside them                                                                    |
| `BLIT_UPLINK_TOKEN`      | unset                                                                                                                  | Bearer token for the `blit uplink` control endpoint                                                                                                                                                                                       |
| `BLIT_TARGET`            | unset                                                                                                                  | Default remote: a URI or named remote (overrides `target` in `blit.conf`)                                                                                                                                                                 |
| `BLIT_REMOTES`           | `~/.config/blit/blit.remotes`                                                                                          | Gateway remotes file path (overrides default location)                                                                                                                                                                                    |
| `BLIT_SCROLLBACK`        | `10000`                                                                                                                | Scrollback rows per PTY                                                                                                                                                                                                                   |
| `BLIT_HUB`               | `hub.blit.sh`                                                                                                          | Signaling hub URL for WebRTC sharing. On `blit gateway`, sets the default hub for `share:` remotes when `BLIT_GATEWAY_WEBRTC=1`.                                                                                                          |
| `BLIT_GATEWAY_WEBRTC`    | unset                                                                                                                  | Set to `1` on `blit gateway` to proxy `share:` remotes via WebRTC. The gateway connects as a WebRTC consumer and bridges terminals to browsers over WebSocket/WebTransport. Without this, `share:` entries in `blit.remotes` are ignored. |
| `BLIT_PASSPHRASE`        | unset                                                                                                                  | Browser passphrase for `blit gateway`; may be plaintext or an argon2 PHC hash generated by `blit hash-passphrase`. Browsers still enter the plaintext passphrase.                                                                         |
| `BLIT_PREFIX`            | `/usr/local` or `~/.local` (Unix)                                                                                      | Override install prefix (`bin/`, `lib/`, `share/` go under this)                                                                                                                                                                          |
| `BLIT_INSTALL_DIR`       | `%LOCALAPPDATA%\blit\bin` (Windows)                                                                                    | Override install location (Windows PowerShell installer)                                                                                                                                                                                  |
| `BLIT_SURFACE_ENCODERS`  | see below                                                                                                              | Comma-separated encoder priority list (see below)                                                                                                                                                                                         |
| `BLIT_SURFACE_BANDWIDTH` | `ultra`                                                                                                                | Ceiling on video bandwidth: `low`, `medium`, `high`, `ultra`, or a raw AV1 quantizer `10`–`255`. Adaptation is always on and only moves cheaper than this                                                                                 |
| `BLIT_SURFACE_SPEED`     | `realtime`                                                                                                             | Encoder speed preset: `slow`, `medium`, `fast`, `realtime`, or a raw `10`–`255` (10 = slowest, 255 = fastest)                                                                                                                             |
| `BLIT_VAAPI_DEVICE`      | `/dev/dri/renderD128`                                                                                                  | VA-API render node for hardware-accelerated encoding                                                                                                                                                                                      |
| `BLIT_CUDA_DEVICE`       | `0`                                                                                                                    | CUDA device ordinal for NVENC hardware encoding                                                                                                                                                                                           |

### Surface video encoders

Set `BLIT_SURFACE_ENCODERS` to a comma-separated priority list of encoders.
The server tries each in order and uses the first that works.

```bash
# Default priority (compositor-resident, then encode engines, then software):
# av1-vulkan,h264-vulkan,av1-nvenc,h264-nvenc,av1-vaapi,h264-vaapi,h264-software,av1-software

# Force software AV1 only:
BLIT_SURFACE_ENCODERS=av1-software

# Prefer NVENC, fall back to software:
BLIT_SURFACE_ENCODERS=av1-nvenc,h264-nvenc,h264-software
```

| Value           | Codec | Backend          | Max resolution | Notes                                                                                                |
| --------------- | ----- | ---------------- | -------------- | ---------------------------------------------------------------------------------------------------- |
| `av1-nvenc`     | AV1   | NVIDIA NVENC     | 8192×4352      | RTX 40+ series; fastest AV1 encode                                                                   |
| `h264-nvenc`    | H.264 | NVIDIA NVENC     | 3840×2160      | Requires proprietary NVIDIA driver                                                                   |
| `av1-vaapi`     | AV1   | VA-API           | 8192×4352      | Intel/AMD GPU                                                                                        |
| `h264-vaapi`    | H.264 | VA-API           | 3840×2160      | Intel/AMD GPU                                                                                        |
| `av1-vulkan`    | AV1   | Vulkan Video     | 8192×4352      | On the compositor's GPU; per-client scaling and pacing; 4:4:4 where the driver supports it           |
| `h264-vulkan`   | H.264 | Vulkan Video     | 3840×2160      | On the compositor's GPU; per-client scaling and pacing; 4:4:4 where the driver supports it           |
| `h264-software` | H.264 | openh264 or x264 | 3840×2160      | Build-time choice (x264 = GPL opt-in)                                                                |
| `av1-software`  | AV1   | rav1e (software) | 3840×2160      | CPU-bound; capped to stay interactive                                                                |

The browser automatically detects the codec from each frame and configures
its WebCodecs decoder accordingly. Clients advertise which codecs they
support and the largest frame they can decode; the server skips encoders the
client can't decode.

The resolution ceiling is per viewer, not per surface. At ordinary display
scales a surface is composited at whatever its most capable subscriber can
receive — so an AV1 client on a 5K display gets a native 5120×2880 stream —
and any viewer whose encoder or decoder stops lower is served an
aspect-preserving downscale of that same surface rather than dragging it down
for everyone. At a sub-1× zoom the 1× compositor source may be larger than the
encode ceiling; the viewer still receives only its viewport-sized downscale.
Clients that don't report a decode ceiling (anything predating the field)
stay at 3840×2160.

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

| Dependency             | Packages (Debian/Ubuntu)          | Used for                                          |
| ---------------------- | --------------------------------- | ------------------------------------------------- |
| `pipewire`             | `pipewire`                        | Audio daemon (private instance per compositor)    |
| `pipewire-pulse`       | `pipewire-pulse`                  | PulseAudio compatibility for apps                 |
| `libpipewire-0.3.so.0` | `pipewire` or `libpipewire-0.3-0` | Monitor capture (in-process, loaded via `dlopen`) |
| `dbus-daemon`          | `dbus`                            | Private D-Bus session (required by PipeWire)      |
| `wireplumber`          | `wireplumber`                     | Session manager (optional, started if available)  |

Audio is disabled automatically when PipeWire is not installed or `libpipewire-0.3.so.0` is not resolvable via `ld.so` (set `LD_LIBRARY_PATH` if you have it in a non-default location), or explicitly with `BLIT_AUDIO=0`.

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

The `grab/blit-demo` image runs unprivileged and launches `blit share` on startup. It includes `blit` itself (the GPL flavor, so software H.264 uses x264 — same as `BLIT_GPL=1` installs), plus fish, busybox, htop, neovim, git, curl, jq, tree, ncdu, and Wayland GUI apps (firefox, foot, mpv, imv, zathura, wev).

The session starts in `/home/blit/blit`, a writable clone of this repo with its full history. The container clones it on first start and fast-forwards it on later ones, so it reflects `main` as of when you started the container, not as of when the image was built. It needs network access to GitHub; without it you still get a shell, just an empty directory.

To build locally:

```bash
nix build .#demo-image
docker load < result
docker run --rm --shm-size=1g grab/blit-demo
```

Firefox wants more shared memory than Docker's default 64 MB; without `--shm-size` its tabs crash. Nothing else in the image cares.
