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

Bun doesn't expose `sendmsg`/`SCM_RIGHTS` in its standard library, but `bun:ffi` can call libc directly. The example below creates a socketpair, spawns `blit-server` with one end as the fd-channel, and uses `sendmsg()` via FFI to pass client fds.

```ts
import { spawn } from "bun";
import { dlopen, FFIType, ptr, CString } from "bun:ffi";

const libc = dlopen("libc.so.6", {
  socketpair: { args: [FFIType.i32, FFIType.i32, FFIType.i32, FFIType.ptr], returns: FFIType.i32 },
  sendmsg:    { args: [FFIType.i32, FFIType.ptr, FFIType.i32], returns: FFIType.i64 },
  close:      { args: [FFIType.i32], returns: FFIType.i32 },
});

const AF_UNIX = 1, SOCK_STREAM = 1;
const SOL_SOCKET = 1, SCM_RIGHTS = 1;

function socketpair(): [number, number] {
  const fds = new Int32Array(2);
  if (libc.symbols.socketpair(AF_UNIX, SOCK_STREAM, 0, ptr(fds)) < 0)
    throw new Error("socketpair failed");
  return [fds[0], fds[1]];
}

function sendFd(channel: number, clientFd: number) {
  // Build a msghdr with one iov byte and one SCM_RIGHTS cmsg carrying clientFd.
  // struct iovec { void *iov_base; size_t iov_len; }
  const iovBuf = new Uint8Array(1);
  const iov = new BigUint64Array(2);
  iov[0] = BigInt(Bun.ptr(iovBuf));  // iov_base
  iov[1] = 1n;                        // iov_len

  // cmsg: { cmsg_len, cmsg_level, cmsg_type, data }
  const CMSG_SPACE = 20; // CMSG_SPACE(sizeof(int)) on x86_64
  const cmsg = new ArrayBuffer(CMSG_SPACE);
  const cmsgView = new DataView(cmsg);
  cmsgView.setUint32(0, 16, true);       // cmsg_len = CMSG_LEN(4)
  cmsgView.setUint32(4, SOL_SOCKET, true);
  cmsgView.setUint32(8, SCM_RIGHTS, true);
  cmsgView.setInt32(12, clientFd, true);  // the fd payload

  // struct msghdr (x86_64): name(8) namelen(4) pad(4) iov(8) iovlen(8) control(8) controllen(8) flags(4)
  const msghdr = new ArrayBuffer(56);
  const msg = new DataView(msghdr);
  msg.setBigUint64(16, BigInt(Bun.ptr(new Uint8Array(iov.buffer))), true); // msg_iov
  msg.setBigUint64(24, 1n, true);                                          // msg_iovlen
  msg.setBigUint64(32, BigInt(Bun.ptr(new Uint8Array(cmsg))), true);       // msg_control
  msg.setBigUint64(40, BigInt(CMSG_SPACE), true);                          // msg_controllen

  if (libc.symbols.sendmsg(channel, ptr(new Uint8Array(msghdr)), 0) < 0)
    throw new Error("sendmsg failed");
}

// Create the fd-channel pair
const [channelTheirs, channelOurs] = socketpair();

// Spawn blit-server with the channel fd
const server = spawn(["blit-server"], {
  env: { ...process.env, BLIT_FD_CHANNEL: String(channelTheirs) },
  stdio: ["inherit", "inherit", "inherit"],
  ipc: undefined,
});
libc.symbols.close(channelTheirs);

// Create a client pair and send one end to the server
const [clientOurs, clientTheirs] = socketpair();
sendFd(channelOurs, clientTheirs);
libc.symbols.close(clientTheirs);

// clientOurs is now a live blit connection -- wrap it with Bun.connect
// and use 4-byte LE length-prefixed framing to exchange messages.
```

The struct layouts above assume x86_64 Linux. If you don't need per-connection mediation (auth gating, connection pooling, sandboxing), connecting directly to `blit-server`'s Unix socket via `Bun.connect({ unix: ... })` is simpler.
