# Contributing to blit

This document helps LLM agents (and humans) contribute to the blit codebase. It covers the development workflow, code conventions, and project structure. For the system architecture, see [ARCHITECTURE.md](ARCHITECTURE.md). For user-facing documentation, see [README.md](README.md).

## Getting started

The project uses Nix for all tooling. Enter the dev shell, then everything works:

```bash
nix develop -c $SHELL
```

If you have direnv, `.envrc` handles this automatically and adds `bin/` to PATH.

## Building and testing

```bash
cargo build                      # debug build, all crates
cargo test --workspace           # all Rust tests
cargo clippy --workspace -- -D warnings   # lint (CI fails on any warning)
cargo fmt -- --check             # formatting (CI fails on any diff)
```

Run `cargo fmt` to auto-fix formatting before committing.

TypeScript (React library):

```bash
cd react && pnpm install && pnpm vitest run
```

E2E (Playwright, requires built binaries):

```bash
./bin/e2e
```

CI runs all three: `./bin/lint`, `./bin/tests`, `./bin/e2e`. These delegate to `nix run .#lint`, etc.

## Packaging

Every `nix run` target has a corresponding script in `bin/`:

```bash
./bin/build-debs             # .deb packages -> dist/debs/
./bin/build-tarballs         # static tarballs -> dist/tarballs/
./bin/browser-publish        # npm publish blit-browser
./bin/react-publish          # npm publish blit-react
```

`build-debs` and `build-tarballs` accept an optional output directory argument (default `dist/debs` and `dist/tarballs`).
The version and platform are derived from `flake.nix` and the build host.
Linkage is verified at `nix build` time — Linux binaries must be statically linked and macOS binaries must not reference nix-store dylibs.

Individual packages can also be built directly:

```bash
nix build .#blit-server      # or blit-cli, blit-gateway
nix build .#blit-server-deb  # or blit-cli-deb, blit-gateway-deb
```

There is no `rustfmt.toml` or `.clippy.toml` — default rustfmt and `clippy -D warnings` are the only style enforcement.

## Dev environment

`./bin/dev` starts the full stack with hot-reloading via `process-compose`:

| Process        | What it does                                                | Port / socket        |
| -------------- | ----------------------------------------------------------- | -------------------- |
| `browser-wasm` | Watches `browser/src` + `remote/src`, rebuilds WASM         | n/a                  |
| `server`       | `cargo watch` running `blit-server --release`               | `/tmp/blit-dev.sock` |
| `gateway`      | `cargo watch` running `blit-gateway --release` (pass=`dev`) | `127.0.0.1:3266`     |
| `web-app`      | Vite dev server for `web-app/`                              | printed by Vite      |

## Project structure

Every Rust crate is a single source file (`lib.rs` or `main.rs`) except `blit-cli` which is split into four and `blit-demo` which has extra binaries in `src/bin/`. There are no multi-level module trees.

| File                          | Lines | Role                                                                                                             |
| ----------------------------- | ----- | ---------------------------------------------------------------------------------------------------------------- |
| `server/src/main.rs`          | ~4400 | PTY host: fork/exec, frame scheduling, protocol handlers, congestion control                                     |
| `remote/src/lib.rs`           | ~3000 | Wire protocol: constants, message builders/parsers, `FrameState`/`TerminalState`, cell encoding, text extraction |
| `cli/src/interactive.rs`      | ~1650 | Console TUI and browser mode                                                                                     |
| `browser/src/lib.rs`          | ~1100 | WASM: applies frame diffs, produces WebGL vertex data, glyph atlas                                               |
| `alacritty-driver/src/lib.rs` | ~1100 | Terminal parsing wrapper around `alacritty_terminal`                                                             |
| `cli/src/agent.rs`            | ~870  | Agent subcommands: `list`, `start`, `show`, `history`, `send`, `close`                                           |
| `demo/src/main.rs`            | ~780  | Demo programs                                                                                                    |
| `gateway/src/main.rs`         | ~700  | WebSocket/WebTransport proxy                                                                                     |
| `fonts/src/lib.rs`            | ~600  | Font discovery and TTF/OTF parsing                                                                               |
| `cli/src/main.rs`             | ~185  | Clap structs and dispatch                                                                                        |
| `cli/src/transport.rs`        | ~130  | Transport abstraction (Unix/TCP/SSH)                                                                             |
| `webserver/src/lib.rs`        | ~90   | Shared axum HTTP helpers                                                                                         |

### Non-Rust code

| Directory  | What                                                                                                               |
| ---------- | ------------------------------------------------------------------------------------------------------------------ |
| `react/`   | `blit-react` npm package — React library with hooks, transports, WebGL renderer. Tests in `react/src/__tests__/`.  |
| `web-app/` | Vite + React SPA — reference browser UI with BSP tiled layouts, overlays, status bar                               |
| `e2e/`     | Playwright tests against the full stack (6 spec files)                                                             |
| `nix/`     | Nix packaging: `common.nix` (toolchain), `packages.nix` (build defs), `tasks.nix` (CI tasks), NixOS/Darwin modules |
| `systemd/` | Socket-activated unit files (user-level and system-level templates)                                                |
| `man/`     | scdoc man pages for `blit`, `blit-server`, `blit-gateway`                                                          |
| `bin/`     | Shell scripts wrapping `nix run` tasks plus the `release` orchestrator                                             |

## Code conventions

**Single-file crates.** Don't introduce `mod` trees. If a crate grows, add a second file at the same level (like `cli/src/agent.rs`) and `mod` it from the root.

**Wire protocol changes** touch multiple layers. A new message type requires:

1. Constants and `ServerMsg`/parse case in `remote/src/lib.rs`
2. A `msg_*()` builder function in `remote/src/lib.rs`
3. Server handler in `server/src/main.rs`
4. Client-side handling in `cli/src/agent.rs` (agent subcommands) and/or `cli/src/interactive.rs` (TUI)
5. Update the protocol tables in [ARCHITECTURE.md](ARCHITECTURE.md)

**Tests live next to the code.** `server/src/main.rs` has a `#[cfg(test)]` module at the bottom. `cli/src/agent.rs` has its own test module with `MockServer`/`MockPty` — an in-process test harness using Unix socket pairs. React tests are in `react/src/__tests__/`.

**Release profile** uses `opt-level = "s"` and LTO. Linux binaries are statically linked via musl; Nix verifies this at build time.

## Versioning and releases

All workspace crates, `react/package.json`, and `nix/common.nix` share a single version number. The `bin/release` script bumps all of them atomically:

```bash
./bin/release 0.12.0
```

This validates version consistency, bumps all files, runs `cargo test -p blit-server`, and commits. CI on the resulting `v*` tag builds debs/tarballs, publishes to crates.io and npm, updates the Homebrew tap, and deploys the APT repo.

## Guardrails

- `cargo clippy -- -D warnings` is the CI gate. Fix all warnings before pushing.
- The WASM crate (`browser/`) targets `wasm32-unknown-unknown` — don't add dependencies that pull in `std::net`, `std::fs`, etc.
- `browser/pkg/` is gitignored. It must be built locally (`./bin/build-browser`) before the web-app or React tests will work.
- The server uses raw `libc` calls (`openpty`, `waitpid`, `kill`, `ioctl`) — changes to PTY lifecycle code need careful attention to signal safety and fd leaks.
- The background zombie reaper (`waitpid(-1, ..., WNOHANG)` every 5s in the server) can race with `cleanup_pty`'s `waitpid` for the specific child. This is intentional — `cleanup_pty` uses `WNOHANG` so it doesn't block if the reaper already collected the child.
