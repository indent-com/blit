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
blit start bash                           # prints the new session ID to stdout
blit start -t build make -j8              # tag it for later reference
blit start --rows 40 --cols 120 bash      # control terminal dimensions
blit show 3                               # current viewport text (plain)
blit show 3 --ansi                        # current viewport with ANSI colors
blit history 3                            # full scrollback + viewport
blit history 3 --from-end 0 --limit 50    # last 50 lines
blit history 3 --from-start 0 --limit 50  # first 50 lines
blit send 3 "ls -la\n"                    # type a command (note the \n)
blit show 3 --rows 40 --cols 120          # resize before capturing viewport
blit history 3 --cols 200                 # resize before reading scrollback
blit close 3                              # destroy the session
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
- `start` prints a single integer (the new session ID) to stdout.
- `show` and `history` print terminal text to stdout, one line per terminal row. Trailing whitespace per row is trimmed.
- `send` and `close` produce no stdout on success.
- All errors go to stderr. Exit code is non-zero on failure.

## Running commands

`blit start` creates a PTY but does not run the command interactively. To execute a shell command and read its output:

```bash
ID=$(blit start bash)
blit send "$ID" "ls -la\n"
sleep 0.5                    # wait for output
blit show "$ID"              # or: blit history "$ID" --from-end 0 --limit 20
blit close "$ID"
```

The `sleep` is necessary because `send` returns immediately after delivering keystrokes to the PTY. There is no built-in "wait for output to settle" mechanism, so poll with `show` or `history` until the output stabilizes or a known prompt appears.

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

- Use `--cols 200` or wider when starting sessions (or pass `--cols` to `show`/`history` to resize before capturing). Narrow terminals cause line wrapping that makes output harder to parse. The default is 80 columns.
- Tag sessions with `-t` so you can identify them in `list` output without tracking IDs.
- Prefer `history --from-end 0 --limit N` over `show` when you need recent output that may have scrolled off-screen.
- `show` is a snapshot of exactly what a human would see in the terminal right now (the viewport). `history` includes scrollback. If a command produced more output than fits on screen, `show` only has the tail.
- Check exit status in `list`. The STATUS column shows `exited(0)` for success, `exited(1)` for failure, `signal(9)` for SIGKILL, etc.
- Do not try to parse `show` or `history` output as structured data. It is terminal text with possible line wrapping and cursor artifacts.
- Do not assume a command has finished after `send`. Always poll with `show` or `history` to confirm.
