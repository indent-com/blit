# blit

blit is a terminal streaming stack. Most browser terminals stream raw PTY bytes over a WebSocket and let the client parse them. blit flips that: the server parses everything, diffs it, and sends only what changed — LZ4-compressed, per-client paced, and rendered with WebGL on the other end.

The core libraries — `blit-server`, `blit-remote`, `blit-browser`, and `blit-react` — handle the heavy lifting. The CLI, gateway, and web app are ready-to-use tools built on top of them.

For a deep dive into how all the pieces connect — wire protocol, frame encoding, transport internals, the rendering pipeline — see [ARCHITECTURE.md](ARCHITECTURE.md).

## The stack at a glance

**`blit-server`** hosts PTYs and produces per-client frame diffs over a Unix socket. It tracks the full parsed terminal state for every PTY, compares against what each client has seen, and sends only the delta. It paces output per client based on render metrics the client reports back.

**`blit-remote`** is the shared wire protocol: binary message builders, frame containers, state primitives.

**`blit-browser`** is the WASM terminal runtime. It receives compressed frame diffs and produces WebGL vertex data for rendering.

**`blit-react`** is the React embedding library. It manages workspaces, connections, sessions, transports, and rendering. This is the primary integration point for applications. See [EMBEDDING.md](EMBEDDING.md).

## Browser access

Browser access to `blit-server` goes through either of two paths — pick one, not both:

- **`blit-gateway`**: a standalone WebSocket/WebTransport proxy, deployed alongside the server for persistent browser access. Handles passphrase auth, serves the web app, optionally enables QUIC.
- **`blit` (the CLI)**: connects to a local or remote `blit-server` (over SSH if needed), embeds a temporary gateway, and opens the browser — no separate gateway deployment required.

**`web-app/`** is the browser UI served by either path. It provides multi-session management, BSP layouts, search, font/palette selection, and reconnection handling.

## What makes it tick

- The server maintains parsed terminal state and sends binary frame diffs, not byte streams.
- Updates are LZ4-compressed. Scrolling is encoded as copy-rect operations — no resending the whole screen.
- The client reports display rate, frame apply time, and backlog depth. The server paces each client independently, so a phone on 3G doesn't stall a workstation on localhost.
- Keystrokes go straight to the PTY. Latency is bounded by link RTT and nothing else.
- The ACK protocol measures true round-trip time per client. Frames are pipelined to the bandwidth-delay product.
- The focused session gets full frame rate. Background sessions update at a lower rate so they don't hog bandwidth for terminals you're not looking at.

## How it compares

|                               | blit                             | ttyd                | gotty               | Eternal Terminal      | Mosh                  | xterm.js + node-pty  |
| ----------------------------- | -------------------------------- | ------------------- | ------------------- | --------------------- | --------------------- | -------------------- |
| Architecture                  | PTY host + gateway               | Single binary       | Single binary       | Client + daemon       | Client + server       | Library (BYO server) |
| Multiple PTYs                 | ✅ First-class                   | ❌ One per instance | ❌ One per instance | ❌ One per connection | ❌ One per connection | ⚠️ Manual            |
| Browser access                | ✅                               | ✅                  | ✅                  | ❌                    | ❌                    | ✅                   |
| Protocol                      | Binary frame diffs               | Raw byte stream     | Raw byte stream     | SSH + prediction      | UDP + SSP             | Raw byte stream      |
| Delta updates                 | ✅ Only changed cells sent       | ❌                  | ❌                  | ❌                    | ✅ State diffs        | ❌                   |
| LZ4 compression               | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Copy-rect scrolling           | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Per-client backpressure       | ✅ Render-metric pacing          | ❌                  | ❌                  | ⚠️ SSH flow control   | ❌                    | ❌                   |
| Background session throttling | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Server-side search            | ✅ Titles + visible + scrollback | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| WebGL rendering               | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ⚠️ Addon             |
| Transport                     | WS, WebTransport, Unix           | WebSocket           | WebSocket           | TCP                   | UDP                   | WebSocket            |
| WebTransport / QUIC           | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Embeddable (React)            | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ✅                   |
| Agent / CLI subcommands       | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Reconnect on disconnect       | ✅                               | ✅                  | ❌                  | ✅                    | ✅                    | ❌                   |
| SSH tunneling built-in        | ✅                               | ❌                  | ❌                  | ✅                    | ✅                    | ❌                   |

### Adjacent tools

**tmux** is the classic terminal multiplexer — windows, panes, session detach/reattach, all over a Unix socket. It's purely terminal-native: no browser access, no wire protocol beyond its own client-server IPC. blit handles the browser streaming side, so the two are complementary — you can run tmux inside a blit PTY.

**Zellij** is a terminal multiplexer (like tmux) — it manages panes, tabs, and layouts inside your existing terminal. It has a WASM plugin system, built-in search, multiplayer session sharing, and a beta web client. Where blit is a streaming stack that sends rendered frames to a browser, Zellij is a local-first workspace that happens to have a web escape hatch.

**sshx** is a collaborative terminal sharing tool. You run a single command and get a shareable URL with an infinite canvas of terminals, live cursors, and end-to-end encryption (Argon2 + AES). It uses a managed cloud mesh for routing, so there's no self-hosting. The focus is real-time pair programming, not persistent server access.

**tmate** is a tmux fork that gives you instant terminal sharing via a hosted relay. You get an SSH URL and a read-only web URL out of the box. It's the fastest path to "let someone else see my terminal" but doesn't do browser-native rendering, backpressure, or multi-session management.

## What lives in this repo

| Directory           | Package          | Role                                             |
| ------------------- | ---------------- | ------------------------------------------------ |
| `server/`           | `blit-server`    | PTY host and frame scheduler                     |
| `remote/`           | `blit-remote`    | Wire protocol and frame/state primitives         |
| `browser/`          | `blit-browser`   | WASM terminal runtime                            |
| `alacritty-driver/` | `blit-alacritty` | Terminal parsing backed by `alacritty_terminal`  |
| `react/`            | `blit-react`     | Workspace-based React client library             |
| `fonts/`            |                  | Font discovery and metadata                      |
| `webserver/`        |                  | Shared HTTP helpers for serving assets and fonts |
| `gateway/`          | `blit-gateway`   | WebSocket/WebTransport proxy                     |
| `cli/`              | `blit`           | Browser client                                   |
| `web-app/`          |                  | Browser UI                                       |
| `demo/`             |                  | Sample programs and test content                 |

## Install

Nix users: jump to [nix-darwin](#nix-darwin) or [NixOS](#nixos).

### macOS (Homebrew)

```bash
brew install indent-com/tap/blit indent-com/tap/blit-gateway indent-com/tap/blit-server
```

### Debian / Ubuntu (APT)

```bash
curl -fsSL https://repo.blit.sh/blit.gpg | sudo gpg --dearmor -o /usr/share/keyrings/blit.gpg
echo "deb [signed-by=/usr/share/keyrings/blit.gpg arch=$(dpkg --print-architecture)] https://repo.blit.sh/ stable main" \
  | sudo tee /etc/apt/sources.list.d/blit.list
sudo apt update
sudo apt install blit blit-server blit-gateway
```

### From source

```bash
nix develop        # or use direnv — .envrc is included
cargo build --release -p blit-cli -p blit-server -p blit-gateway
```

Individual Nix packages:

```bash
nix build .#blit-server      # or blit-cli, blit-gateway
nix build .#blit-server-deb  # or blit-cli-deb, blit-gateway-deb
```

## Quick start

Start the server, then open a browser session:

```bash
blit-server &
blit
```

`blit` launches an embedded gateway and opens your browser. To access a remote host over SSH:

```bash
blit --ssh myhost
```

To run the gateway separately (e.g. for persistent browser access):

```bash
BLIT_PASS=secret blit-gateway &
blit-server
# open http://localhost:3264
```

If building from source, substitute `cargo run -p blit-server`, `cargo run -p blit-cli`, etc. For the dev environment with hot-reloading, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Running services

### macOS (Homebrew)

```bash
brew services start blit-server
brew services start blit-gateway
```

Configuration lives in env files under `$(brew --prefix)/etc/blit/`. These start empty (the binaries have sensible defaults) and are preserved across upgrades. Add any environment variable from the tables above to override defaults:

```bash
echo 'export BLIT_PASS="secret"' >> $(brew --prefix)/etc/blit/blit-gateway.env
echo 'export BLIT_SCROLLBACK="50000"' >> $(brew --prefix)/etc/blit/blit-server.env
brew services restart blit-gateway blit-server
```

### Debian / Ubuntu (systemd)

The `blit-server` .deb ships the unit files, so after installing via APT:

```bash
sudo systemctl enable --now blit@alice.socket
```

### Manual (systemd)

On non-Debian systems, copy the units from the repo:

```bash
sudo cp systemd/blit@.socket systemd/blit@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now blit@alice.socket
```

## Configuration

### `blit-server`

| Variable          | Default                                                                      | Purpose                     |
| ----------------- | ---------------------------------------------------------------------------- | --------------------------- |
| `SHELL`           | `/bin/sh`                                                                    | Shell to spawn for new PTYs |
| `BLIT_SOCK`       | `$TMPDIR/blit.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit-$USER.sock` | Unix socket path            |
| `BLIT_SCROLLBACK` | `10000`                                                                      | Scrollback rows per PTY     |

### `blit-gateway`

| Variable        | Default                                                                      | Purpose                           |
| --------------- | ---------------------------------------------------------------------------- | --------------------------------- |
| `BLIT_PASS`     | required                                                                     | Browser passphrase                |
| `BLIT_ADDR`     | `0.0.0.0:3264`                                                               | HTTP/WebSocket listen address     |
| `BLIT_SOCK`     | `$TMPDIR/blit.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit-$USER.sock` | Upstream server socket            |
| `BLIT_CORS`     | unset                                                                        | CORS origin for font routes       |
| `BLIT_QUIC`     | unset                                                                        | Set to `1` to enable WebTransport |
| `BLIT_TLS_CERT` | auto-generated                                                               | TLS cert for WebTransport         |
| `BLIT_TLS_KEY`  | auto-generated                                                               | TLS key for WebTransport          |

### `blit` (CLI)

| Variable    | Default                                                                                                                | Purpose          |
| ----------- | ---------------------------------------------------------------------------------------------------------------------- | ---------------- |
| `BLIT_SOCK` | `$TMPDIR/blit.sock`, `/tmp/blit-$USER.sock`, `/run/blit/$USER.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit.sock` | Unix socket path |

For SSH targets, `blit --ssh HOST` forwards the remote Unix socket over SSH and opens the browser with an embedded local gateway.

## Agent subcommands

The CLI includes non-interactive subcommands designed for programmatic / LLM agent use. All subcommands accept `--socket PATH`, `--tcp HOST:PORT`, or `--ssh HOST` to select the transport.

```bash
blit list                                      # List all PTYs (TSV: ID, TAG, TITLE, STATUS)
blit start htop                                # Start a PTY running htop, print its ID
blit start -t build make -j8                   # Start with a tag
blit start --rows 40 --cols 120 bash           # Start with a custom size
blit show 3                                    # Dump current visible terminal text
blit show 3 --ansi                             # Include ANSI color/style codes
blit history 3                                 # Dump all scrollback + viewport
blit history 3 --from-start 0 --limit 50       # First 50 lines
blit history 3 --from-end 0 --limit 50         # Last 50 lines
blit history 3 --from-end 0 --limit 50 --ansi  # Last 50 with ANSI styling
blit send 3 "q"                                # Send keystrokes (supports \n, \t, \x1b escapes)
blit show 3 --rows 40 --cols 120               # Resize before capturing viewport
blit history 3 --cols 200                      # Resize before reading scrollback
blit close 3                                   # Close and remove a PTY

# Against a remote host
blit --ssh myhost list
blit --ssh myhost start htop
blit --ssh myhost show 1
```

Output is plain text with no decoration — designed to be easy for scripts and LLMs to parse. Errors go to stderr; non-zero exit on failure.

## Contributing

Building from source, running tests, dev environment setup, code conventions, and release process are all covered in [CONTRIBUTING.md](CONTRIBUTING.md).

## nix-darwin

```nix
{ inputs, ... }: {
  imports = [ inputs.blit.darwinModules.blit ];

  services.blit = {
    enable = true;
    gateways.default = {
      port = 3264;
      passFile = "/path/to/blit-pass-env";
    };
  };
}
```

## NixOS

```nix
{ inputs, ... }: {
  imports = [ inputs.blit.nixosModules.blit ];

  services.blit = {
    enable = true;
    users = [ "alice" "bob" ];
    gateways.alice = {
      user = "alice";
      port = 3264;
      passFile = "/run/secrets/blit-alice-pass";
    };
  };
}
```
