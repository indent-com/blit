# Embedding with `blit-react`

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

## React API

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

## Workspace operations

- `createSession({ connectionId, rows, cols, tag?, command?, cwdFromSessionId? })`
- `closeSession(sessionId)`
- `restartSession(sessionId)`
- `focusSession(sessionId | null)`
- `search(query, { connectionId? })`
- `setVisibleSessions(sessionIds)`
- `addConnection(...)` / `removeConnection(connectionId)` / `reconnectConnection(connectionId)`

## Transports

All transports share a common set of options (`BlitTransportOptions`):

| Option | Default | Description |
| --- | --- | --- |
| `reconnect` | `true` | Auto-reconnect on disconnect |
| `reconnectDelay` | `500` | Initial reconnect delay (ms) |
| `maxReconnectDelay` | `10000` | Maximum reconnect delay (ms) |
| `reconnectBackoff` | `1.5` | Backoff multiplier |
| `connectTimeoutMs` | none (WS) / `10000` (WT, WebRTC) | Connection timeout (ms) |

```ts
// WebSocket
new WebSocketTransport(url, passphrase, { reconnect, reconnectDelay, connectTimeoutMs, ... })

// WebTransport (QUIC/HTTP3)
new WebTransportTransport(url, passphrase, { serverCertificateHash, ... })

// WebRTC data channel
createWebRtcDataChannelTransport(peerConnection, { label, displayRateFps, ... })
```

Or implement your own:

```ts
interface BlitTransport {
  connect(): void;
  send(data: Uint8Array): void;
  close(): void;
  readonly status: ConnectionStatus;
  readonly authRejected: boolean;
  readonly lastError: string | null;
  addEventListener(type: "message" | "statuschange", listener: Function): void;
  removeEventListener(type: "message" | "statuschange", listener: Function): void;
}
```

## fd-channel mode

fd-channel lets an external process own `blit-server`'s lifecycle and control which clients connect. Instead of the server binding a socket and accepting connections itself, the external process:

1. Creates a Unix socketpair.
2. Passes one end's fd number to `blit-server` via `--fd-channel FD` (or `BLIT_FD_CHANNEL`).
3. Sends pre-connected client Unix stream fds over the channel using `sendmsg()` with `SCM_RIGHTS` ancillary data.

Each received fd is handled identically to a socket-accepted client -- same binary protocol, same frame pacing, same multi-session support. The server shuts down when the channel closes.

This is the integration point for embedding blit inside a service that wants to enforce its own auth, manage connection routing, or sandbox the server.

### Wire format on received fds

Once a client fd is passed to the server, all communication uses the standard blit binary protocol: **4-byte little-endian length prefix** followed by the message payload. Messages start with a 1-byte opcode. See [ARCHITECTURE.md](ARCHITECTURE.md) for the full opcode table.

### Python

Python's `socket` module supports `SCM_RIGHTS` natively via `sendmsg()`. Create a socketpair, spawn `blit-server` with one end, and send connected client fds over the other.

```python
import os, socket, subprocess, struct

# Create the fd-channel pair
channel_theirs, channel_ours = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)

# Start blit-server with the channel fd
env = {**os.environ, "BLIT_FD_CHANNEL": str(channel_theirs.fileno())}
proc = subprocess.Popen(
    ["blit-server"],
    env=env,
    pass_fds=(channel_theirs.fileno(),),
)
channel_theirs.close()  # server owns its end now

# Create a connected client pair -- one end for us, one for the server
client_ours, client_theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)

# Send client_theirs to the server via SCM_RIGHTS
channel_ours.sendmsg(
    [b"\x00"],  # 1-byte dummy payload (required by sendmsg)
    [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("i", client_theirs.fileno()))],
)
client_theirs.close()  # server owns its end now

# client_ours is now a live blit connection -- read the HELLO frame
def read_frame(sock):
    length_buf = sock.recv(4)
    length = int.from_bytes(length_buf, "little")
    return sock.recv(length)

hello = read_frame(client_ours)
# hello[0] == 0x07 (S2C_HELLO), hello[1:3] == protocol version (u16 LE)

# Send a CREATE to open a PTY: opcode 0x10, rows=24, cols=80, tag=""
create_msg = struct.pack("<BHHH", 0x10, 24, 80, 0)
client_ours.sendall(struct.pack("<I", len(create_msg)) + create_msg)
```

### Bun

Bun does not expose `sendmsg`/`SCM_RIGHTS` directly, but it can spawn `blit-server` with inherited fds and then connect to it by creating a Unix socketpair via a small native helper or by using the standard socket path fallback. The simplest approach uses Bun's built-in Unix socket support alongside fd-channel:

```ts
import { spawn } from "bun";

// Bun can't create socketpairs with SCM_RIGHTS directly, so use a helper.
// blit-fdpass is a tiny C/Rust utility that bridges: it holds the channel
// fd and accepts local Unix connections, forwarding each as an SCM_RIGHTS
// send. Alternatively, use node:child_process with a native addon.

// Simpler approach: use blit-server's socket mode and connect directly.
// fd-channel is most useful from languages with native sendmsg support.

// If you have a native SCM_RIGHTS helper (e.g., via bun:ffi or a C addon):
const SOCK_PATH = "/tmp/blit-embedded.sock";

const server = spawn(["blit-server", "--socket", SOCK_PATH], {
  env: { ...process.env, SHELL: "/bin/bash" },
});

// Wait for socket to appear, then connect
await Bun.sleep(100);

const client = await Bun.connect({
  unix: SOCK_PATH,
  socket: {
    data(socket, data) {
      const view = new DataView(data.buffer);
      // First message: S2C_HELLO (opcode 0x07)
      // Parse 4-byte LE length prefix, then payload
    },
    open(socket) {
      // Send C2S_CREATE: opcode 0x10, rows=24, cols=80, tag_len=0
      const msg = new Uint8Array([0x10, 24, 0, 80, 0, 0, 0]);
      const frame = new Uint8Array(4 + msg.length);
      new DataView(frame.buffer).setUint32(0, msg.length, true);
      frame.set(msg, 4);
      socket.write(frame);
    },
  },
});
```

For Bun, fd-channel is usable if you bridge through `bun:ffi` to call `sendmsg()` with `SCM_RIGHTS`, or use a native addon. In most cases, connecting directly to the Unix socket is simpler -- fd-channel is primarily valuable when the embedding process needs to mediate every connection (auth gating, connection pooling, sandboxing).
