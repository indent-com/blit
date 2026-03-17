---
name: blit
description: >
  Terminal multiplexer and experimental Wayland compositor. Use when you need to create,
  control, or read from terminal sessions via the CLI, or run and interact
  with GUI applications. Covers starting PTYs, sending keystrokes, reading
  output, checking exit status, managing session lifecycle, and driving
  graphical windows through the experimental headless Wayland compositor (listing
  surfaces, capturing screenshots, clicking, typing, and sending key
  presses).
---

# blit CLI

blit is a terminal multiplexer and experimental headless Wayland compositor. Every session can run both CLI programs (via PTYs) and GUI applications (via the built-in compositor). Surfaces are video-encoded and streamed to browsers; the CLI gives programmatic control over both terminals and graphical windows.

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

Windows (PowerShell):

```powershell
irm https://install.blit.sh/install.ps1 | iex
```

Nix:

```bash
nix profile install github:indent-com/blit#blit
```

## Learn

Run `blit learn` to print the full CLI reference (usage guide for scripts and LLM agents).
