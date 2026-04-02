Reading material:

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
| `js/blit-hub/README.md` | blit-hub signaling relay: protocol, deployment, configuration | Hub protocol, endpoints, deployment config, or environment variables change |
