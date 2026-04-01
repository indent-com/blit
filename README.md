# blit

Terminal streaming for browsers and AI agents. One binary, nothing to configure.

```bash
curl https://install.blit.sh | sh
blit # opens a browser
```

Share a terminal over WebRTC:

```bash
blit share # prints a URL anyone can open
```

Connect to a remote host over SSH:

```bash
blit --ssh myhost
```

Control terminals programmatically:

```bash
blit start htop # start a terminal, print its ID
blit show 1     # dump current terminal text
blit send 1 "q" # send keystrokes
```

The server auto-starts when needed.

## Install

```bash
curl https://install.blit.sh | sh
```

### macOS (Homebrew)

```bash
brew install indent-com/tap/blit
```

### Debian / Ubuntu (APT)

```bash
curl -fsSL https://install.blit.sh/blit.gpg | sudo gpg --dearmor -o /usr/share/keyrings/blit.gpg
echo "deb [signed-by=/usr/share/keyrings/blit.gpg arch=$(dpkg --print-architecture)] https://install.blit.sh/ stable main" \
  | sudo tee /etc/apt/sources.list.d/blit.list
sudo apt update && sudo apt install blit
```

### Nix

```bash
nix profile install github:indent-com/blit#blit
```

Or jump to [nix-darwin](#nix-darwin) / [NixOS](#nixos) for service configuration.

### From source

```bash
nix develop # or use direnv — .envrc is included
cargo build --release -p blit-cli
```

## How it works

`blit` hosts PTYs and tracks full parsed terminal state. For each connected browser it computes a binary diff against what that browser last saw and sends only the delta — LZ4-compressed, with scrolling encoded as copy-rect operations. WebGL-rendered in the browser.

Each client is paced independently based on render metrics it reports back: display rate, frame apply time, backlog depth. A phone on 3G doesn't stall a workstation on localhost. The focused session gets full frame rate; background sessions throttle down. Keystrokes go straight to the PTY — latency is bounded by link RTT.

`blit` opens the browser with an embedded gateway. For persistent multi-user browser access, `blit-gateway` is a standalone proxy that handles passphrase auth, serves the web app, and optionally enables QUIC. `blit-server` can also run standalone for headless/daemon use. For embedding in your own app, [`blit-react`](EMBEDDING.md) is the React integration library.

For wire protocol details, frame encoding, and transport internals, see [ARCHITECTURE.md](ARCHITECTURE.md).

## CLI subcommands

All subcommands auto-start a local server if needed. For remote hosts, use `--ssh HOST`, `--tcp HOST:PORT`, or `--share PASSPHRASE`.

```bash
blit list                                # List all PTYs (TSV: ID, TAG, TITLE, STATUS)
blit start htop                          # Start a PTY running htop, print its ID
blit start -t build make -j8             # Start with a tag
blit start --rows 40 --cols 120 bash     # Start with a custom size
blit start --wait --timeout 60 make -j8  # Start and block until exit
blit show 3                              # Dump current visible terminal text
blit show 3 --ansi                       # Include ANSI color/style codes
blit history 3                           # Dump all scrollback + viewport
blit history 3 --from-start 0 --limit 50 # First 50 lines
blit history 3 --from-end 0 --limit 50   # Last 50 lines
blit send 3 "q"                          # Send keystrokes (supports \n, \t, \x1b escapes)
blit show 3 --rows 40 --cols 120         # Resize before capturing viewport
blit history 3 --cols 200                # Resize before reading scrollback
blit wait 3 --timeout 30                 # Block until session exits
blit wait 3 --timeout 60 --pattern DONE  # Block until output matches regex
blit restart 3                           # Restart an exited session
blit close 3                             # Close and remove a PTY

blit share                               # Share via WebRTC (prints URL)
blit share --passphrase mysecret         # Share with a specific passphrase
blit share --verbose                     # Share with connection diagnostics

blit upgrade                             # Upgrade blit to the latest version

blit --ssh myhost list                   # Against a remote host
blit --ssh myhost start htop
blit --ssh myhost show 1
```

Output is plain text with no decoration — designed to be easy for scripts and LLMs to parse. Errors go to stderr; non-zero exit on failure.

If you're building an AI agent that drives terminals, [SKILLS.md](SKILLS.md) is a ready-made skill definition you can drop into your agent's tool list.

## Configuration

| Variable          | Default                                                                                                                | Purpose                              |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------ |
| `BLIT_SOCK`       | `$TMPDIR/blit.sock`, `/tmp/blit-$USER.sock`, `/run/blit/$USER.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit.sock` | Unix socket path                     |
| `BLIT_SCROLLBACK` | `10000`                                                                                                                | Scrollback rows per PTY              |
| `BLIT_HUB`        | `hub.blit.sh`                                                                                                          | Signaling hub URL for WebRTC sharing |

### `blit-gateway` (optional, for persistent multi-user browser access)

| Variable        | Default                                                                      | Purpose                           |
| --------------- | ---------------------------------------------------------------------------- | --------------------------------- |
| `BLIT_PASS`     | required                                                                     | Browser passphrase                |
| `BLIT_ADDR`     | `0.0.0.0:3264`                                                               | HTTP/WebSocket listen address     |
| `BLIT_SOCK`     | `$TMPDIR/blit.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit-$USER.sock` | Upstream server socket            |
| `BLIT_CORS`     | unset                                                                        | CORS origin for font routes       |
| `BLIT_QUIC`     | unset                                                                        | Set to `1` to enable WebTransport |
| `BLIT_TLS_CERT` | auto-generated                                                               | TLS cert for WebTransport         |
| `BLIT_TLS_KEY`  | auto-generated                                                               | TLS key for WebTransport          |

## Running as a service

### macOS (Homebrew)

```bash
brew services start blit-server
brew services start blit-gateway
```

### Debian / Ubuntu (systemd)

```bash
sudo systemctl enable --now blit@alice.socket
```

## How it compares

|                               | blit                             | ttyd                | gotty               | Eternal Terminal      | Mosh                  | xterm.js + node-pty  |
| ----------------------------- | -------------------------------- | ------------------- | ------------------- | --------------------- | --------------------- | -------------------- |
| Architecture                  | Single binary                    | Single binary       | Single binary       | Client + daemon       | Client + server       | Library (BYO server) |
| Multiple PTYs                 | ✅ First-class                   | ❌ One per instance | ❌ One per instance | ❌ One per connection | ❌ One per connection | ⚠️ Manual            |
| Browser access                | ✅                               | ✅                  | ✅                  | ❌                    | ❌                    | ✅                   |
| Delta updates                 | ✅ Only changed cells            | ❌                  | ❌                  | ❌                    | ✅ State diffs        | ❌                   |
| LZ4 compression               | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Per-client backpressure       | ✅ Render-metric pacing          | ❌                  | ❌                  | ⚠️ SSH flow control   | ❌                    | ❌                   |
| WebGL rendering               | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ⚠️ Addon             |
| Transport                     | WS, WebTransport, WebRTC, Unix   | WebSocket           | WebSocket           | TCP                   | UDP                   | WebSocket            |
| Embeddable (React)            | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ✅                   |
| Agent / CLI subcommands       | ✅                               | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| SSH tunneling built-in        | ✅                               | ❌                  | ❌                  | ✅                    | ✅                    | ❌                   |

## What lives in this repo

| Directory                  | Package                 | Role                                                             |
| -------------------------- | ----------------------- | ---------------------------------------------------------------- |
| `crates/cli/`              | `blit`                  | Browser client, agent subcommands, SSH tunnels, `server`/`share` |
| `crates/server/`           | `blit-server`           | PTY host and frame scheduler (also embedded in `blit`)           |
| `crates/gateway/`          | `blit-gateway`          | WebSocket/WebTransport proxy for multi-user access               |
| `crates/webrtc-forwarder/` | `blit-webrtc-forwarder` | WebRTC bridge for NAT traversal (STUN/TURN)                      |
| `crates/remote/`           | `blit-remote`           | Wire protocol and frame/state primitives                         |
| `crates/browser/`          | `blit-browser`          | WASM terminal runtime                                            |
| `crates/alacritty-driver/` | `blit-alacritty`        | Terminal parsing backed by `alacritty_terminal`                  |
| `crates/fonts/`            | `blit-fonts`            | Font discovery and metadata                                      |
| `crates/webserver/`        | `blit-webserver`        | Shared HTTP helpers for serving assets and fonts                 |
| `js/react/`                | `blit-react`            | React client library ([EMBEDDING.md](EMBEDDING.md))              |
| `js/web-app/`              |                         | Browser UI                                                       |

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

See [`nix/darwin-module.nix`](nix/darwin-module.nix) for the full list of options.

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

See [`nix/nixos-module.nix`](nix/nixos-module.nix) for the full list of options.
