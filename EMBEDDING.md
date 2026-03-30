# Embedding

There are two distinct dimensions: embedding the frontend into your app, and embedding `blit-server` into your own service.

## Your app, our components: `blit-react`

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

| API                                        | Purpose                                                  |
| ------------------------------------------ | -------------------------------------------------------- |
| `new BlitWorkspace({ wasm, connections })` | Create a workspace with one or more transports           |
| `BlitWorkspaceProvider`                    | Put the workspace, palette, and font settings in context |
| `useBlitWorkspace()`                       | Get the imperative workspace object                      |
| `useBlitWorkspaceState()`                  | Read the full reactive workspace snapshot                |
| `useBlitConnection(connectionId?)`         | Read one connection snapshot                             |
| `useBlitSessions()`                        | Read all sessions                                        |
| `useBlitFocusedSession()`                  | Read the currently focused session                       |
| `BlitTerminal`                             | Render one session by `sessionId`                        |

### Workspace operations

- `createSession({ connectionId, rows, cols, tag?, command?, cwdFromSessionId? })`
- `closeSession(sessionId)`
- `restartSession(sessionId)`
- `focusSession(sessionId | null)`
- `search(query, { connectionId? })`
- `setVisibleSessions(sessionIds)`
- `addConnection(...)` / `removeConnection(connectionId)` / `reconnectConnection(connectionId)`

### Transports

All transports share a common set of options (`BlitTransportOptions`):

| Option              | Default                          | Description                  |
| ------------------- | -------------------------------- | ---------------------------- |
| `reconnect`         | `true`                           | Auto-reconnect on disconnect |
| `reconnectDelay`    | `500`                            | Initial reconnect delay (ms) |
| `maxReconnectDelay` | `10000`                          | Maximum reconnect delay (ms) |
| `reconnectBackoff`  | `1.5`                            | Backoff multiplier           |
| `connectTimeoutMs`  | none (WS) / `10000` (WT, WebRTC) | Connection timeout (ms)      |

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
  removeEventListener(
    type: "message" | "statuschange",
    listener: Function,
  ): void;
}
```

## Your service, our server: `fd-channel` mode

`fd-channel` lets an external process own `blit-server`'s lifecycle and control which clients connect via `SCM_RIGHTS` fd passing. See [ARCHITECTURE.md](ARCHITECTURE.md) for the protocol details and the working examples:

- [Python](examples/fd-channel-python.py)
- [Bun](examples/fd-channel-bun.ts)
