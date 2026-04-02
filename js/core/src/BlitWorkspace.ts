import {
  BlitConnection,
  type CreateBlitConnectionOptions,
} from "./BlitConnection";
import type {
  BlitConnectionSnapshot,
  BlitSearchResult,
  BlitSession,
  BlitTransport,
  BlitWorkspaceSnapshot,
  ConnectionId,
  SessionId,
  TransportConfig,
} from "./types";
import type { BlitWasmModule } from "./TerminalStore";
import { WebSocketTransport } from "./transports/websocket";
import { WebTransportTransport } from "./transports/webtransport";
import { createShareTransport } from "./transports/webrtc-share";

export interface AddBlitConnectionOptions extends Omit<
  CreateBlitConnectionOptions,
  "wasm" | "transport"
> {
  transport?: BlitTransport | TransportConfig;
  wasm?: BlitWasmModule | Promise<BlitWasmModule>;
}

export interface CreateBlitWorkspaceOptions {
  wasm: BlitWasmModule | Promise<BlitWasmModule>;
  connections?: AddBlitConnectionOptions[];
}

export interface CreateWorkspaceSessionOptions {
  connectionId: ConnectionId;
  rows: number;
  cols: number;
  tag?: string;
  command?: string;
  cwdFromSessionId?: SessionId;
}

export interface ResizeWorkspaceSessionOptions {
  sessionId: SessionId;
  rows: number;
  cols: number;
}

function workspaceError(message: string): Error {
  return new Error(message);
}

export class BlitWorkspace {
  private readonly listeners = new Set<() => void>();
  private readonly connectionListeners = new Map<ConnectionId, () => void>();
  private readonly connections = new Map<ConnectionId, BlitConnection>();
  private readonly defaultWasm: BlitWasmModule | Promise<BlitWasmModule>;

  private snapshot: BlitWorkspaceSnapshot = {
    connections: [],
    sessions: [],
    focusedSessionId: null,
    ready: false,
  };

  constructor({ wasm, connections = [] }: CreateBlitWorkspaceOptions) {
    this.defaultWasm = wasm;
    for (const connection of connections) {
      this.addConnection(connection);
    }
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): BlitWorkspaceSnapshot => this.snapshot;

  addConnection(options: AddBlitConnectionOptions): BlitConnection {
    if (this.connections.has(options.id)) {
      throw workspaceError(`Connection ${options.id} already exists`);
    }

    const transport = resolveTransport(options.transport);
    const connection = new BlitConnection({
      ...options,
      transport,
      wasm: options.wasm ?? this.defaultWasm,
    });
    this.connections.set(options.id, connection);
    this.connectionListeners.set(
      options.id,
      connection.subscribe(() => this.recomputeSnapshot()),
    );
    this.recomputeSnapshot();
    return connection;
  }

  removeConnection(connectionId: ConnectionId): void {
    const connection = this.connections.get(connectionId);
    if (!connection) return;

    this.connectionListeners.get(connectionId)?.();
    this.connectionListeners.delete(connectionId);
    this.connections.delete(connectionId);
    connection.close();
    connection.dispose();
    this.recomputeSnapshot();
  }

  dispose(): void {
    for (const connectionId of [...this.connections.keys()]) {
      this.removeConnection(connectionId);
    }
    this.listeners.clear();
  }

  getConnection(connectionId: ConnectionId): BlitConnection | null {
    return this.connections.get(connectionId) ?? null;
  }

  getConnectionSnapshot(
    connectionId: ConnectionId,
  ): BlitConnectionSnapshot | null {
    return (
      this.snapshot.connections.find(
        (connection) => connection.id === connectionId,
      ) ?? null
    );
  }

  getSession(sessionId: SessionId): BlitSession | null {
    return (
      this.snapshot.sessions.find((session) => session.id === sessionId) ?? null
    );
  }

  async createSession(
    options: CreateWorkspaceSessionOptions,
  ): Promise<BlitSession> {
    const connection = this.requireConnection(options.connectionId);
    if (options.cwdFromSessionId) {
      const sourceSession = this.requireSession(options.cwdFromSessionId);
      if (sourceSession.connectionId !== options.connectionId) {
        throw workspaceError(
          `Cannot create a session in ${options.connectionId} from session ${options.cwdFromSessionId}`,
        );
      }
    }
    const session = await connection.createSession({
      rows: options.rows,
      cols: options.cols,
      tag: options.tag,
      command: options.command,
      cwdFromSessionId: options.cwdFromSessionId,
    });
    return session;
  }

  async closeSession(sessionId: SessionId): Promise<void> {
    const session = this.requireSession(sessionId);
    const connection = this.requireConnection(session.connectionId);
    await connection.closeSession(sessionId);
  }

  restartSession(sessionId: SessionId): void {
    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).restartSession(sessionId);
  }

  killSession(sessionId: SessionId, signal = 15): void {
    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).killSession(sessionId, signal);
  }

  focusSession(sessionId: SessionId | null): void {
    if (sessionId === null) {
      this.snapshot = {
        ...this.snapshot,
        focusedSessionId: null,
      };
      this.emit();
      return;
    }

    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).focusSession(sessionId);
    if (this.snapshot.focusedSessionId !== sessionId) {
      this.snapshot = {
        ...this.snapshot,
        focusedSessionId: sessionId,
      };
      this.emit();
    }
  }

  reconnectConnection(connectionId: ConnectionId): void {
    this.requireConnection(connectionId).reconnect();
  }

  sendInput(sessionId: SessionId, data: Uint8Array): void {
    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).sendInput(sessionId, data);
  }

  resizeSession(sessionId: SessionId, rows: number, cols: number): void {
    this.resizeSessions([{ sessionId, rows, cols }]);
  }

  clearSessionSize(sessionId: SessionId): void {
    this.clearSessionSizes([sessionId]);
  }

  clearSessionSizes(sessionIds: Iterable<SessionId>): void {
    const sessionIdsByConnection = new Map<ConnectionId, SessionId[]>();
    for (const sessionId of sessionIds) {
      const session = this.getSession(sessionId);
      if (!session) continue;
      let bucket = sessionIdsByConnection.get(session.connectionId);
      if (!bucket) {
        bucket = [];
        sessionIdsByConnection.set(session.connectionId, bucket);
      }
      bucket.push(sessionId);
    }
    for (const [connectionId, bucket] of sessionIdsByConnection) {
      this.requireConnection(connectionId).clearSessionSizes(bucket);
    }
  }

  resizeSessions(entries: Iterable<ResizeWorkspaceSessionOptions>): void {
    const entriesByConnection = new Map<
      ConnectionId,
      ResizeWorkspaceSessionOptions[]
    >();
    for (const entry of entries) {
      const session = this.getSession(entry.sessionId);
      if (!session) continue;
      let bucket = entriesByConnection.get(session.connectionId);
      if (!bucket) {
        bucket = [];
        entriesByConnection.set(session.connectionId, bucket);
      }
      bucket.push(entry);
    }
    for (const [connectionId, bucket] of entriesByConnection) {
      this.requireConnection(connectionId).resizeSessions(bucket);
    }
  }

  scrollSession(sessionId: SessionId, offset: number): void {
    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).scrollSession(
      sessionId,
      offset,
    );
  }

  sendMouse(
    sessionId: SessionId,
    type: number,
    button: number,
    col: number,
    row: number,
  ): void {
    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).sendMouse(
      sessionId,
      type,
      button,
      col,
      row,
    );
  }

  async search(
    query: string,
    scope?: { connectionId?: ConnectionId },
  ): Promise<BlitSearchResult[]> {
    const trimmed = query.trim();
    if (trimmed.length === 0) return [];

    if (scope?.connectionId) {
      return this.requireConnection(scope.connectionId).search(trimmed);
    }

    const results = await Promise.all(
      [...this.connections.values()].map(async (connection) => {
        try {
          return await connection.search(trimmed);
        } catch {
          return [];
        }
      }),
    );
    return results.flat().sort((left, right) => right.score - left.score);
  }

  setVisibleSessions(sessionIds: Iterable<SessionId>): void {
    const desiredByConnection = new Map<ConnectionId, Set<SessionId>>();

    for (const sessionId of sessionIds) {
      const session = this.getSession(sessionId);
      if (!session) continue;
      let set = desiredByConnection.get(session.connectionId);
      if (!set) {
        set = new Set<SessionId>();
        desiredByConnection.set(session.connectionId, set);
      }
      set.add(sessionId);
    }

    for (const [connectionId, connection] of this.connections) {
      connection.setVisibleSessionIds(
        desiredByConnection.get(connectionId) ?? [],
      );
    }
  }

  getConnectionDebugStats(
    connectionId: ConnectionId,
    sessionId: SessionId | null,
  ): ReturnType<BlitConnection["getDebugStats"]> | null {
    return this.connections.get(connectionId)?.getDebugStats(sessionId) ?? null;
  }

  private emit(): void {
    for (const listener of this.listeners) listener();
  }

  private recomputeSnapshot(): void {
    const connections = [...this.connections.values()].map((connection) =>
      connection.getSnapshot(),
    );
    const sessions = connections.flatMap((connection) => connection.sessions);
    const focusedSessionId = this.resolveFocusedSessionId(
      connections,
      sessions,
    );
    this.snapshot = {
      connections,
      sessions,
      focusedSessionId,
      ready:
        connections.length > 0 &&
        connections.every((connection) => connection.ready),
    };
    this.emit();
  }

  private resolveFocusedSessionId(
    connections: readonly BlitConnectionSnapshot[],
    sessions: readonly BlitSession[],
  ): SessionId | null {
    if (this.snapshot.focusedSessionId) {
      const focused = sessions.find(
        (session) => session.id === this.snapshot.focusedSessionId,
      );
      if (focused && focused.state !== "closed") {
        return focused.id;
      }
    }

    for (const connection of connections) {
      if (!connection.focusedSessionId) continue;
      const focused = sessions.find(
        (session) => session.id === connection.focusedSessionId,
      );
      if (focused && focused.state !== "closed") {
        return focused.id;
      }
    }

    return sessions.find((session) => session.state !== "closed")?.id ?? null;
  }

  private requireConnection(connectionId: ConnectionId): BlitConnection {
    const connection = this.connections.get(connectionId);
    if (!connection) {
      throw workspaceError(`Unknown connection ${connectionId}`);
    }
    return connection;
  }

  private requireSession(sessionId: SessionId): BlitSession {
    const session = this.getSession(sessionId);
    if (!session) {
      throw workspaceError(`Unknown session ${sessionId}`);
    }
    return session;
  }
}

export function createBlitWorkspace(
  options: CreateBlitWorkspaceOptions,
): BlitWorkspace {
  return new BlitWorkspace(options);
}

function isTransportConfig(
  value: BlitTransport | TransportConfig | undefined,
): value is TransportConfig {
  return value != null && "type" in value && typeof (value as TransportConfig).type === "string";
}

function resolveTransport(
  config: BlitTransport | TransportConfig | undefined,
): BlitTransport {
  if (config == null) {
    throw workspaceError("transport or TransportConfig is required");
  }
  if (!isTransportConfig(config)) {
    return config;
  }
  switch (config.type) {
    case "websocket":
      return new WebSocketTransport(config.url, config.passphrase, config.options);
    case "webtransport":
      return new WebTransportTransport(config.url, config.passphrase, config.options);
    case "share":
      return createShareTransport(config.hubUrl, config.passphrase, config.debug);
    case "custom":
      return config.transport;
  }
}
