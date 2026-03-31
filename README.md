# blit

Stream terminals to a browser. The server parses PTY output, diffs it, and sends only what changed — LZ4-compressed, per-client paced, WebGL-rendered.

```bash
blit-server &
blit                # opens browser
```

Or over SSH:

```bash
blit --ssh myhost
```

## Install

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

### Nix

```bash
nix build .#blit-server      # or blit-cli, blit-gateway
```

Or jump to [nix-darwin](#nix-darwin) / [NixOS](#nixos) for service configuration.

### From source

```bash
nix develop        # or use direnv — .envrc is included
cargo build --release -p blit-cli -p blit-server -p blit-gateway
```

## How it works

**`blit-server`** hosts PTYs and tracks full parsed terminal state. For each connected client it computes a binary diff against what that client last saw and sends only the delta — LZ4-compressed, with scrolling encoded as copy-rect operations.

**`blit-gateway`** is a stateless WebSocket/WebTransport proxy that authenticates browser clients and forwards traffic to the server over a Unix socket. PTYs survive gateway restarts.

**`blit` (CLI)** connects to a local or remote server (over SSH if needed), embeds a temporary gateway, and opens the browser. No separate deployment required.

**`blit-react`** is the React embedding library — the primary integration point for applications. See [EMBEDDING.md](EMBEDDING.md).

The server paces each client independently based on render metrics the client reports back: display rate, frame apply time, backlog depth. A phone on 3G doesn't stall a workstation on localhost. The focused session gets full frame rate; background sessions throttle down. Keystrokes go straight to the PTY — latency is bounded by link RTT and nothing else.

For wire protocol details, frame encoding, and transport internals, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Browser access

Pick one, not both:

- **`blit-gateway`**: standalone proxy for persistent browser access. Handles passphrase auth, serves the web app, optionally enables QUIC.
- **`blit` (CLI)**: connects to the server, embeds a temporary gateway, opens the browser.

With the gateway:

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

Configuration lives in env files under `$(brew --prefix)/etc/blit/`:

```bash
echo 'export BLIT_PASS="secret"' >> $(brew --prefix)/etc/blit/blit-gateway.env
echo 'export BLIT_SCROLLBACK="50000"' >> $(brew --prefix)/etc/blit/blit-server.env
brew services restart blit-gateway blit-server
```

### Debian / Ubuntu (systemd)

```bash
sudo systemctl enable --now blit@alice.socket
```

### Manual (systemd)

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

## What lives in this repo

| Directory                  | Package          | Role                                             |
| -------------------------- | ---------------- | ------------------------------------------------ |
| `crates/server/`           | `blit-server`    | PTY host and frame scheduler                     |
| `crates/remote/`           | `blit-remote`    | Wire protocol and frame/state primitives         |
| `crates/browser/`          | `blit-browser`   | WASM terminal runtime                            |
| `crates/alacritty-driver/` | `blit-alacritty` | Terminal parsing backed by `alacritty_terminal`  |
| `crates/gateway/`          | `blit-gateway`   | WebSocket/WebTransport proxy                     |
| `crates/cli/`              | `blit`           | Browser client, agent subcommands, SSH tunnels   |
| `crates/fonts/`            | `blit-fonts`     | Font discovery and metadata                      |
| `crates/webserver/`        | `blit-webserver` | Shared HTTP helpers for serving assets and fonts |
| `crates/demo/`             | `blit-demo`      | Sample programs and test content                 |
| `js/react/`                | `blit-react`     | Workspace-based React client library             |
| `js/web-app/`              |                  | Browser UI                                       |

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
