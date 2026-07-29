---
name: blit
description: >
  Terminal multiplexer, remote filesystem and git client, language-server
  bridge, and experimental Wayland compositor. Use when you need to create,
  control, or read from terminals via the CLI; read, write, find, or grep
  files on a server; inspect a repository's status, log, or diffs; ask a
  language server for definitions, references, hover, completions, signature
  help, symbols, diagnostics, or a rename plan; read or write the server's
  key/value store; or run and interact with GUI applications. Covers
  starting PTYs, sending keystrokes, reading output, checking exit status,
  managing terminal lifecycle, attaching a local terminal to a remote one,
  searching file contents across a tree, and driving graphical windows through the experimental headless Wayland
  compositor (listing surfaces, capturing screenshots, clicking, typing,
  and sending key presses).
---

# blit CLI

blit is a terminal multiplexer and experimental headless Wayland compositor. Every terminal can run both CLI programs (via PTYs) and GUI applications (via the built-in compositor). Surfaces are video-encoded and streamed to browsers; the CLI gives programmatic control over both terminals and graphical windows.

Everything the CLI does works locally or against a remote over one wire —
`--on ssh:host`, `--on share:passphrase`, or a named remote — including the
filesystem, git, and language-server commands. Beyond terminals it can:

- **Files** — `blit fs cat|find|grep|write|sync|mkdir|mv|rm|ln`. `grep`
  searches file _contents_ server-side across a tree (literal or regex,
  case, whole word, `.gitignore`-aware); `find` is a fuzzy path search.
- **Git** — `blit git status|log|diff|show|ls-tree|ls-files|merge-base`,
  including ranges, `--follow`, and `--watch` to stream changes.
- **Code intelligence** — `blit lsp def|refs|hover|complete|signature|symbols|diag|rename`,
  backed by real language servers running next to the code.
- **Key/value store** — `blit kv get|put|rm|ls`, prefix-watchable, with
  compare-and-swap writes. Handy as host-local scratch space for scripts.

## Install

```bash
curl -sf https://install.blit.sh | sh
```

Windows (PowerShell):

```powershell
irm https://install.blit.sh/install.ps1 | iex
```

## Learn

Run `blit learn` to print the full CLI reference (usage guide for scripts and LLM agents).
