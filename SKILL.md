---
name: blit
description: >
  Terminal multiplexer and experimental Wayland compositor. Use when you need to create,
  control, or read from terminals via the CLI, or run and interact
  with GUI applications. Covers starting PTYs, sending keystrokes, reading
  output, checking exit status, managing terminal lifecycle, and driving
  graphical windows through the experimental headless Wayland compositor (listing
  surfaces, capturing screenshots, clicking, typing, and sending key
  presses).
---

# blit CLI

blit is a terminal multiplexer and experimental headless Wayland compositor. Every terminal can run both CLI programs (via PTYs) and GUI applications (via the built-in compositor). Surfaces are video-encoded and streamed to browsers; the CLI gives programmatic control over both terminals and graphical windows.

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
