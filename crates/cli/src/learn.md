# blit CLI

blit is a terminal multiplexer and headless Wayland compositor. Terminals run CLI programs (PTYs) and GUI applications (compositor). Surfaces are video-encoded and streamed to browsers; the CLI gives programmatic control over both.

## Running commands

```bash
ID=$(blit terminal start --cols 200 -- ls -la)     # run a command
ID=$(blit terminal start --cols 200)            # start a shell
```

Always use `--cols 200` or wider to avoid line wrapping. Tag terminals with `-t`.

The command is executed directly — no login shell, so no rc files and no shell
syntax. For a pipe, a redirection, a glob, or a `&&`, ask for the shell:

```bash
blit terminal start --cols 200 --shell 'make 2>&1 | tail -40'
```

Pass a working directory and environment variables the same way you would to
any other program. `--env` is repeatable, and options go _before_ the command:

```bash
blit terminal start --cols 200 --cwd /src/blit --env RUST_LOG=debug -- cargo run
```

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
- `blit terminal history ID --since CURSOR` — only what is new since `SEQ`, `SEQ:COL`, `now`, or `start`

Always pass `--limit` or `--since`. Bare `history` is the whole scrollback.

`--since` prints a cursor to feed back in. `--json` is the structured form
(text plus the next cursor). Default cap is 256 KiB; the reply says where to
continue from if it truncated.

Both `show` and line-based `history` accept `--cols`/`--rows` to resize before
reading, and `--ansi` to preserve colors.

## Commands in a live shell

A shell that emits OSC 133 records each command (see
`docs/shell-integration.md`). Without that, the journal is empty.

```bash
blit terminal send "$ID" "cargo test\n"
blit terminal output "$ID" --wait 600          # that command's output; exits with its status
blit terminal journal "$ID"                    # INDEX STATUS EXIT MS START_SEQ END_SEQ COMMAND
blit terminal output "$ID" 3                   # command 3's output
blit terminal journal "$ID" --json
```

`--wait` blocks server-side until the command finishes (exit 124 on timeout).
`wait --pattern` matches only output produced after the wait began.

## Terminal lifecycle

```bash
blit terminal list            # show all terminals
blit client list              # show peers, subscriptions, and view sizes
blit client kick 3 --reason "duplicate tab"
blit terminal close "$ID"     # tear down a terminal
blit terminal kill "$ID" TERM # signal the process, keep the terminal
blit terminal restart "$ID"   # re-run an exited terminal
blit terminal resize "$ID" 200 50  # set the viewport (cols rows)
blit terminal attach "$ID"    # drive it from here; Ctrl-] detaches
blit quit                     # shut down the server
```

Terminals persist until closed or the daemon exits. Clean up when done.

`attach` needs a real tty on stdin and repaints the remote grid in the
alternate screen, so your scrollback survives. It exits with the remote
program's status if that program finishes while you are attached.

`type` synthesises US-QWERTY keystrokes and understands `{Return}`;
`text` sends the characters themselves, which is the only way to reach
anything non-ASCII.

## Remotes

```bash
blit --on ssh:dev-server terminal list     # SSH (auto-installs blit)
blit --on share:mypassphrase terminal list # WebRTC shared terminal
blit --on prod terminal list               # named remote
blit --on local:work terminal list         # named local server (auto-starts)

blit remote add prod ssh:alice@prod.co
blit remote set-default prod
```

## Files

All paths are relative to `--root` (default: the client's cwd, resolved
against _your_ cwd, not the daemon's). `--json` emits NDJSON.

```bash
blit fs cat src/main.rs                  # print a file (bytes, unmodified)
blit fs find main.rs                     # fuzzy-find by path, best first
blit fs grep needle                      # search contents, PATH:LINE:TEXT
blit fs grep -e 'fn \w+' --root crates   # regex
blit fs grep -sw Config                  # case-sensitive, whole word
blit fs grep -l needle                   # matching paths only
blit fs grep --no-ignore TODO            # include gitignored files
blit fs sync . --json                    # mirror a tree, stream changes
```

`fs grep` is case-insensitive literal by default and honours `.gitignore`,
which is what keeps it fast on a tree with build output — `--no-ignore`
searches ignored files too and ranks them last. It exits 1 when nothing
matched, like grep(1), so `if blit fs grep -l TODO; then …` works.

Writes are compare-and-swap by default, so a concurrent change is a
conflict rather than a silent clobber (exit 1):

```bash
echo hi | blit fs write notes.txt              # unconditional overwrite
echo hi | blit fs write notes.txt --create     # fail if it exists
echo hi | blit fs write notes.txt --if-hash H  # only if unchanged
blit fs mkdir -p a/b        # create a directory
blit fs mv old new          # rename or move
blit fs rm -r dir           # remove a subtree
blit fs ln -s target link   # symlink (omit -s for a hard link)
```

## Git

Read-only introspection of repositories on the server. `--repo` picks the
worktree (default: cwd); `--json` emits NDJSON.

```bash
blit git status                     # branch, ahead/behind, stash, worktree
blit git status --watch             # stream changes
blit git log                        # history, newest first
blit git log v1.0                   # from a tag
blit git log main..feature          # a range
blit git log --follow -- src/main.rs
blit git diff                       # unstaged
blit git diff --staged              # staged
blit git diff main dev              # between two commits
blit git diff main...dev            # since they diverged (from the merge base)
blit git diff --merge-base main     # worktree vs where main forked (a `base` line names it)
blit git diff HEAD~2 -p -- src      # with hunks, limited to a path
blit git show HEAD:src/main.rs      # a file's bytes at a revision
blit git show HEAD                  # the commit object itself
blit git ls-tree HEAD                # one tree level (MODE TYPE OID<TAB>NAME)
blit git ls-tree HEAD:src            # descend by passing a path
blit git merge-base main feature     # best common ancestors (exit 1 if unrelated)
blit git ls-files                    # the index (MODE STAGE OID<TAB>PATH)
blit git ls-files src                # limited to a path prefix
blit git blame src/main.rs           # one row per attributed range
blit git blame src/main.rs --start 40 --lines 20   # just a viewport
blit git reflog                      # HEAD's reflog, newest first
blit git reflog refs/heads/main -n 5
blit git discover /workspace         # repositories under a path
blit git fetch origin                # per-ref outcomes; exit 1 if any refused
blit git fetch origin refs/pull/12/head --anchor
```

`blame` prints commit oids, not authors — resolve them with `git log` when
you need names, which keeps a viewport blame small. `fetch` runs the
server's own `git`, so its credential helpers and config apply, and it
exits non-zero when any ref was refused (plain `git fetch` can exit 0
having refused one refspec of several).

## Code intelligence

Language servers (rust-analyzer, gopls, clangd, …) are discovered by
project markers, spawned on the server, and stay warm across
invocations. Positions are 1-based PATH:LINE:COL; all commands take
`--root` (default: server cwd) and `--json` (NDJSON).

```bash
blit lsp wait                        # block until servers finish indexing
blit lsp diag                        # current diagnostics (exit 1 if any)
blit lsp diag --wait                 # settle first — use after editing files
blit lsp def src/main.rs:10:4        # definition of the symbol at 10:4
blit lsp refs src/main.rs:10:4       # references (--declaration to include it)
blit lsp hover src/main.rs:10:4      # type and docs
blit lsp complete src/main.rs:10:4   # completions (TSV: LABEL, KIND, DETAIL)
blit lsp signature src/main.rs:10:4  # signature, active parameter underlined
blit lsp symbols Config              # fuzzy workspace symbol search
blit lsp symbols --file src/main.rs  # file outline
blit lsp rename src/main.rs:10:4 nm  # rename plan (prints edits, never applies)
blit lsp list                        # running servers (ref, phase, memory)
```

A first call in a fresh workspace may exit 2 with "warming up" — run
`blit lsp wait` once, then query. The edit loop: change files, then
`blit lsp diag --wait` to see resulting errors.

## Key/value store

A prefix-watchable store on the server — the web app's settings live here,
and it doubles as host-local scratch space for scripts.

```bash
blit kv put build/status ok        # set from an argument
cat report.json | blit kv put ci/report   # or from stdin
blit kv get build/status           # value bytes to stdout (exit 1 if absent)
blit kv ls build/                  # keys under a prefix (TSV: KEY, SIZE)
blit kv ls build/ --values         # include values
blit kv ls build/ --watch          # stream changes as they happen
blit kv rm build/status            # delete
```

Writes are compare-and-swap when you ask: `--if-hash H` writes only if the
current value still hashes to H, exiting 1 on conflict. Without it a put is
an unconditional overwrite. `--durable` waits for disk.

## Wasmi extensions

```bash
blit ext run worker.wasm arg1 --guest-flag
blit ext list
blit ext status NAME
blit ext attach NAME
blit ext commands --on prod
blit --on prod @builder --help
blit --on prod @builder build --release app
```

`blit ext commands` lists live command namespaces advertised by named,
persistent extensions. Connection options must precede `@name`; every later
token is sent to the extension verbatim, including `--json`, `--on`, and other
tokens beginning with `-`. A final `--help` immediately after an advertised
command path is rendered locally from its descriptor. Redirected stdin is
streamed to the command, while terminal stdin starts closed.

## Web panes

In the browser UI, `Cmd+K` → **Web pane** opens a URL the server can reach as a
tiling pane. The empty-pane entry doubles as a location bar: `[remote>][command]`
takes a URL wherever it takes a command, so `localhost:3000` or
`prod>localhost:3000` opens a pane instead of a terminal (a scheme or a port is
what marks it as a location; a bare word stays a command). Locations are
remembered per server in its KV store, and the focused pane takes over the
status bar with back/forward/reload and its title.

## Port forwarding

Forward local ports to whatever the server can reach — `ssh -L` over any blit
transport, plus UDP. Specs are `[kind/][bind:]port:host:hostport`, where kind
is `tcp` (default) or `udp`.

```bash
blit forward 8080:localhost:3000               # local 8080 → server's :3000
blit forward 8080:localhost:3000 5432:db:5432   # a list, one connection
blit forward udp/5353:resolver.internal:53      # a UDP flow per local source
blit forward tls/8443:api.internal:443          # server terminates TLS
blit forward 0:db.internal:5432                 # port 0: pick one, print it
blit --on prod forward 5432:db.internal:5432    # through a named remote
```

Listeners bind to **127.0.0.1** unless a bind address is given: the local
socket has no passphrase in front of it, so binding a wildcard address would
hand the relay to anyone who can reach the machine. Every spec binds before
any starts serving, so a failed bind leaves nothing running.

Forwards end with the process — the listening socket is local, so there is
nothing to reattach to. Keep a set of them in `~/.config/blit/blit.forwards`:

```bash
blit forward add web 8080:localhost:3000   # add or update an entry
blit forward list                           # every entry, disabled ones marked
blit forward toggle web                     # disable without removing
blit forward rm web
blit forward --all                          # start every enabled entry
```

A `tls/` forward takes plaintext locally and speaks TLS to the target, so
`curl http://localhost:8443/` reaches an `https://` service without a
certificate anywhere on your side. `--alpn h2,http/1.1` offers ALPN (omitted
offers none, which is not the same as offering http/1.1) and the negotiated
protocol is printed once. `--insecure` skips certificate verification, and the
server must permit it too (`blit server --allow-forward-insecure`) — a client
asking is not enough. Plain `tcp/` forwards stay opaque: TLS the local client
speaks passes straight through, end to end.

By default the relay reaches whatever the server reaches. To restrict it, give
`blit server --allow-forward 'host[:ports]'` (a name, a `*.suffix` glob, an
address, a CIDR block, or `*`; repeatable, or `BLIT_ALLOW_FORWARD`) — one
pattern makes it an allowlist, loopback still permitted. `BLIT_NET=0` turns
forwarding off entirely.

UDP note: blit's wire is reliable and ordered, so relayed datagrams get
retransmission and head-of-line blocking they did not ask for. Fine for
DNS-shaped request/response traffic; poor for anything running its own
congestion control, such as QUIC.

## SOCKS5 proxy

Proxy TCP into the server's network with a single local port — `ssh -D`. The
target comes from each request instead of a spec, so nothing has to be known in
advance. The listen address is `[bind_address:]port`.

```bash
blit socks 1080                                  # SOCKS5 on 127.0.0.1:1080
blit socks 0.0.0.0:1080                          # reachable from the network
blit socks 0                                     # pick a port, print it
blit --on prod socks 1080                        # through a named remote
curl -x socks5h://localhost:1080 http://api.internal/
```

Use a client that sends **names**, not addresses: `socks5h://` in curl,
`network.proxy.socks_remote_dns` in Firefox, `--proxy-server="socks5://…"` in
Chrome. Names are resolved on the server, so the proxy reaches hosts this
machine cannot look up — which is most of the reason to use it over a forward.
A client that resolves locally still works but loses that.

CONNECT only: BIND and UDP ASSOCIATE are answered `0x07` (command not
supported). No authentication beyond the no-auth method. Failures keep their
reason — a name that does not resolve is `0x04`, a refused connection is
`0x05`, a target denied by `--allow-forward` is `0x02` — so a client reports
what actually happened.

Binds **127.0.0.1** unless a bind address is given, and the default matters
more here than for a forward: a forward hands out one target, a proxy hands out
everything the server can reach. The same `blit server --allow-forward` patterns
and `BLIT_NET=0` govern it. The proxy ends with the process.

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
blit surface click 1 100 50 --button middle         # middle-click (pastes the primary selection)
blit surface click 1 100 50 --button back           # thumb buttons: back, forward
blit surface key 1 Return                           # key press
blit surface key 1 ctrl+shift+c                     # modifier combo
blit surface type 1 "hello{Return}"                 # type text ({braces} for special keys)
blit surface text 1 "café — naïve"                  # commit literal UTF-8 (non-ASCII works)
blit surface scroll 1 3                             # scroll down 3 wheel notches
blit surface scroll 1 -2 --horizontal               # scroll left 2 notches
blit surface focus 1                                # give it keyboard/pointer focus
blit surface record 1 --output video.h264           # record until Ctrl+C
blit surface record 1 --duration 10 --output v.h264 # record 10 seconds
blit surface record 1 --frames 30 --output v.h264   # record 30 frames
```
