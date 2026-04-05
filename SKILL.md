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

## Wayland compositor (experimental)

On Linux and macOS, every blit PTY session includes an experimental headless Wayland compositor. GUI applications launched inside a session automatically connect to it via `WAYLAND_DISPLAY` (set in the PTY environment). Their windows are captured, encoded as H.264 or AV1 video, and streamed to connected browser clients in real time. The compositor is not available on Windows.

No special flags are needed — the compositor starts on the first PTY creation and shuts down when all PTYs exit.

### Launching GUI apps

Start a GUI application inside a blit session just like any other command:

```bash
ID=$(blit start foot)          # Wayland terminal emulator
ID=$(blit start firefox)       # browser (uses Wayland by default)
```

Or launch from an existing shell session:

```bash
ID=$(blit start bash)
blit send "$ID" "foot &\n"
```

### Listing surfaces

`blit surfaces` lists all compositor surfaces as TSV:

```bash
blit surfaces
# ID  TITLE          SIZE     APP_ID
# 3   /home/user     696x492  foot
```

### Capturing screenshots

```bash
blit capture 3                     # writes surface-3.png
blit capture 3 --output /tmp/s.png # custom path
```

### Interacting with surfaces

```bash
blit click 3 100 50               # left-click at (100, 50)
blit key 3 Return                 # press Enter
blit key 3 ctrl+c                 # Ctrl+C
blit type 3 "hello{Return}"      # type text (xdotool-style braces for special keys)
```
