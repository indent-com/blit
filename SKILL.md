---
name: blit-terminal
description: >
  Use when you need to create, control, or read from terminal sessions
  via the CLI. Covers starting PTYs for commands or shells, sending keystrokes,
  reading output, checking exit status, and managing session lifecycle. Activate
  to interact with terminals programmatically.
---

# blit CLI

## Install

```bash
curl -sf https://install.blit.sh | sh
```

macOS (Homebrew):

```bash
brew install indent-com/tap/blit
```

Debian / Ubuntu (APT):

```bash
curl -fsSL https://install.blit.sh/blit.gpg | sudo gpg --dearmor -o /usr/share/keyrings/blit.gpg
echo "deb [signed-by=/usr/share/keyrings/blit.gpg arch=$(dpkg --print-architecture)] https://install.blit.sh/ stable main" \
  | sudo tee /etc/apt/sources.list.d/blit.list
sudo apt update && sudo apt install blit
```

Nix:

```bash
nix profile install github:indent-com/blit#blit
```

## Learn

Run `blit learn` to print the full CLI reference (usage guide for scripts and LLM agents).
