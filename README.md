# blit

blit is a terminal streaming stack. Most browser terminals stream raw PTY bytes over a WebSocket and let the client parse them. blit flips that: the server parses everything, diffs it, and sends only what changed — LZ4-compressed, per-client paced, and rendered with WebGL on the other end.

The core libraries — `blit-server`, `blit-remote`, `blit-browser`, and `blit-react` — are the product. The CLI, gateway, and web app are a demo of what you can build with them.

For a deep dive into how all the pieces connect — wire protocol, frame encoding, transport internals, the rendering pipeline — see [ARCHITECTURE.md](ARCHITECTURE.md).

## The stack at a glance

**`blit-server`** hosts PTYs and produces per-client frame diffs over a Unix socket. It tracks the full parsed terminal state for every PTY, compares against what each client has seen, and sends only the delta. It paces output per client based on render metrics the client reports back.

**`blit-remote`** is the shared wire protocol: binary message builders, frame containers, state primitives.

**`blit-browser`** is the WASM terminal runtime. It receives compressed frame diffs and produces WebGL vertex data for rendering.

**`blit-react`** is the React embedding library. It manages workspaces, connections, sessions, transports, and rendering. This is the primary integration point for applications.

## The demo

Browser access to `blit-server` goes through either of two paths — pick one, not both:

- **`blit-gateway`**: a standalone WebSocket/WebTransport proxy, deployed alongside the server for persistent browser access. Handles passphrase auth, serves the web app, optionally enables QUIC.
- **`blit` (the CLI)**: connects to a local or remote `blit-server` (over SSH if needed), embeds a temporary gateway, and opens the browser — no separate gateway deployment required. Also has a `--console` mode that renders directly in the terminal.

**`web-app/`** is the browser UI served by either path. It demonstrates multi-session management, BSP layouts, search, font/palette selection, and reconnection handling. It is a reference implementation, not a product surface.

## What makes it tick

- The server maintains parsed terminal state and sends binary frame diffs, not byte streams.
- Updates are LZ4-compressed. Scrolling is encoded as copy-rect operations — no resending the whole screen.
- The client reports display rate, frame apply time, and backlog depth. The server paces each client independently, so a phone on 3G doesn't stall a workstation on localhost.
- Keystrokes go straight to the PTY. Latency is bounded by link RTT and nothing else.
- The ACK protocol measures true round-trip time per client. Frames are pipelined to the bandwidth-delay product.
- The focused session gets full frame rate. Background sessions update at a lower rate so they don't hog bandwidth for terminals you're not looking at.

## How it compares

| | blit | ttyd | gotty | Eternal Terminal | Mosh | xterm.js + node-pty |
| --- | --- | --- | --- | --- | --- | --- |
| Architecture | Separate PTY host + gateway | Single binary | Single binary | Client + daemon | Client + server | Library (BYO server) |
| Multiple PTYs | Yes, first-class | One per instance | One per instance | One per connection | One per connection | Manual |
| Protocol | Binary frame diffs | Terminal byte stream | Terminal byte stream | SSH + prediction | UDP + SSP | Terminal byte stream |
| Backpressure | Per-client pacing from render metrics | None | None | SSH flow control | None | None |
| Server-side search | Titles + visible + scrollback | No | No | No | No | No |
| Transport | WebSocket, WebTransport, Unix socket | WebSocket | WebSocket | TCP | UDP | WebSocket |
| Embeddable | React library | No | No | No | No | Yes (xterm.js) |

## Embedding with `blit-react`

`blit-react` is workspace-first. A `BlitWorkspace` owns connections, each connection owns sessions, and each `BlitTerminal` renders a session by ID.

```tsx
import {
  BlitTerminal,
  BlitWorkspace,
  BlitWorkspaceProvider,
  WebSocketTransport,
  useBlitFocusedSession,
  useBlitSessions,
  useBlitWorkspace,
} from "blit-react";
import { useEffect, useMemo } from "react";

function EmbeddedBlit({ wasm, passphrase }: { wasm: any; passphrase: string }) {
  const transport = useMemo(
    () => new WebSocketTransport("wss://example.com/blit", passphrase),
    [passphrase],
  );

  const workspace = useMemo(
    () =>
      new BlitWorkspace({
        wasm,
        connections: [{ id: "default", transport }],
      }),
    [transport, wasm],
  );

  useEffect(() => () => workspace.dispose(), [workspace]);

  return (
    <BlitWorkspaceProvider workspace={workspace}>
      <TerminalScreen />
    </BlitWorkspaceProvider>
  );
}

function TerminalScreen() {
  const workspace = useBlitWorkspace();
  const sessions = useBlitSessions();
  const focusedSession = useBlitFocusedSession();

  useEffect(() => {
    if (sessions.length > 0) return;
    void workspace.createSession({
      connectionId: "default",
      rows: 24,
      cols: 80,
    });
  }, [sessions.length, workspace]);

  return (
    <BlitTerminal
      sessionId={focusedSession?.id ?? null}
      style={{ width: "100%", height: "100vh" }}
    />
  );
}
```

### React API

| API | Purpose |
| --- | --- |
| `new BlitWorkspace({ wasm, connections })` | Create a workspace with one or more transports |
| `BlitWorkspaceProvider` | Put the workspace, palette, and font settings in context |
| `useBlitWorkspace()` | Get the imperative workspace object |
| `useBlitWorkspaceState()` | Read the full reactive workspace snapshot |
| `useBlitConnection(connectionId?)` | Read one connection snapshot |
| `useBlitSessions()` | Read all sessions |
| `useBlitFocusedSession()` | Read the currently focused session |
| `BlitTerminal` | Render one session by `sessionId` |

### Workspace operations

- `createSession({ connectionId, rows, cols, tag?, command?, cwdFromSessionId? })`
- `closeSession(sessionId)`
- `restartSession(sessionId)`
- `focusSession(sessionId | null)`
- `search(query, { connectionId? })`
- `setVisibleSessions(sessionIds)`
- `addConnection(...)` / `removeConnection(connectionId)` / `reconnectConnection(connectionId)`

### Transports

```ts
// WebSocket
new WebSocketTransport(url, passphrase, { reconnect, reconnectDelay, maxReconnectDelay, reconnectBackoff })

// WebTransport (QUIC/HTTP3)
new WebTransportTransport(url, passphrase, { reconnect, serverCertificateHash })

// WebRTC data channel
createWebRtcDataChannelTransport(peerConnection, { label, displayRateFps, connectTimeoutMs })
```

Or implement your own:

```ts
interface BlitTransport {
  connect(): void;
  send(data: Uint8Array): void;
  close(): void;
  readonly status: ConnectionStatus;
  addEventListener(type: "message" | "statuschange", listener: Function): void;
  removeEventListener(type: "message" | "statuschange", listener: Function): void;
}
```

## What lives in this repo

| Directory | Package | Role |
| --- | --- | --- |
| `server/` | `blit-server` | PTY host and frame scheduler |
| `remote/` | `blit-remote` | Wire protocol and frame/state primitives |
| `browser/` | `blit-browser` | WASM terminal runtime |
| `alacritty-driver/` | `blit-alacritty` | Terminal parsing backed by `alacritty_terminal` |
| `react/` | `blit-react` | Workspace-based React client library |
| `fonts/` | | Font discovery and metadata |
| `webserver/` | | Shared HTTP helpers for serving assets and fonts |
| `gateway/` | `blit-gateway` | Demo: WebSocket/WebTransport proxy |
| `cli/` | `blit` | Demo: browser/console client |
| `web-app/` | | Demo: browser UI |
| `demo/` | | Demo programs and test content |

## Quick start

```bash
nix develop        # or use direnv — .envrc is included

# Demo: browser
cargo run -p blit-server &
cargo run -p blit-cli

# Demo: console
cargo run -p blit-cli -- --console

# Demo: standalone gateway
BLIT_PASS=secret cargo run -p blit-gateway &
cargo run -p blit-server
# open http://localhost:3264

# Demo: remote host
blit myhost
blit --console user@myhost

# Auto-reload development loop
dev
```

## Configuration (demo binaries)

### `blit-server`

| Variable | Default | Purpose |
| --- | --- | --- |
| `SHELL` | `/bin/sh` | Shell to spawn for new PTYs |
| `BLIT_SOCK` | `$XDG_RUNTIME_DIR/blit.sock` or `/tmp/blit.sock` | Unix socket path |
| `BLIT_SCROLLBACK` | `10000` | Scrollback rows per PTY |

### `blit-gateway`

| Variable | Default | Purpose |
| --- | --- | --- |
| `BLIT_PASS` | required | Browser passphrase |
| `BLIT_ADDR` | `0.0.0.0:3264` | HTTP/WebSocket listen address |
| `BLIT_SOCK` | `$XDG_RUNTIME_DIR/blit.sock` or `/tmp/blit.sock` | Upstream server socket |
| `BLIT_CORS` | unset | CORS origin for font routes |
| `BLIT_QUIC` | unset | Set to `1` to enable WebTransport |
| `BLIT_TLS_CERT` | auto-generated | TLS cert for WebTransport |
| `BLIT_TLS_KEY` | auto-generated | TLS key for WebTransport |

### `blit` (CLI)

| Variable | Default | Purpose |
| --- | --- | --- |
| `BLIT_SOCK` | `$XDG_RUNTIME_DIR/blit.sock` or `/tmp/blit.sock` | Unix socket path |
| `BLIT_DISPLAY_FPS` | `240` | Advertised display rate in console mode |

For SSH targets, `blit` forwards the remote Unix socket over SSH and either opens the browser with an embedded local gateway or renders directly in the current terminal with `--console`.

## Deployment (demo binaries)

### NixOS

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

### systemd

```bash
sudo cp systemd/blit@.socket systemd/blit@.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now blit@alice.socket
```

### macOS (nix-darwin)

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

## Building and testing

Every `nix run` target has a corresponding script in `bin/` so you don't need to remember the nix invocation:

```bash
./bin/tests                  # Rust + React unit tests
./bin/lint                   # Clippy
./bin/e2e                    # Playwright e2e tests
./bin/build-debs             # .deb packages → dist/debs/
./bin/build-tarballs         # static tarballs → dist/tarballs/
./bin/browser-publish        # npm publish blit-browser
./bin/react-publish          # npm publish blit-react
```

`build-debs` and `build-tarballs` accept an optional output directory argument (default `dist/debs` and `dist/tarballs`).
The version and platform are derived from `flake.nix` and the build host.
Linkage is verified at `nix build` time — Linux binaries must be statically linked and macOS binaries must not reference nix-store dylibs.

Individual packages can also be built directly:

```bash
nix build .#blit-server      # or blit-cli, blit-gateway
nix build .#blit-server-deb  # or blit-cli-deb, blit-gateway-deb
```
