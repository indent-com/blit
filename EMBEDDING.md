# Embedding

There are two distinct dimensions: embedding the frontend into your app, and embedding `blit server` into your own service.

## Your app, our components: `@blit-sh/react` / `@blit-sh/solid`

`@blit-sh/react` and `@blit-sh/solid` are workspace-first. Both are thin wrappers over `@blit-sh/core`'s `BlitTerminalSurface`. A `BlitWorkspace` owns connections, each connection owns terminals, and each `BlitTerminal` renders a terminal by ID.

```tsx
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  useBlitFocusedSession,
  useBlitSessions,
  useBlitWorkspace,
} from "@blit-sh/react";
import { BlitWorkspace, WebSocketTransport } from "@blit-sh/core";
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

Read-only terminals preserve the host terminal dimensions and contain the canvas within the embedding element. The contained canvas remains centered by default; pass `readOnlyObjectPosition="left top"` to align it differently.

### React API

| API                                                    | Purpose                                                  |
| ------------------------------------------------------ | -------------------------------------------------------- |
| `new BlitWorkspace({ wasm, connections })`             | Create a workspace with one or more transports           |
| `BlitWorkspaceProvider`                                | Put the workspace, palette, and font settings in context |
| `useBlitWorkspace()`                                   | Get the imperative workspace object                      |
| `useBlitWorkspaceState()`                              | Read the full reactive workspace snapshot                |
| `useBlitConnection(connectionId?)`                     | Read one connection snapshot                             |
| `useBlitSessions()`                                    | Read all terminals                                       |
| `useBlitFocusedSession()`                              | Read the currently focused terminal                      |
| `useBlitWorkspaceConnection(workspace, id, transport)` | Manage a connection lifecycle with cleanup               |
| `BlitTerminal`                                         | Render one terminal by `sessionId`                       |

### Solid API

```tsx
import {
  BlitTerminal,
  BlitWorkspaceProvider,
  createBlitWorkspace,
  createBlitWorkspaceState,
  createBlitSessions,
  useBlitFocusedSession,
} from "@blit-sh/solid";
import { BlitWorkspace } from "@blit-sh/core";
import { createSignal, onCleanup, createEffect } from "solid-js";

function EmbeddedBlit(props: { wasm: any; passphrase: string }) {
  const workspace = new BlitWorkspace({
    wasm: props.wasm,
    connections: [
      {
        id: "default",
        transport: {
          type: "websocket",
          url: "wss://example.com/blit",
          passphrase: props.passphrase,
        },
      },
    ],
  });
  onCleanup(() => workspace.dispose());

  return (
    <BlitWorkspaceProvider workspace={workspace}>
      <TerminalScreen />
    </BlitWorkspaceProvider>
  );
}

function TerminalScreen() {
  const workspace = createBlitWorkspace();
  const sessions = createBlitSessions();
  const focusedSession = () => useBlitFocusedSession(workspace);

  createEffect(() => {
    if (sessions().length > 0) return;
    workspace.createSession({ connectionId: "default", rows: 24, cols: 80 });
  });

  return (
    <BlitTerminal
      sessionId={focusedSession()?.id ?? null}
      style={{ width: "100%", height: "100vh" }}
    />
  );
}
```

| API                                                       | Purpose                                                  |
| --------------------------------------------------------- | -------------------------------------------------------- |
| `new BlitWorkspace({ wasm, connections })`                | Create a workspace with one or more transports           |
| `BlitWorkspaceProvider`                                   | Put the workspace, palette, and font settings in context |
| `createBlitWorkspace()`                                   | Get the imperative workspace object from context         |
| `createBlitWorkspaceState(workspace?)`                    | Reactive signal tracking the workspace snapshot          |
| `createBlitSessions(workspace?)`                          | Reactive signal tracking all terminals                   |
| `useBlitSession(workspace, sessionId)`                    | Look up a single terminal by ID (non-reactive)           |
| `useBlitFocusedSession(workspace)`                        | Look up the focused terminal (non-reactive)              |
| `useBlitConnection(workspace, sessionId)`                 | Look up a connection snapshot (non-reactive)             |
| `createBlitWorkspaceConnection(workspace, id, transport)` | Manage a connection lifecycle with `onCleanup`           |
| `BlitTerminal`                                            | Render one terminal by `sessionId`                       |

### Wayland surface rendering (experimental)

`BlitSurfaceView` renders a single Wayland surface from a terminal's compositor. The server encodes each surface as H.264 or AV1; the component decodes via WebCodecs and draws to a canvas.

```tsx
import { BlitSurfaceView } from "@blit-sh/react";

function AppWindow({
  connectionId,
  surfaceId,
}: {
  connectionId: string;
  surfaceId: number;
}) {
  return (
    <BlitSurfaceView
      connectionId={connectionId}
      surfaceId={surfaceId}
      style={{ width: 800, height: 600 }}
    />
  );
}
```

Every terminal has an experimental Wayland compositor available. Any command — shell, TUI, or GUI — can open Wayland surfaces:

```tsx
workspace.createSession({
  connectionId: "default",
  rows: 24,
  cols: 80,
  command: "my-gui-app",
});
```

Surfaces created by the terminal appear in the connection's `surfaceStore`, keyed by the terminal's PTY ID. Each surface has a `surfaceId`, `parentId`, `title`, `appId`, `width`, and `height`.

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

## Server-side: a Node/Bun client over a unix socket

You can also run a `@blit-sh/core` client **server-side** (Node/Bun/Deno) to drive a
local `blit server` over its unix-domain socket — e.g. to script terminals or run
headless commands. The non-browser building blocks live under the
`@blit-sh/core/node` subpath (kept out of the package root so `node:net` and
runtime globals never leak into browser bundles):

```ts
import { BlitWorkspace, exitCodeFromStatus, nullLogger } from "@blit-sh/core";
import { NodeUnixSocketTransport, loadBlitWasm } from "@blit-sh/core/node";

// `loadBlitWasm()` initializes the @blit-sh/browser WASM off-browser: it reads
// the colocated blit_browser_bg.wasm from disk and feeds it to init(), so you
// never touch raw wasm bytes. (If you depend on a self-initializing
// `@blit-sh/browser/node` build it is returned as-is.)
const wasm = await loadBlitWasm();

const transport = new NodeUnixSocketTransport(
  process.env.BLIT_SOCK ?? "/tmp/blit.sock",
);
const workspace = new BlitWorkspace({
  wasm,
  logger: nullLogger, // no-op logger; omit to log lifecycle events to console
  connections: [{ id: "default", transport }],
});

const session = await workspace.createSession({
  connectionId: "default",
  rows: 24,
  cols: 80,
  command: "my-command",
});
```

The unix transport speaks blit's framing protocol (4-byte little-endian
length-prefixed frames) for you — there is no need to re-implement the wire
format. `BunUnixSocketTransport` and `DenoUnixSocketTransport` are the
runtime-native equivalents.

### Exit status

When a session's process exits, its `BlitSession.state` becomes `"exited"` and
`BlitSession.exitStatus` carries the raw status from the server:

- `>= 0` — normal exit; the value is the exit code.
- `< 0` — terminated by a signal; the value is the negated signal number.
- `EXIT_STATUS_UNKNOWN` — not yet collected.

`exitCodeFromStatus(status)` maps that to a conventional shell exit code
(unknown → `1`, signalled → `128 + signal`), and `formatExitStatus(status)`
renders `"exited(<code>)"` / `"signal(<n>)"`. Both mirror the `blit` CLI.

```ts
import { exitCodeFromStatus } from "@blit-sh/core";

workspace.subscribe(() => {
  for (const s of workspace.getSnapshot().sessions) {
    if (s.state === "exited" && s.exitStatus !== null) {
      console.log(`${s.id} exited with code`, exitCodeFromStatus(s.exitStatus));
    }
  }
});
```

## Your service, our server: `fd-channel` mode

`fd-channel` lets an external process own `blit server`'s lifecycle and control which clients connect via `SCM_RIGHTS` fd passing. See [ARCHITECTURE.md](ARCHITECTURE.md) for the protocol details and the working examples:

- [Python](examples/fd-channel-python.py)
- [Bun](examples/fd-channel-bun.ts)
