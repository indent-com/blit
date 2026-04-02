# blit

Terminal streaming for browsers and AI agents. One binary, nothing to configure.

Try it now — no install needed:

```bash
docker run --rm grab/blit-demo
```

Or install and run locally:

```bash
curl -sf https://install.blit.sh | sh
blit open # opens a browser
```

Share a terminal over WebRTC:

```bash
blit share # prints a URL anyone can open
```

Connect to a remote host over SSH:

```bash
blit open --ssh myhost
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
curl -sf https://install.blit.sh | sh
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

Or jump to [`nix/README.md`](nix/README.md) for nix-darwin / NixOS service configuration.

## How it works

`blit` hosts PTYs and tracks full parsed terminal state. For each connected browser it computes a binary diff against what that browser last saw and sends only the delta — LZ4-compressed, with scrolling encoded as copy-rect operations. WebGL-rendered in the browser.

Each client is paced independently based on render metrics it reports back: display rate, frame apply time, backlog depth. A phone on 3G doesn't stall a workstation on localhost. The focused session gets full frame rate; background sessions throttle down. Keystrokes go straight to the PTY — latency is bounded by link RTT.

`blit open` opens the browser with an embedded gateway. For persistent multi-user browser access, `blit-gateway` is a standalone proxy that handles passphrase auth, serves the web app, and optionally enables QUIC. `blit-server` can also run standalone for headless/daemon use. For embedding in your own app, [`@blit-sh/react`](EMBEDDING.md) and [`@blit-sh/solid`](EMBEDDING.md) provide framework bindings.

For wire protocol details, frame encoding, and transport internals, see [ARCHITECTURE.md](ARCHITECTURE.md).

## CLI reference

Run `blit learn` to print the full CLI reference. For the machine-readable version, see [SKILL.md](SKILL.md). All subcommands auto-start a local server if needed.

## Configuration

| Variable          | Default                                                                                                                | Purpose                              |
| ----------------- | ---------------------------------------------------------------------------------------------------------------------- | ------------------------------------ |
| `BLIT_SOCK`       | `$TMPDIR/blit.sock`, `/tmp/blit-$USER.sock`, `/run/blit/$USER.sock`, `$XDG_RUNTIME_DIR/blit.sock`, or `/tmp/blit.sock` | Unix socket path                     |
| `BLIT_SCROLLBACK` | `10000`                                                                                                                | Scrollback rows per PTY              |
| `BLIT_HUB`        | `hub.blit.sh`                                                                                                          | Signaling hub URL for WebRTC sharing |

For `blit-gateway` configuration, running as a systemd/launchd service, and Nix module setup, see [SERVICES.md](SERVICES.md) and [`nix/README.md`](nix/README.md).

## How it compares

|                          | blit                           | ttyd                | gotty               | Eternal Terminal      | Mosh                  | xterm.js + node-pty  |
| ------------------------ | ------------------------------ | ------------------- | ------------------- | --------------------- | --------------------- | -------------------- |
| Architecture             | Single binary                  | Single binary       | Single binary       | Client + daemon       | Client + server       | Library (BYO server) |
| Multiple PTYs            | ✅ First-class                 | ❌ One per instance | ❌ One per instance | ❌ One per connection | ❌ One per connection | ⚠️ Manual            |
| Browser access           | ✅                             | ✅                  | ✅                  | ❌                    | ❌                    | ✅                   |
| Delta updates            | ✅ Only changed cells          | ❌                  | ❌                  | ❌                    | ✅ State diffs        | ❌                   |
| LZ4 compression          | ✅                             | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| Per-client backpressure  | ✅ Render-metric pacing        | ❌                  | ❌                  | ⚠️ SSH flow control   | ❌                    | ❌                   |
| WebGL rendering          | ✅                             | ❌                  | ❌                  | ❌                    | ❌                    | ⚠️ Addon             |
| Transport                | WS, WebTransport, WebRTC, Unix | WebSocket           | WebSocket           | TCP                   | UDP                   | WebSocket            |
| Embeddable (React/Solid) | ✅                             | ❌                  | ❌                  | ❌                    | ❌                    | ✅                   |
| Agent / CLI subcommands  | ✅                             | ❌                  | ❌                  | ❌                    | ❌                    | ❌                   |
| SSH tunneling built-in   | ✅                             | ❌                  | ❌                  | ✅                    | ✅                    | ❌                   |

## Contributing

Building from source, running tests, dev environment setup, code conventions, and release process are all covered in [CONTRIBUTING.md](CONTRIBUTING.md). CI/CD pipelines, the install site, and the signaling hub are documented in [SERVICES.md](SERVICES.md). The crate and package map is in [ARCHITECTURE.md](ARCHITECTURE.md).

## Docker sandbox

The `grab/blit-demo` image runs unprivileged and launches `blit share` on startup. It includes `blit` itself, plus fish, busybox, htop, neovim, git, curl, jq, tree, and ncdu.

To build locally:

```bash
nix build .#demo-image
docker load < result
docker run --rm grab/blit-demo
```
