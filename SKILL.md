---
name: blit-terminal
description: >
  Use when you need to create, control, or read from terminal sessions
  via the CLI. Covers starting PTYs for commands or shells, sending keystrokes,
  reading output, checking exit status, and managing session lifecycle. Activate
  to interact with terminals programmatically.
---

# blit CLI

Drive terminal sessions programmatically through stateless CLI subcommands. Each subcommand opens a fresh connection, performs one operation, and exits.

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

## Transport options

By default, `blit` connects to the local daemon via its default Unix socket. Use these global flags (before the subcommand) to connect elsewhere:

| Flag                      | Description                            |
| ------------------------- | -------------------------------------- |
| `-s`, `--socket <SOCKET>` | Connect to a specific Unix socket      |
| `--tcp <TCP>`             | Connect via raw TCP (`HOST:PORT`)      |
| `--ssh <SSH>`             | Connect via SSH to a remote host       |
| `--share <PASSPHRASE>`    | Connect via WebRTC to a shared session |

```bash
blit --socket /tmp/blit.sock list
blit --tcp 192.168.1.10:7890 show 1
blit --ssh dev-server start bash
blit --share mypassphrase list
```

`--ssh` tunnels the blit protocol over SSH: it spawns `ssh -T HOST` with an inline script that connects to the remote blit Unix socket via `nc -U` (falling back to `socat`). SSH connection multiplexing is enabled (`ControlMaster=auto`, `ControlPersist=300s`) so subsequent commands reuse the same SSH connection. The remote host must have a running blit daemon.

`--share` connects via WebRTC using a passphrase for peer-to-peer session sharing. Both sides must use the same passphrase and signaling hub (set via `--hub` or `BLIT_HUB`).

## Output conventions

- `list` prints tab-separated values with a header row (`ID`, `TAG`, `TITLE`, `COMMAND`, `STATUS`). Parse on `\t`.
  - COMMAND column: the command passed to `start`, or empty for default-shell sessions.
  - STATUS column: `running`, `exited(N)` (normal exit with code N), `signal(N)` (killed by signal N), or `exited` (exit status unknown).
- `start` prints a single integer (the new session ID) to stdout.
- `show` and `history` print terminal text to stdout, one line per terminal row. Trailing whitespace per row is trimmed.
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
