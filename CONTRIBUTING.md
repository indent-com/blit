Reading material:

- ARCHITECTURE.md
- EMBEDDING.md
- README.md
- SERVICES.md
- SKILL.md
- UNSAFE.md
- nix/README.md
- js/blit-hub/README.md

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
| `EMBEDDING.md`            | Embedding blit in other apps: React components (`@blit-sh/react`), embedding `blit-server` as a library                                       | Public embedding APIs, component props, or integration patterns change                                                       |
| `SKILL.md`                | LLM agent skill definition: install instructions and pointer to `blit learn`. Deployed to `install.blit.sh/SKILL.md` by the release workflow. | Install methods change or the `learn` subcommand output changes                                                              |
| `crates/cli/src/learn.md` | Full CLI reference printed by `blit learn`: usage patterns, subcommand details, transport options, escapes                                    | CLI subcommands, flags, output conventions, or transport options change                                                      |
| `UNSAFE.md`               | Unsafe Rust code audit: which crates use `unsafe`, why, and what invariants they rely on                                                      | Unsafe code is added, removed, or its safety invariants change                                                               |
| `nix/README.md`           | nix-darwin and NixOS service module configuration examples                                                                                    | Nix module options or usage patterns change                                                                                  |
| `js/blit-hub/README.md`   | blit-hub signaling relay: protocol, deployment, configuration                                                                                 | Hub protocol, endpoints, deployment config, or environment variables change                                                  |

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

This launches the server, gateway, WASM watcher, and Vite dev servers via `process-compose`. See [Dev environment](#dev-environment) for details on what each process does.

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

CI runs `./bin/lint`, `./bin/tests`, and `./bin/e2e`. These delegate to `nix run .#<task>`, etc.

## Packaging

Every `nix run` target has a corresponding script in `bin/`:

```bash
./bin/build-debs             # .deb packages -> dist/debs/
./bin/build-tarballs         # static tarballs -> dist/tarballs/
./bin/publish-npm-packages   # npm publish @blit-sh/browser, @blit-sh/core, @blit-sh/react, @blit-sh/solid, @blit-sh/solid
./bin/publish-crates         # cargo publish
```

`build-debs` and `build-tarballs` accept an optional output directory argument (default `dist/debs` and `dist/tarballs`).
The version and platform are derived from `flake.nix` and the build host.
Linkage is verified at `nix build` time — Linux binaries must be statically linked and macOS binaries must not reference nix-store dylibs.

Individual packages can also be built directly:

```bash
nix build .#blit-server      # or blit-cli, blit-gateway
nix build .#blit-server-deb  # or blit-cli-deb, blit-gateway-deb
```

There is no `rustfmt.toml` or `.clippy.toml` — default rustfmt, prettier, and `clippy -D warnings` are the style enforcement. `./bin/fmt` runs both formatters in one pass.

## Dev environment

`./bin/dev` starts the full stack with hot-reloading via `process-compose`:

| Process        | What it does                                                      | Port / socket        |
| -------------- | ----------------------------------------------------------------- | -------------------- |
| `browser-wasm` | Watches `crates/browser/src` + `crates/remote/src`, rebuilds WASM | n/a                  |
| `server`       | `cargo watch` running `blit-server --release`                     | `/tmp/blit-dev.sock` |
| `gateway`      | `cargo watch` running `blit-gateway --release` (pass=`dev`)       | `127.0.0.1:3266`     |
| `web-app`      | Vite dev server for `js/web-app/`                                 | printed by Vite      |
| `website`      | Vite dev server for `js/website/`                                 | printed by Vite      |

## Project structure

Most Rust crates are one or two source files. `blit-cli` is split into five and `blit-webrtc-forwarder` uses a multi-file module tree.

| File                                 | Lines | Role                                                                                                             |
| ------------------------------------ | ----- | ---------------------------------------------------------------------------------------------------------------- |
| `crates/server/src/lib.rs`           | ~4400 | PTY host: fork/exec, frame scheduling, protocol handlers, congestion control                                     |
| `crates/remote/src/lib.rs`           | ~3100 | Wire protocol: constants, message builders/parsers, `FrameState`/`TerminalState`, cell encoding, text extraction |
| `crates/webrtc-forwarder/src/`       | ~2000 | WebRTC forwarder (7 files: signaling, ICE, TURN, peer management)                                                |
| `crates/cli/src/interactive.rs`      | ~1700 | Console TUI and browser mode                                                                                     |
| `crates/cli/src/agent.rs`            | ~1300 | Agent subcommands: `list`, `start`, `show`, `history`, `send`, `close`                                           |
| `crates/browser/src/lib.rs`          | ~1200 | WASM: applies frame diffs, produces WebGL vertex data, glyph atlas                                               |
| `crates/alacritty-driver/src/lib.rs` | ~1200 | Terminal parsing wrapper around `alacritty_terminal`                                                             |
| `crates/gateway/src/main.rs`         | ~750  | WebSocket/WebTransport proxy                                                                                     |
| `crates/fonts/src/lib.rs`            | ~660  | Font discovery and TTF/OTF parsing                                                                               |
| `crates/cli/src/main.rs`             | ~490  | Clap structs and dispatch                                                                                        |
| `crates/cli/src/learn.md`            | ~155  | CLI reference text printed by `blit learn`                                                                       |
| `crates/webserver/src/lib.rs`        | ~120  | Shared axum HTTP helpers                                                                                         |
| `crates/webserver/src/config.rs`     | ~210  | Server configuration types                                                                                       |
| `crates/cli/src/transport.rs`        | ~190  | Transport abstraction (Unix/TCP/SSH/WebRTC)                                                                      |

### Non-Rust code

| Directory      | What                                                                                                                            |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `js/core/`     | `@blit-sh/core` npm package — framework-agnostic core: transports, BSP layout, protocol, WebGL renderer, `BlitTerminalSurface`  |
| `js/react/`    | `@blit-sh/react` npm package — thin React bindings wrapping `BlitTerminalSurface` from core. Tests in `js/react/src/__tests__/` |
| `js/solid/`    | `@blit-sh/solid` npm package — thin Solid bindings wrapping `BlitTerminalSurface` from core                                     |
| `js/web-app/`  | Vite + React SPA — reference browser UI with BSP tiled layouts, overlays, status bar                                            |
| `js/website/`  | Marketing/docs website (Vite + SSR with prerendering)                                                                           |
| `js/blit-hub/` | Signaling relay server (Bun, deployed to Fly.io). See [js/blit-hub/README.md](js/blit-hub/README.md)                            |
| `e2e/`         | Playwright tests against the full stack (6 spec files)                                                                          |
| `examples/`    | fd-channel examples in Python and Bun                                                                                           |
| `nix/`         | Nix packaging: `common.nix` (toolchain), `packages.nix` (build defs), `tasks.nix` (CI tasks), NixOS/Darwin modules              |
| `systemd/`     | Socket-activated unit files (user-level and system-level templates) and service units                                           |
| `man/`         | scdoc man pages for `blit`, `blit-server`, `blit-gateway`, `blit-webrtc-forwarder`                                              |
| `bin/`         | Shell scripts wrapping `nix run` tasks plus release scripts (`release-prepare`, `release-tag`, `prepare-release`)               |

## Code conventions

**Flat crate layout.** Don't introduce deep `mod` trees. If a crate grows, add files at the same level (like `cli/src/agent.rs`) and `mod` them from the root. `blit-webrtc-forwarder` is the one exception with a multi-file module tree.

**Wire protocol changes** touch multiple layers. A new message type requires:

1. Constants and `ServerMsg`/parse case in `remote/src/lib.rs`
2. A `msg_*()` builder function in `remote/src/lib.rs`
3. Server handler in `server/src/main.rs`
4. Client-side handling in `cli/src/agent.rs` (agent subcommands) and/or `cli/src/interactive.rs` (TUI)
5. Update the protocol tables in [ARCHITECTURE.md](ARCHITECTURE.md)

**Tests live next to the code.** `server/src/lib.rs` has a `#[cfg(test)]` module at the bottom. `cli/src/agent.rs` has its own test module with `MockServer`/`MockPty` — an in-process test harness using Unix socket pairs. Core tests are in `core/src/__tests__/`, React tests in `react/src/__tests__/`.

**Release profile** uses `opt-level = 3`, LTO, `codegen-units = 1`, and `panic = "abort"`. Linux binaries are statically linked via musl; Nix verifies this at build time.

## Versioning and releases

All workspace crates, `js/core/package.json`, `js/react/package.json`, `js/solid/package.json`, and `nix/common.nix` share a single version number. The JS packages live in a pnpm workspace rooted at `js/` with a shared `js/pnpm-lock.yaml`.

Releases go through a three-step process:

1. **Prepare**: `./bin/release-prepare 0.12.0` runs `bin/prepare-release` locally (version bumping, validation, tests), pushes a `release/<version>` branch, and opens a PR against `main`.
2. **Tag**: After the PR is merged, run `./bin/release-tag 0.12.0` to create a signed tag and push it to origin.
3. The `release.yml` workflow triggers on the `v*` tag push. It first verifies the tag signature via the GitHub API — unsigned or unverified tags fail the workflow immediately.

CI on the verified tag builds debs/tarballs, publishes to crates.io and npm, updates the Homebrew tap, and deploys the APT repo.

## CI checks

PRs must be reviewed and pass the following CI checks before merging:

| Check                  | What it covers                                                                                                                             |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `lint`                 | Formatting (`cargo fmt --check` + `prettier --check`) and clippy (`./bin/lint`)                                                            |
| `e2e`                  | Playwright end-to-end tests (`./bin/e2e`)                                                                                                  |
| `dev-check`            | Full-stack smoke test: starts dev services via `process-compose`, waits for health, exercises the CLI, then tears down (`./bin/dev-check`) |
| `test (ubuntu-latest)` | Rust and JS test suite on Ubuntu                                                                                                           |

## Guardrails

- `./bin/lint` is the CI gate (fmt + clippy). Run `./bin/fmt` to auto-fix formatting and `./bin/clippy` to check clippy warnings before pushing.
- The WASM crate (`crates/browser/`) targets `wasm32-unknown-unknown` — don't add dependencies that pull in `std::net`, `std::fs`, etc.
- `crates/browser/pkg/` is gitignored. It must be built locally (`./bin/build-browser`) before the web-app or React tests will work.
- The server uses raw `libc` calls (`openpty`, `waitpid`, `kill`, `ioctl`) — changes to PTY lifecycle code need careful attention to signal safety and fd leaks.
- The background zombie reaper (`waitpid(-1, ..., WNOHANG)` every 5s in the server) can race with `cleanup_pty`'s `waitpid` for the specific child. This is intentional — `cleanup_pty` uses `WNOHANG` so it doesn't block if the reaper already collected the child.
