Reading material:

- AGENTS.md
- ARCHITECTURE.md
- CONTRIBUTING.md
- EMBEDDING.md
- README.md
- SERVICES.md
- SKILL.md
- UNSAFE.md
- js/blit-hub/README.md

# Documentation maintenance guide

When making changes, update the relevant docs in the same PR.

| Document | Scope | Update when... |
| --- | --- | --- |
| `README.md` | User-facing overview: installation, usage, features | CLI flags, install methods, or supported platforms change |
| `ARCHITECTURE.md` | System internals: data flow, crate responsibilities, transport layers, rendering pipeline | Crates are added/removed/renamed, data flow between components changes, or new transport/rendering mechanisms are introduced |
| `CONTRIBUTING.md` | Developer workflow: building, testing, code conventions, project structure | Build steps, test commands, directory layout, or dev tooling changes |
| `SERVICES.md` | Hosted services and CI/CD: install.blit.sh, hub.blit.sh, GitHub Actions workflows, release lifecycle, secrets | CI jobs are added/removed/changed, deployment targets change, new secrets are introduced, or the release process is modified |
| `EMBEDDING.md` | Embedding blit in other apps: React components (`@blit-sh/react`), embedding `blit-server` as a library | Public embedding APIs, component props, or integration patterns change |
| `SKILL.md` | LLM agent skill definition: how to drive terminal sessions via CLI subcommands | CLI subcommands used for programmatic terminal control change |
| `UNSAFE.md` | Unsafe Rust code audit: which crates use `unsafe`, why, and what invariants they rely on | Unsafe code is added, removed, or its safety invariants change |
| `AGENTS.md` | Agent-specific setup: nix dev shell, toolchain, quick reference | Dev shell packages change, agent-facing workflow instructions change |
| `js/blit-hub/README.md` | blit-hub signaling relay: protocol, deployment, configuration | Hub protocol, endpoints, deployment config, or environment variables change |

# Agent instructions

This file helps LLM agents work in the blit repository. For full contributor docs see [CONTRIBUTING.md](CONTRIBUTING.md).

## Toolchain setup

All build tools (Rust, wasm-pack, pnpm, Node, cargo-watch, process-compose, etc.) are provided by a Nix dev shell. **Nothing is installed globally.** If you get "command not found" for `cargo`, `pnpm`, `wasm-pack`, or any other tool, you need to enter the dev shell first:

```bash
nix develop -c $SHELL
```

This is the single prerequisite. Once inside the shell every tool listed in `flake.nix` is on your `PATH`.

## Quick reference

| Task | Command |
| --- | --- |
| Build (debug) | `cargo build` |
| Build (release) | `cargo build --release` |
| Run all Rust tests | `cargo test --workspace` |
| Lint (clippy) | `cargo clippy --workspace -- -D warnings` |
| Format check | `cargo fmt -- --check` |
| Format fix | `cargo fmt` |
| JS tests | `cd js && pnpm install && pnpm test` |
| Full dev stack | `./bin/dev` |
| E2E tests | `./bin/e2e` |
| CI lint suite | `./bin/lint` |
| CI test suite | `./bin/tests` |

## What the dev shell provides

The Nix dev shell (`nix develop`) pins every tool to an exact revision via `flake.nix`. Key packages:

- **Rust** stable toolchain with targets `wasm32-unknown-unknown`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, plus `clippy` and `llvm-tools`
- **WASM**: `wasm-pack`, `wasm-bindgen-cli`, `binaryen`
- **JS**: `nodejs`, `pnpm`, `bun`
- **Cargo extras**: `cargo-watch`, `cargo-edit`, `cargo-flamegraph`, `cargo-llvm-cov`
- **Other**: `process-compose`, `flyctl`, `scdoc`, `samply`, `curl`, static musl C compiler

## CI checks

PRs must pass before merging:

| Check | What |
| --- | --- |
| `e2e` | Playwright end-to-end tests |
| `dev-check` | Full-stack smoke test |
| `test (macos-latest)` | Rust + JS tests on macOS |
| `test (ubuntu-latest)` | Rust + JS tests on Ubuntu |

`cargo clippy -- -D warnings` is the lint gate. Fix all warnings before pushing.

## Common pitfalls

- The WASM crate (`crates/browser/`) targets `wasm32-unknown-unknown` -- do not add dependencies that use `std::net`, `std::fs`, etc.
- `crates/browser/pkg/` is gitignored and must be built locally (`./bin/build-browser`) before web-app or React tests work.
- All workspace crates, JS packages, and `nix/common.nix` share a single version number. Use `./bin/release <version>` to bump atomically.
