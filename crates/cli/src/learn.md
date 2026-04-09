# blit CLI

blit is a terminal multiplexer and headless Wayland compositor. Terminals run CLI programs (PTYs) and GUI applications (compositor). Surfaces are video-encoded and streamed to browsers; the CLI gives programmatic control over both.

## Running commands

```bash
ID=$(blit terminal start --cols 200 -- ls -la)     # run a command
ID=$(blit terminal start --cols 200)            # start a shell
```

Always use `--cols 200` or wider to avoid line wrapping. Tag terminals with `-t`.

`start` returns immediately. Use `--wait --timeout N` to block until completion:

```bash
blit terminal start --cols 200 --wait --timeout 120 make -j8
```

Or use `blit terminal wait` separately for pattern matching:

```bash
ID=$(blit terminal start --cols 200 make)
blit terminal wait "$ID" --timeout 120 --pattern 'BUILD (SUCCESS|FAILURE)'
```

## Sending input

```bash
blit terminal send "$ID" "ls -la\n" # type a command and press Enter
blit terminal send "$ID" "\x03"     # Ctrl+C
```

Supports C-style escapes: `\n`, `\t`, `\r`, `\\`, `\0`, `\xHH`. Use `-` to read from stdin.

`\n` sends CR (0x0D), which is what a real terminal sends for Enter. This works
regardless of whether the program is in canonical or raw mode. `\r` also sends
CR. Use `\x0a` if you need a literal LF byte.

## Reading output

- `blit terminal show ID` — current viewport (what's on screen now)
- `blit terminal history ID --from-end 0 --limit N` — last N lines from scrollback

Both accept `--cols`/`--rows` to resize before reading, and `--ansi` to preserve colors.

## Terminal lifecycle

```bash
blit terminal list            # show all terminals
blit terminal close "$ID"     # tear down a terminal
blit terminal kill "$ID" TERM # signal the process, keep the terminal
blit terminal restart "$ID"   # re-run an exited terminal
blit quit                     # shut down the server
```

Terminals persist until closed or the daemon exits. Clean up when done.

## Remotes

```bash
blit --on ssh:dev-server terminal list     # SSH (auto-installs blit)
blit --on share:mypassphrase terminal list # WebRTC shared terminal
blit --on prod terminal list               # named remote

blit remote add prod ssh:alice@prod.co
blit remote set-default prod
```

## Clipboard

```bash
blit clipboard list                            # list available MIME types
blit clipboard get                             # read clipboard (text/plain)
blit clipboard get --mime image/png > shot.png # read specific MIME type
blit clipboard set "hello"                     # set clipboard from argument
echo "hello" | blit clipboard set              # set clipboard from stdin
blit clipboard set --mime image/png < shot.png # set specific MIME type
```

## GUI surfaces

On Linux, GUI apps launched in a terminal connect to the built-in Wayland compositor automatically.

```bash
ID=$(blit terminal start firefox)
ID=$(blit terminal start brave --ozone-platform=wayland https://example.com)
```

Chromium-based browsers and Electron apps need `--ozone-platform=wayland`.

### Surface commands

```bash
blit surface list                                   # list surfaces (TSV: ID, TITLE, SIZE, APP_ID)
blit surface close 1                                # close a surface (sends xdg_toplevel close)
blit surface capture 1                              # screenshot → surface-1.png
blit surface capture 1 --output s.png --scale 240   # 2x render (scale in 120ths: 120=1x, 240=2x)
blit surface click 1 100 50                         # left-click at (100, 50)
blit surface click 1 100 50 --button right          # right-click
blit surface key 1 Return                           # key press
blit surface key 1 ctrl+shift+c                     # modifier combo
blit surface type 1 "hello{Return}"                 # type text ({braces} for special keys)
blit surface record 1 --output video.h264           # record until Ctrl+C
blit surface record 1 --duration 10 --output v.h264 # record 10 seconds
blit surface record 1 --frames 30 --output v.h264   # record 30 frames
```
