---
name: blit-terminal
description: >
  Use when you need to create, control, or read from terminal sessions
  via the blit CLI. Covers starting PTYs, sending keystrokes, reading
  output, checking exit status, and managing session lifecycle. Activate
  when interacting with blit terminals programmatically.
---

# blit CLI

Drive terminal sessions programmatically through stateless CLI subcommands. Each subcommand opens a fresh connection, performs one operation, and exits.

## Quick reference

```bash
blit list                                 # TSV: ID  TAG  TITLE  STATUS
blit start --cols 200                     # start default shell, print session ID
blit start --cols 200 htop                # start a specific command
blit start -t build --cols 200 make -j8   # tag it for later reference
blit start --rows 40 --cols 200 htop      # control terminal dimensions
blit start --wait --cols 200 make test    # block until command exits, propagate exit code
blit show 3                               # current viewport text (plain)
blit show 3 --ansi                        # current viewport with ANSI colors
blit history 3                            # full scrollback + viewport
blit history 3 --from-end 0 --limit 50    # last 50 lines
blit history 3 --from-start 0 --limit 50  # first 50 lines
blit send 3 "ls -la\n"                    # type a command (note the \n)
blit show 3 --rows 40 --cols 200          # resize before capturing viewport
blit history 3 --cols 200                 # resize before reading scrollback
blit restart 3                            # restart an exited session
blit close 3                              # destroy the session
```

**Always start sessions with `--cols 200`** (or wider). The default is 80 columns, which causes line wrapping that makes output difficult to parse. Pass `--cols` to `show`/`history` to resize an existing session before reading.

## `show` vs `history`

These are the two ways to read terminal output. Getting this distinction right is critical.

| | `show` | `history` |
|---|---|---|
| **What it returns** | Current viewport — exactly what a human would see on screen right now | Full scrollback buffer + viewport |
| **When to use** | Quick glance at current state (e.g. is the prompt back?) | Reading command output that may have scrolled off-screen |
| **Gotcha** | If a command produced more output than fits on screen, earlier output is lost | Returns everything, which can be very large for long-running commands |

**Rule of thumb:** Use `history --from-end 0 --limit N` when you need recent output. Use `show` when you only care about what's visible right now (e.g. checking for a prompt).

## Running commands

`blit start` creates a PTY and prints its session ID. Pass a command directly or omit it to start the user's default shell:

```bash
ID=$(blit start --cols 200 ls -la)     # run a command
ID=$(blit start --cols 200)            # start a shell
```

The command runs asynchronously — `start` returns as soon as the PTY is created, not when the command finishes. Poll with `show` or `history` to read output.

### Waiting for completion

For one-shot commands where you only need the exit code, use `--wait`:

```bash
blit start --wait --cols 200 make test   # blocks until done, exits with make's exit code
```

When you need to read the output, poll until the session exits or a known marker appears:

```bash
# For one-shot commands: poll until the process exits
for i in $(seq 1 50); do
  blit list | grep -P "^$ID\t" | grep -q 'exited' && break
  sleep 0.2
done
blit history "$ID" --from-end 0 --limit 20

# For interactive sessions: poll for a shell prompt
for i in $(seq 1 50); do
  blit history "$ID" --from-end 0 --limit 3 | grep -q '\$ $' && break
  sleep 0.2
done

# For commands with known end markers
for i in $(seq 1 150); do
  blit history "$ID" --from-end 0 --limit 5 | grep -qE 'BUILD (SUCCESS|FAILURE)' && break
  sleep 0.2
done
```

**Do not assume a command has finished after `start` or `send`.** Always poll to confirm.

## Session lifecycle

Sessions persist as long as the blit daemon is running. They are **not** cleaned up automatically.

- A session stays alive until you `close` it or the process inside it exits.
- If the process exits on its own, the session remains in the `list` output with an `exited(N)` status. It still consumes resources until explicitly closed.
- Use `blit restart ID` to re-run an exited session with its original command, size, and tag. Fails if the session is still running.
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

| Flag                      | Description                       |
| ------------------------- | --------------------------------- |
| `-s`, `--socket <SOCKET>` | Connect to a specific Unix socket |
| `--tcp <TCP>`             | Connect via raw TCP (`HOST:PORT`) |
| `--ssh <SSH>`             | Connect via SSH to a remote host  |

```bash
blit --socket /tmp/blit.sock list
blit --tcp 192.168.1.10:7890 show 1
blit --ssh dev-server start bash
```

## Output conventions

- `list` prints tab-separated values with a header row. Parse on `\t`.
  - STATUS column: `running`, `exited(N)` (normal exit with code N), `signal(N)` (killed by signal N), or `exited` (exit status unknown).
- `start` prints a single integer (the new session ID) to stdout. With `--wait`, it then blocks until the command exits and uses its exit code (signal deaths exit with 128 + signal number).
- `show` and `history` print terminal text to stdout, one line per terminal row. Trailing whitespace per row is trimmed.
- `send`, `restart`, and `close` produce no stdout on success.
- All errors go to stderr. Exit code is non-zero on failure.


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

## Pagination

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

## ANSI mode

By default, `show` and `history` return plain text with colors stripped. Pass `--ansi` to preserve ANSI SGR escape sequences (colors, bold, underline, etc). This is useful when color carries semantic meaning (e.g. red errors in compiler output, colored diffs).

## Guidelines

- Tag sessions with `-t` so you can identify them in `list` output without tracking IDs.
- Check exit status in `list`. The STATUS column shows `exited(0)` for success, `exited(1)` for failure, `signal(9)` for SIGKILL, etc.
- Do not try to parse `show` or `history` output as structured data. It is terminal text with possible line wrapping and cursor artifacts.
