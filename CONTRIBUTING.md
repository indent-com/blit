Reading material:

- ARCHITECTURE.md
- EMBEDDING.md
- README.md
- SERVICES.md
- SKILL.md
- UNSAFE.md
- nix/README.md
- js/hub/README.md

# Contributing to blit

This document helps LLM agents (and humans) contribute to the blit codebase. It covers the development workflow, code conventions, and project structure. For the system architecture, see [ARCHITECTURE.md](ARCHITECTURE.md). For user-facing documentation, see [README.md](README.md).

## Documentation maintenance guide

When making changes, update the relevant docs in the same PR.

| Document                  | Scope                                                                                                                                         | Update when...                                                                                                               |
| ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `README.md`               | User-facing overview: installation, usage, features                                                                                           | CLI flags, install methods, or supported platforms change                                                                    |
| `ARCHITECTURE.md`         | System internals: data flow, crate responsibilities, transport layers, rendering pipeline                                                     | Crates are added/removed/renamed, data flow between components changes, or new transport/rendering mechanisms are introduced |
| `CONTRIBUTING.md`         | Developer workflow: building, testing, code conventions, project structure                                                                    | Build steps, test commands, directory layout, or dev tooling changes                                                         |
| `SERVICES.md`             | Hosted services, CI/CD, and running as a service (Homebrew, systemd)                                                                          | CI jobs are added/removed/changed, deployment targets change, new secrets are introduced, or the release process is modified |
| `EMBEDDING.md`            | Embedding blit in other apps: React components (`@blit-sh/react`), embedding `blit server` as a library                                       | Public embedding APIs, component props, or integration patterns change                                                       |
| `SKILL.md`                | LLM agent skill definition: install instructions and pointer to `blit learn`. Deployed to `install.blit.sh/SKILL.md` by the release workflow. | Install methods change or the `learn` subcommand output changes                                                              |
| `crates/cli/src/learn.md` | Full CLI reference printed by `blit learn`: usage patterns, subcommand details, transport options, escapes                                    | CLI subcommands, flags, output conventions, or transport options change                                                      |
| `UNSAFE.md`               | Unsafe Rust code audit: which crates use `unsafe`, why, and what invariants they rely on                                                      | Unsafe code is added, removed, or its safety invariants change                                                               |
| `nix/README.md`           | nix-darwin and NixOS service module configuration examples                                                                                    | Nix module options or usage patterns change                                                                                  |
| `js/hub/README.md`        | blit-hub signaling relay: protocol, deployment, configuration                                                                                 | Hub protocol, endpoints, deployment config, or environment variables change                                                  |

## Getting started

### Install Nix and direnv

The project uses Nix for all tooling — the Rust toolchain, wasm-pack, pnpm, Node, process-compose, cargo-watch, and everything else. There is no `Makefile` that installs things piecemeal and no list of system dependencies to chase down. One `flake.nix` pins every tool to an exact revision, so every contributor builds with identical versions regardless of OS or distro. If it works in the dev shell, it works in CI.

direnv makes this invisible. Instead of remembering to run `nix develop` every time you `cd` into the repo, direnv evaluates `.envrc`, enters the Nix dev shell, and adds `bin/` to your PATH automatically. Leave the directory and it restores your previous environment. The result: you open a terminal, `cd blit`, and every tool is just there.

**1. Install the [Determinate Nix Installer](https://github.com/DeterminateSystems/nix-installer):**

```bash
curl --proto '=https' --tlsv1.2 -sSf -L https://install.determinate.systems/nix | sh -s -- install
```

This is preferred over the official Nix installer because it enables flakes and the nix command out of the box, configures uninstall support, and works reliably on both macOS and Linux without manual `nix.conf` edits.

**2. [Install direnv](https://direnv.net/docs/installation.html)** and [hook it into your shell](https://direnv.net/docs/hook.html).

**3. Allow the `.envrc`:**

```bash
cd blit
direnv allow
```

The first run downloads and builds the toolchain (cached after that). Once you see `blit dev shell`, you're ready.

### Without direnv

If you'd rather not install direnv, you can enter the dev shell manually:

```bash
nix develop -c $SHELL
```

You'll need to re-run this every time you open a new terminal in the repo.

## Quick start

Once you're in the dev shell, start the full stack with hot-reloading:

```bash
./bin/dev
```

This launches the build, server, gateway, WASM watcher, and Vite dev servers via `process-compose`. See [Dev environment](#dev-environment) for details on what each process does.

## Building and testing

```bash
cargo build                      # debug build, all crates
cargo test --workspace           # all Rust tests
./bin/clippy                     # clippy (CI fails on any warning)
./bin/fmt --check                # formatting check (CI fails on any diff)
./bin/fmt                        # auto-fix formatting
./bin/lint                       # all of the above in one pass
```

`./bin/fmt` runs `cargo fmt` (Rust) and `prettier` (JS/TS/JSON/MD). `./bin/lint` runs fmt check + clippy together — this is what CI runs.

TypeScript (JS workspace — core, react, solid):

```bash
cd js && pnpm install && pnpm test
```

Or individual packages:

```bash
cd js && pnpm --filter @blit-sh/core run test
cd js && pnpm --filter @blit-sh/react run test
```

E2E (Playwright, requires built binaries):

```bash
./bin/e2e
```

CI (`ci.yml`) runs `./bin/lint`, `./bin/tests`, `./bin/e2e`, `./bin/coverage`, and `./bin/dev-check`. These delegate to `nix run .#<task>`, etc.

## Packaging

Every `nix run` target has a corresponding script in `bin/`:

```bash
./bin/build-debs             # .deb packages -> dist/debs/
./bin/build-tarballs         # release tarballs -> dist/tarballs/
./bin/publish-npm-packages   # npm publish @blit-sh/browser, @blit-sh/core, @blit-sh/react, @blit-sh/solid
./bin/publish-crates         # cargo publish
```

`build-debs` and `build-tarballs` accept an optional output directory argument (default `dist/debs` and `dist/tarballs`).
The version and platform are derived from `flake.nix` and the build host.
Linkage is verified at `nix build` time — on Linux the musl binary must have only `libc.so` as a NEEDED library, the glibc binary targets glibc 2.31 via `cargo-zigbuild` (all other deps statically linked), and macOS binaries must not reference nix-store dylibs.

Individual packages can also be built directly:

```bash
nix build .#blit
```

There is no `rustfmt.toml` or `.clippy.toml` — default rustfmt, prettier, and `clippy -D warnings` are the style enforcement. `./bin/fmt` runs both formatters in one pass.

## Dev environment

`./bin/dev` starts the full stack with hot-reloading via `process-compose`:

| Process        | What it does                                                               | Default port / socket |
| -------------- | -------------------------------------------------------------------------- | --------------------- |
| `build`        | One-shot `cargo build -p blit-cli --profile profiling`; restart to rebuild | n/a                   |
| `browser-wasm` | Watches `crates/browser/src` + `crates/remote/src`, rebuilds WASM          | n/a                   |
| `server`       | Runs `blit server`, auto-restarts when the binary changes                  | `/tmp/blit-dev.sock`  |
| `gateway`      | Runs `blit gateway` (pass=`dev`), auto-restarts when binary changes        | `127.0.0.1:10001`     |
| `ui`           | Vite dev server for `js/ui/`                                               | `127.0.0.1:10000`     |
| `website`      | Astro dev server for `js/website/`                                         | `127.0.0.1:10002`     |

### Running multiple dev stacks

Every port and socket path is derived from `DEV_INSTANCE` (default `0`). Each instance gets a block of ports at `10000 + (N * 5)`:

| Instance | UI    | Gateway | Website |
| -------- | ----- | ------- | ------- |
| 0        | 10000 | 10001   | 10002   |
| 1        | 10005 | 10006   | 10007   |
| 2        | 10010 | 10011   | 10012   |

```bash
DEV_INSTANCE=1 ./bin/dev   # second stack on 10005-10007
```

`bin/dev` prints the concrete addresses on startup. `DEV_INSTANCE` is intentionally unprefixed: blit strips most `BLIT_*` variables from child terminals, but passes everything else through. This means `DEV_INSTANCE` propagates into nested shells so you always know which instance you're inside and can pick a different one. You can also override individual values:

| Variable             | Default (instance 0) | Description                   |
| -------------------- | -------------------- | ----------------------------- |
| `DEV_INSTANCE`       | `0`                  | Instance number (0, 1, 2, …)  |
| `BLIT_DEV_SOCK`      | `/tmp/blit-dev.sock` | blit server Unix socket       |
| `BLIT_DEV_UI_PORT`   | `10000`              | Vite UI dev-server port       |
| `BLIT_DEV_GW_PORT`   | `10001`              | blit gateway port             |
| `BLIT_DEV_SITE_PORT` | `10002`              | Astro website dev-server port |

## Project structure

Most Rust crates are one or two source files. The CLI crate (`blit-cli`) is split into six and `blit-webrtc-forwarder` uses a multi-file module tree.

| File                                     | Role                                                                                                                  |
| ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| `crates/server/src/lib.rs`               | PTY host: fork/exec, frame scheduling, protocol handlers, congestion control, compositor integration                  |
| `crates/server/src/surface_encoder.rs`   | Surface video encoding: AV1 (rav1e), H.264 (openh264, VA-API, NVENC)                                                  |
| `crates/server/src/vaapi_encode.rs`      | Direct VA-API H.264 and AV1 encoding (dlopen, no FFmpeg)                                                              |
| `crates/server/src/nvenc_encode.rs`      | Direct NVENC H.264 and AV1 encoding via CUDA + NVENC SDK (dlopen, no FFmpeg)                                          |
| `crates/server/src/gpu_libs.rs`          | Runtime dlopen loaders for libva, NVENC, GBM shared across encoders                                                   |
| `crates/server/src/audio.rs`             | Audio capture pipeline: PipeWire daemon spawn, in-process capture via `audio_pw`, Opus encoding                       |
| `crates/server/src/audio_pw.rs`          | In-process libpipewire-0.3 capture client (runtime `dlopen`), replaces the former pw-cat subprocess                   |
| `crates/remote/src/lib.rs`               | Wire protocol: constants, message builders/parsers, `FrameState`/`TerminalState`, cell encoding, text extraction      |
| `crates/compositor/src/imp.rs`           | Experimental headless Wayland compositor (wayland-server): surface tracking, input forwarding, protocol delegates     |
| `crates/compositor/src/render.rs`        | Surface compositing: `SurfaceMeta` and layer collection (`collect_gpu_layers`) for the GPU renderer                   |
| `crates/compositor/src/vulkan_render.rs` | Vulkan GPU compositor: dlopen libvulkan.so via ash, DMA-BUF import, multi-layer compositing                           |
| `crates/webrtc-forwarder/src/`           | WebRTC forwarder (6 files: signaling, ICE, TURN, peer management)                                                     |
| `crates/cli/src/agent.rs`                | Agent subcommands: `list`, `start`, `show`, `history`, `send`, `close`, `surfaces`, `capture`, `click`, `key`, `type` |
| `crates/cli/src/main.rs`                 | Dispatch, embedded server/gateway                                                                                     |
| `crates/cli/src/cli.rs`                  | Clap struct definitions                                                                                               |
| `crates/cli/src/interactive.rs`          | Browser mode                                                                                                          |
| `crates/cli/src/transport.rs`            | Transport abstraction (Unix/TCP/SSH/WebRTC)                                                                           |
| `crates/cli/src/learn.md`                | CLI reference text printed by `blit learn`                                                                            |
| `crates/browser/src/lib.rs`              | WASM: applies frame diffs, produces WebGL vertex data, glyph atlas                                                    |
| `crates/alacritty-driver/src/lib.rs`     | Terminal parsing wrapper around `alacritty_terminal`                                                                  |
| `crates/gateway/src/lib.rs`              | WebSocket/WebTransport proxy, multi-destination routing                                                               |
| `crates/fonts/src/lib.rs`                | Font discovery and TTF/OTF parsing                                                                                    |
| `crates/webserver/src/lib.rs`            | Shared axum HTTP helpers                                                                                              |
| `crates/webserver/src/config.rs`         | Server configuration types                                                                                            |

### Non-Rust code

| Directory                    | What                                                                                                                            |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `js/core/`                   | `@blit-sh/core` npm package — framework-agnostic core: transports, BSP layout, protocol, WebGL renderer, `BlitTerminalSurface`  |
| `js/react/`                  | `@blit-sh/react` npm package — thin React bindings wrapping `BlitTerminalSurface` from core. Tests in `js/react/src/__tests__/` |
| `js/solid/`                  | `@blit-sh/solid` npm package — thin Solid bindings wrapping `BlitTerminalSurface` from core                                     |
| `js/ui/`                     | Vite + Solid SPA — browser UI with BSP tiled layouts, overlays, status bar                                                      |
| `js/website/`                | Marketing/docs website (Astro + Solid)                                                                                          |
| `js/hub/`                    | Signaling relay server (Bun, deployed to Fly.io). See [js/hub/README.md](js/hub/README.md)                                      |
| `e2e/`                       | Playwright tests against the full stack (6 spec files)                                                                          |
| `examples/`                  | fd-channel examples in Python and Bun                                                                                           |
| `nix/`                       | Nix packaging: `common.nix` (toolchain), `packages.nix` (build defs), `tasks.nix` (CI tasks), NixOS/Darwin modules              |
| `systemd/`                   | Socket-activated unit files (user-level and system-level templates) and service units                                           |
| `crates/cli/src/generate.rs` | Man pages and shell completions generated from clap definitions via `blit generate <dir>`                                       |
| `bin/`                       | Shell scripts wrapping `nix run` tasks plus release scripts (`release-prepare`, `release-tag`, `prepare-release`)               |

## Code conventions

**Flat crate layout.** Don't introduce deep `mod` trees. If a crate grows, add files at the same level (like `cli/src/agent.rs`) and `mod` them from the root. `blit-webrtc-forwarder` is the one exception with a multi-file module tree.

**Wire protocol changes** touch multiple layers. A new message type requires:

1. Constants and `ServerMsg`/parse case in `remote/src/lib.rs`
2. A `msg_*()` builder function in `remote/src/lib.rs`
3. Server handler in `server/src/lib.rs`
4. Client-side handling in `cli/src/agent.rs` (agent subcommands) and/or `cli/src/interactive.rs` (TUI)
5. Update the protocol tables in [ARCHITECTURE.md](ARCHITECTURE.md)

**Core-first JS architecture.** `@blit-sh/core` is the framework-agnostic shared package — transports, workspace state, BSP layout, keyboard shortcuts, renderers, and protocol logic all live here. `@blit-sh/react` and `@blit-sh/solid` are thin glue layers that wrap core primitives in framework-specific hooks/components (React hooks, Solid signals). When adding new UI-related logic (keyboard shortcuts, session cycling, layout management, etc.), implement it in `@blit-sh/core` and expose a framework-agnostic API. The framework packages should only bridge reactive state to core interfaces and bindDOM events — never duplicate decision logic.

**Tests live next to the code.** `server/src/lib.rs` has a `#[cfg(test)]` module at the bottom. `cli/src/agent.rs` has its own test module with `MockServer`/`MockPty` — an in-process test harness using Unix socket pairs. Core tests are in `core/src/__tests__/`, React tests in `react/src/__tests__/`.

**Release profile** uses `opt-level = 3`, LTO, `codegen-units = 1`, and `panic = "abort"`. On Linux, two release tarballs are produced: a glibc variant (all deps statically linked, glibc 2.31+ via zig cc, dlopen works for GPU) and a musl variant (all deps statically linked except musl libc) for Alpine. Both are single-binary tarballs. Nix verifies linkage at build time.

## Versioning and releases

All workspace crates, `js/core/package.json`, `js/react/package.json`, `js/solid/package.json`, and `nix/common.nix` share a single version number. The JS packages live in a pnpm workspace rooted at `js/` with a shared `js/pnpm-lock.yaml`.

Releases go through a three-step process:

1. **Prepare**: `./bin/release-prepare 0.12.0` runs `bin/prepare-release` locally (version bumping, validation, tests), pushes a `release/<version>` branch, and opens a PR against `main`.
2. **Tag**: After the PR is merged, run `./bin/release-tag 0.12.0` to create a signed tag and push it to origin.
3. The `release.yml` workflow triggers on the `v*` tag push. It first verifies the tag signature via the GitHub API — unsigned or unverified tags fail the workflow immediately.

CI on the verified tag builds debs/tarballs, publishes to crates.io and npm, updates the Homebrew tap, and deploys the APT repo.

## Guardrails

- `./bin/lint` is the CI gate (fmt + clippy). Run `./bin/fmt` to auto-fix formatting and `./bin/clippy` to check clippy warnings before pushing.
- The WASM crate (`crates/browser/`) targets `wasm32-unknown-unknown` — don't add dependencies that pull in `std::net`, `std::fs`, etc.
- `crates/browser/pkg/` is gitignored. It must be built locally (`./bin/build-browser`) before the UI or React tests will work.
- The server uses raw `libc` calls (`openpty`, `waitpid`, `kill`, `ioctl`) — changes to PTY lifecycle code need careful attention to signal safety and fd leaks.
- The background zombie reaper (`waitpid(-1, ..., WNOHANG)` every 5s in the server) can race with `cleanup_pty`'s `waitpid` for the specific child. This is intentional — `cleanup_pty` uses `WNOHANG` so it doesn't block if the reaper already collected the child.

## Wayland compositor (experimental)

The experimental headless Wayland compositor (`crates/compositor/`) is `#[cfg(target_os = "linux")]` only — it compiles to a stub on macOS and Windows. It uses `wayland-server` directly and runs as a single shared thread across all terminals.

### How it works

1. When the first PTY is created, `ensure_compositor()` spawns a compositor thread with a calloop event loop.
2. The compositor creates a Wayland listening socket (`/tmp/wayland-N`) and sets `WAYLAND_DISPLAY` + `XDG_RUNTIME_DIR` in the PTY child environment.
3. GUI apps launched inside any terminal connect to this socket and create `xdg_toplevel` windows.
4. On each `wl_surface.commit`, the compositor uploads the buffer as a persistent GPU texture (SHM is uploaded, DMA-BUF is imported via Vulkan), composites the surface tree, and sends a `CompositorEvent::SurfaceCommit` with the composited `PixelData` to the server.
5. The server encodes the pixel data as H.264 or AV1 (zero-copy from a VA surface when available, or from BGRA staging) and stores the encoded frame in `last_frames`. The tick loop sends frames to connected browser clients using the same pacing/congestion-control system as terminal updates.
6. Browser clients decode frames via WebCodecs and render to a `<canvas>`.

### Key data flow

```
Wayland app → compositor thread → CompositorEvent::SurfaceCommit
  → server tick: SurfaceEncoder::encode() → last_frames
  → server tick: pacing check → msg_surface_frame → gateway WS → browser
  → browser: SurfaceStore → VideoDecoder → canvas
```

### Surface encoding

`crates/server/src/surface_encoder.rs` wraps seven backends behind a common `SurfaceEncoder` interface:

- **NVENC AV1 / H.264** — NVIDIA GPU hardware encoding via CUDA + NVENC SDK (dlopen, no FFmpeg)
- **AV1 VA-API** — Intel/AMD GPU hardware encoding via libva directly (dlopen, no FFmpeg)
- **H.264 VA-API** — Intel/AMD GPU hardware encoding via libva directly (dlopen, no FFmpeg)
- **AV1 (rav1e)** — software, handles odd dimensions
- **H.264 software (openh264)** — software fallback, max 3840x2160

`BLIT_SURFACE_ENCODERS` is a comma-separated priority list. The server tries each in order and uses the first that succeeds. Default: `av1-nvenc,h264-nvenc,av1-vaapi,h264-vaapi,h264-software,av1-software`. `BLIT_SURFACE_QUALITY` (low/medium/high/lossless) controls rav1e speed/quantizer presets. `BLIT_VAAPI_DEVICE` selects the VA-API render node (default `/dev/dri/renderD128`). `BLIT_CUDA_DEVICE` selects the CUDA device ordinal for NVENC (default `0`).

### Testing surfaces without a browser

```bash
blit -s /tmp/blit-dev.sock start bash
blit -s /tmp/blit-dev.sock send 1 'foot &\n'
blit -s /tmp/blit-dev.sock surfaces          # list surfaces (TSV)
blit -s /tmp/blit-dev.sock capture 1         # screenshot → surface-1.png
blit -s /tmp/blit-dev.sock click 1 100 50    # click at (x, y)
blit -s /tmp/blit-dev.sock key 1 Return      # press a key
blit -s /tmp/blit-dev.sock type 1 'hello'    # type text
```

### Wire protocol surface messages

Surface, clipboard, and audio messages use opcodes `0x20`+ (see the constants in `crates/remote/src/lib.rs`). A new client receives `S2C_SURFACE_CREATED` for each existing surface during the initial state sync, followed by keyframes via the normal pacing loop.
