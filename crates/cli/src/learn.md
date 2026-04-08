# blit CLI

blit is a terminal multiplexer and experimental headless Wayland compositor. Every session can run both CLI programs (via PTYs) and GUI applications (via the built-in compositor). Surfaces are video-encoded and streamed to browsers; the CLI gives programmatic control over both terminals and graphical windows.

Drive terminal sessions programmatically through stateless CLI subcommands. Each subcommand opens a fresh connection, performs one operation, and exits.

## Supported platforms

| Platform | Arch          | Wayland compositor | Notes                 |
| -------- | ------------- | ------------------ | --------------------- |
| Linux    | x86_64, arm64 | Yes                | Full features         |
| macOS    | arm64         | No                 | PTY multiplexing only |
| Windows  | x86_64        | No                 | PTY multiplexing only |

## Running commands

`blit start` creates a PTY and prints its session ID. Pass a command directly or omit it to start the user's default shell:

```bash
ID=$(blit start --cols 200 ls -la)     # run a command
ID=$(blit start --cols 200)            # start a shell
```

**Always start sessions with `--cols 200`** (or wider). The default is 80 columns, which causes line wrapping that makes output difficult to parse. Pass `--cols` to `show`/`history` to resize an existing session before reading.

Tag sessions with `-t` so you can identify them in `list` output without tracking IDs.

The command runs asynchronously — `start` returns as soon as the PTY is created, not when the command finishes. Use `--wait --timeout N` on `start` or `blit wait` separately to block until completion.

### Waiting for completion

For one-shot commands, the simplest approach is `start --wait --timeout N`:

```bash
# Start and block until the command finishes
blit start --cols 200 --wait --timeout 120 make -j8
```

For more control, use `blit wait` separately. It blocks until a session exits or a pattern matches in its output. The `--timeout` flag is required.

```bash
# Start, then wait separately (useful when you need the session ID)
ID=$(blit start --cols 200 make -j8)
blit wait "$ID" --timeout 120
blit history "$ID" --from-end 0 --limit 50

# Wait for a specific output pattern (regex)
ID=$(blit start --cols 200 make)
blit wait "$ID" --timeout 120 --pattern 'BUILD (SUCCESS|FAILURE)'

# Wait for a shell prompt to return after sending a command
blit send "$ID" "npm install\n"
blit wait "$ID" --timeout 60 --pattern '\$ $'
```

Exit codes: `blit wait` (and `start --wait`) exits with the PTY's exit code on normal exit, 124 on timeout, and prints the exit status to stdout (e.g. `exited(0)`, `signal(9)`). With `--pattern`, it prints the matching line instead and exits 0.

**Do not assume a command has finished after `start` or `send`.** Always use `wait` to confirm.

## Sending input

`blit send ID TEXT` writes text to a session's PTY. The TEXT argument supports C-style escapes (see [Escape sequences](#escape-sequences)):

```bash
blit send "$ID" "ls -la\n"          # type a command and press Enter
blit send "$ID" "\x03"              # send Ctrl+C
blit send "$ID" "q"                 # press q (e.g. to quit less)
```

Use `-` as TEXT to read from stdin. This is useful for multi-byte or binary payloads:

```bash
printf '\x1b:wq\n' | blit send "$ID" -
```

`send` returns an error if the session has already exited.

## `show` vs `history`

These are the two ways to read terminal output. Getting this distinction right is critical.

|                     | `show`                                                                        | `history`                                                                                                                                                                                      |
| ------------------- | ----------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **What it returns** | Current viewport — exactly what a human would see on screen right now         | Full scrollback buffer + viewport                                                                                                                                                              |
| **When to use**     | Quick glance at current state (e.g. is the prompt back?)                      | Reading command output that may have scrolled off-screen                                                                                                                                       |
| **Gotcha**          | If a command produced more output than fits on screen, earlier output is lost | Without `--limit`, returns everything — can be megabytes for long-running sessions. Unless processing the output, always use `--limit` with `--from-end` or `--from-start` to cap output size. |

**Rule of thumb:** Use `history --from-end 0 --limit N` when you need recent output. Use `show` when you only care about what's visible right now (e.g. checking for a prompt).

### Pagination

`history` supports pagination from either direction:

```bash
# Forward pagination (from oldest)
blit history 3 --from-start 0 --limit 100    # lines 0-99
blit history 3 --from-start 100 --limit 100  # lines 100-199

# Backward pagination (from newest)
blit history 3 --from-end 0 --limit 100      # last 100 lines
blit history 3 --from-end 100 --limit 100    # 100 lines before those
```

Without `--from-start` or `--from-end`, all lines are returned.

Both `show` and `history` accept `--cols` and `--rows` to resize the session before capturing. This is useful to avoid line wrapping artifacts when reading a session that was started at a narrow width:

```bash
blit show "$ID" --cols 200
blit history "$ID" --cols 200 --from-end 0 --limit 50
```

By default, `show` and `history` return plain text with colors stripped. Pass `--ansi` to preserve ANSI SGR escape sequences (colors, bold, underline, etc). This is useful when color carries semantic meaning (e.g. red errors in compiler output, colored diffs).

## Session lifecycle

Sessions persist as long as the blit daemon is running. They are **not** cleaned up automatically.

- A session stays alive until you `close` it or the process inside it exits.
- If the process exits on its own, the session remains in the `list` output with an `exited(N)` status. It still consumes resources until explicitly closed.
- Use `blit kill ID [SIGNAL]` to send a signal to the session's leader process without tearing down the session. Accepts signal names (`TERM`, `KILL`, `INT`, `HUP`, `USR1`, etc.) or numbers (`9`, `15`). Defaults to `TERM`. Use this instead of `close` when you want to signal the process but keep the session around (e.g. to read its final output or wait for it to exit).
- Use `blit restart ID` to re-run an exited session with its original command, size, and tag. Fails if the session is still running. Restart reuses the same session ID but does **not** clear terminal scrollback — old output persists and new output writes on top. Use `close` + `start` if you need a clean slate.
- Sessions do **not** persist across daemon restarts.
- **Clean up after yourself.** Always `close` sessions when you are done. Leaked sessions accumulate and waste resources.

```bash
# Restart a failed build
blit restart "$ID"

# Clean up a specific session
blit close "$ID"

# Check for leaked sessions
blit list
```

## The daemon (`blit server`)

`blit server` runs the daemon that all other subcommands connect to. In most cases it is started automatically; you only need this if you want to run it manually, configure scrollback, or bind it to a specific socket:

```bash
blit server                                      # start with defaults
blit server --socket /tmp/blit.sock              # custom socket path
blit server --scrollback 50000                   # larger scrollback buffer
```

The daemon does not persist across restarts. All sessions are lost when it exits.

## Named remotes

Save frequently-used destinations as named remotes in `~/.config/blit/blit.remotes`:

```bash
blit remote add rabbit ssh:rabbit              # SSH remote
blit remote add prod ssh:alice@prod.co         # user@host
blit remote add lab tcp:10.0.0.5:3264          # raw TCP
blit remote add demo share:mysecret            # WebRTC shared session
blit remote list                               # show all remotes
blit remote remove rabbit                      # remove a remote
blit remote set-default prod                   # make prod the default
```

The default remote is stored in `~/.config/blit/blit.conf` as `target = prod`. When set, all agent subcommands (`list`, `start`, `show`, etc.) connect to it automatically.

URI formats:

| URI                                 | Description                           |
| ----------------------------------- | ------------------------------------- |
| `ssh:[user@]host[:/path/to/socket]` | SSH (embedded client, auto-install)   |
| `tcp:host:port`                     | Raw TCP connection                    |
| `socket:/path`                      | Explicit Unix socket                  |
| `share:passphrase`                  | WebRTC shared session                 |
| `local`                             | Local server (auto-started if needed) |

## Connecting to remotes

By default, `blit` connects to the local daemon via its default Unix socket. Use `--on` (before the subcommand) to connect elsewhere:

```bash
blit --on ssh:dev-server list              # SSH remote
blit --on tcp:192.168.1.10:7890 show 1     # raw TCP
blit --on socket:/tmp/blit.sock list       # explicit Unix socket
blit --on share:mypassphrase list          # WebRTC shared session
blit --on prod list                        # named remote from blit.remotes
```

`--on` accepts any URI (`ssh:`, `tcp:`, `socket:`, `share:`, `local`) or a named remote from `blit.remotes`. See [ARCHITECTURE.md § URI vocabulary](../../../ARCHITECTURE.md#uri-vocabulary) for the full list.

SSH connections use an embedded SSH client (russh — pure Rust, no system `ssh` required). If blit is not installed on the remote host, it is auto-installed to `~/.local/bin` on first connection. If the server is not running, it is auto-started.

### Remote host requirements (SSH auto-install)

When connecting to an SSH remote that doesn't have blit, the embedded SSH client auto-installs it to `~/.local/bin`. This requires:

- `curl` or `wget` — to download the installer from `https://install.blit.sh`
- CA certificates — for HTTPS (typically `ca-certificates` package)
- A supported OS/arch (Linux x86_64/arm64, macOS x86_64/arm64)

### SSH authentication

The embedded SSH client (russh) supports:

- **ssh-agent** via `SSH_AUTH_SOCK` (preferred)
- **Key files**: `~/.ssh/id_ed25519`, `~/.ssh/id_ecdsa`, `~/.ssh/id_rsa`
- **~/.ssh/config**: Hostname, User, Port, IdentityFile
- **~/.ssh/known_hosts**: accept-new behavior (unknown hosts are accepted and recorded; changed keys are rejected)

## Browser UI

`blit open` opens the terminal UI in a browser with the local server plus all remotes from `~/.config/blit/blit.remotes`:

```bash
blit open                                       # local + all configured remotes
blit open --port 8080                           # bind to a specific port
```

Manage remotes with `blit remote add/remove` or through the Remotes dialog (Cmd+K) in the browser.

## Remote installation

`blit install [user@]host` explicitly installs blit on a remote host over SSH. This is optional — SSH remotes are auto-installed on first connection. Use this for pre-provisioning or non-SSH hosts.

```bash
blit install dev-server
blit install pcarrier@dev-server
```

## GUI surface automation

On Linux, every blit PTY session includes an experimental headless Wayland compositor. GUI applications launched inside a session automatically connect to it via `WAYLAND_DISPLAY` (set in the PTY environment). Their windows are captured, encoded as H.264 or AV1 video, and streamed to connected browser clients in real time. The compositor is not available on macOS or Windows.

No special flags are needed — the compositor starts on the first PTY creation and shuts down when all PTYs exit.

When a blit session runs GUI applications, you can list, screenshot, and interact with their windows.

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

### Launching Chromium-based apps

Chromium and Electron apps need `--ozone-platform=wayland` to connect to the compositor:

```bash
chromium --ozone-platform=wayland http://example.com &
```

On machines without a GPU, add flags to software-render WebGL via SwiftShader:

```bash
chromium --ozone-platform=wayland --ignore-gpu-blocklist --enable-unsafe-swiftshader http://example.com &
```

For Electron apps, pass the same flags after `--`.

### Listing surfaces

`blit surfaces` lists all compositor surfaces as TSV with columns: `ID`, `TITLE`, `SIZE`, `APP_ID`.

```bash
blit surfaces
```

### Recording video / terminal output

`blit record` captures raw encoded video from a surface, or timestamped terminal output from a PTY.

```bash
# Record 30 frames of surface video (Annex B H.264/H.265 or OBU AV1, playable with ffplay)
blit record surface 1 --frames 30 --output video.h264

# Record 100 terminal update frames
blit record pty 1 --frames 100 --output session.blitrec
```

Surface recordings produce raw Annex B bitstreams (H.264/H.265) or OBU streams (AV1) that `ffprobe` and `ffplay` can read directly. Terminal recordings use a compact binary format (BLITREC) with microsecond timestamps.

### Capturing screenshots

`blit capture ID` captures a surface as a PNG image. By default, the file is written to `surface-<ID>.png`. Use `--output` to specify a path:

```bash
blit capture 1                     # writes surface-1.png
blit capture 1 --output /tmp/s.png # writes /tmp/s.png
```

### Clicking

`blit click ID X Y` sends a left-click at pixel coordinates (X, Y) on a surface:

```bash
blit click 1 100 50
```

Use `--button` to send a right-click or middle-click:

```bash
blit click --button right 1 100 50   # right-click (context menu)
blit click --button middle 1 100 50  # middle-click
```

### Key presses

`blit key ID KEY` sends a single key press and release. Supports modifier combinations with `+`:

```bash
blit key 1 Return
blit key 1 ctrl+a
blit key 1 shift+Tab
blit key 1 ctrl+shift+c
```

### Typing text

`blit type ID TEXT` types a string character by character. Special keys use xdotool-style `{braces}`:

```bash
blit type 1 "hello world"
blit type 1 "hello{Return}"
blit type 1 "{ctrl+a}replacement text{Return}"
```

## Output conventions

- `list` prints tab-separated values with a header row (`ID`, `TAG`, `TITLE`, `COMMAND`, `STATUS`). Parse on `\t`.
  - COMMAND column: the command passed to `start`, or empty for default-shell sessions.
  - STATUS column: `running`, `exited(N)` (normal exit with code N), `signal(N)` (killed by signal N), or `exited` (exit status unknown).
  - Example: `1\t\tpcarrier@host: /src\t\trunning`
- `start` prints a single integer (the new session ID) to stdout.
- `show` and `history` print terminal text to stdout, one line per terminal row. Trailing whitespace per row is trimmed.
- `surfaces` prints tab-separated values with a header row (`ID`, `TITLE`, `SIZE`, `APP_ID`). Parse on `\t`.
- `capture` writes a PNG file and prints the output path to stdout.
- `click`, `key`, and `type` produce no stdout on success.
- `send`, `restart`, `kill`, and `close` produce no stdout on success. `send` and `kill` return an error if the session has already exited.
- `wait` prints the exit status (e.g. `exited(0)`) on success, or the matching line when `--pattern` is used. Exit code 124 on timeout.
- All errors go to stderr. Exit code is non-zero on failure.
- Check exit status in `list`. The STATUS column shows `exited(0)` for success, `exited(1)` for failure, `signal(9)` for SIGKILL, etc.
- Do not try to parse `show` or `history` output as structured data. It is terminal text with possible line wrapping and cursor artifacts.

## Escape sequences

`send` supports C-style escapes: `\n` (newline/enter), `\r` (carriage return), `\t` (tab), `\\` (literal backslash), `\0` (NUL), `\xHH` (hex byte).

| Action                 | Input                                                             |
| ---------------------- | ----------------------------------------------------------------- |
| Press Enter            | `\n`                                                              |
| Press Ctrl+C           | `\x03`                                                            |
| Press Ctrl+D (EOF)     | `\x04`                                                            |
| Press Ctrl+Z (suspend) | `\x1a`                                                            |
| Press Escape           | `\x1b`                                                            |
| Arrow keys             | `\x1b[A` (up), `\x1b[B` (down), `\x1b[C` (right), `\x1b[D` (left) |
| Quit vim               | `\x1b:q!\n`                                                       |

For multi-byte payloads or binary data, pipe through stdin:

```bash
printf '\x1b:wq\n' | blit send 3 -
```
