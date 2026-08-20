import {
  BlitConnection,
  type AwaitSessionExitOptions,
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
import type {
  FsFileIndex,
  FsGrepOptions,
  FsGrepResult,
  FsSyncHandle,
  FsSyncOptions,
} from "./fs";
import type {
  GitDiscoverOptions,
  GitFoundRepo,
  GitOpenOptions,
  GitRepoHandle,
} from "./git";
import type {
  KvFetchResult,
  KvPutOptions,
  KvWatchHandle,
  KvWatchOptions,
} from "./kv";
import type { LspHandle, LspOpenOptions } from "./lsp";
import { WebSocketTransport } from "./transports/websocket";
import { WebTransportTransport } from "./transports/webtransport";
import { createShareTransport } from "./transports/webrtc-share";
import { BlitActivityStore } from "./activity";

export interface AddBlitConnectionOptions extends Omit<
  CreateBlitConnectionOptions,
  "wasm" | "transport"
> {
  transport?: BlitTransport | TransportConfig;
  wasm?: BlitWasmModule | Promise<BlitWasmModule>;
}

/** Logger interface for workspace lifecycle events. */
export interface BlitLogger {
  /** Called for informational events (subscribe, unsubscribe, connect, etc.). */
  info(msg: string, ...args: unknown[]): void;
  /** Called for warnings (decode errors, transport issues, etc.). */
  warn(msg: string, ...args: unknown[]): void;
}

/** Default logger that writes to the console. */
export const consoleLogger: BlitLogger = {
  info: (msg, ...args) => console.log(`[blit] ${msg}`, ...args),
  warn: (msg, ...args) => console.warn(`[blit] ${msg}`, ...args),
};

/** Silent logger that discards everything. */
export const nullLogger: BlitLogger = {
  info: () => {},
  warn: () => {},
};

export interface CreateBlitWorkspaceOptions {
  wasm: BlitWasmModule | Promise<BlitWasmModule>;
  connections?: AddBlitConnectionOptions[];
  logger?: BlitLogger;
}

export interface CreateWorkspaceSessionOptions {
  connectionId: ConnectionId;
  rows: number;
  cols: number;
  tag?: string;
  /** Run this through the target server's login shell. */
  command?: string;
  /** Exec this argv directly, no shell. Needs `FEATURE_CREATE_EXEC`. */
  argv?: readonly string[];
  cwdFromSessionId?: SessionId;
  cwd?: string;
  /** Environment overrides for the child. Needs `FEATURE_CREATE_EXEC`. */
  env?: Readonly<Record<string, string>>;
  /** Server-enforced lifetime, armed at creation. Needs
   *  `FEATURE_PTY_DEADLINE`. */
  deadlineMs?: number;
  /** Whether the creating connection should immediately receive terminal
   *  frame updates. Defaults to true. */
  subscribe?: boolean;
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
  private readonly termCwdListeners = new Set<
    (sessionId: SessionId, cwd: string) => void
  >();
  private readonly termCwdUnsubs = new Map<ConnectionId, () => void>();
  private readonly connections = new Map<ConnectionId, BlitConnection>();
  private readonly defaultWasm: BlitWasmModule | Promise<BlitWasmModule>;
  private surfaceDiagnosticsEnabled = false;
  /** Slow operations surfaced by shell chrome such as the status bar. */
  readonly activities = new BlitActivityStore();
  readonly logger: BlitLogger;

  private snapshot: BlitWorkspaceSnapshot = {
    connections: [],
    sessions: [],
    focusedSessionId: null,
    ready: false,
  };

  constructor({ wasm, connections = [], logger }: CreateBlitWorkspaceOptions) {
    this.defaultWasm = wasm;
    this.logger = logger ?? consoleLogger;
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
      logger: this.logger,
    });
    connection.surfaceStore.setDiagnosticsEnabled(
      this.surfaceDiagnosticsEnabled,
    );
    this.connections.set(options.id, connection);
    this.connectionListeners.set(
      options.id,
      connection.subscribe(() => this.recomputeSnapshot()),
    );
    this.termCwdUnsubs.set(
      options.id,
      connection.onTermCwd((sessionId, cwd) => {
        for (const listener of this.termCwdListeners) listener(sessionId, cwd);
      }),
    );
    this.recomputeSnapshot();
    return connection;
  }

  removeConnection(connectionId: ConnectionId): void {
    const connection = this.connections.get(connectionId);
    if (!connection) return;

    this.connectionListeners.get(connectionId)?.();
    this.connectionListeners.delete(connectionId);
    this.termCwdUnsubs.get(connectionId)?.();
    this.termCwdUnsubs.delete(connectionId);
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
    this.activities.clear();
  }

  getConnection(connectionId: ConnectionId): BlitConnection | null {
    return this.connections.get(connectionId) ?? null;
  }

  /** Drop terminal view-size registrations while keeping connections alive. */
  resetViewSizes(): void {
    for (const connection of this.connections.values()) {
      connection.resetViewSizes();
    }
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
      argv: options.argv,
      cwdFromSessionId: options.cwdFromSessionId,
      cwd: options.cwd,
      env: options.env,
      deadlineMs: options.deadlineMs,
      subscribe: options.subscribe,
    });
    return session;
  }

  async closeSession(sessionId: SessionId): Promise<void> {
    const session = this.requireSession(sessionId);
    const connection = this.requireConnection(session.connectionId);
    await connection.closeSession(sessionId);
  }

  /** Wait for a session to exit or close, preserving its final exit status. */
  async awaitSessionExit(
    sessionId: SessionId,
    options?: AwaitSessionExitOptions,
  ): Promise<BlitSession> {
    const session = this.requireSession(sessionId);
    return this.requireConnection(session.connectionId).awaitSessionExit(
      sessionId,
      options,
    );
  }

  closeSurface(connectionId: ConnectionId, surfaceId: number): void {
    const connection = this.requireConnection(connectionId);
    connection.sendSurfaceClose(surfaceId);
  }

  /**
   * Mirror a directory tree from one connection's server (docs/fs-watch.md):
   * a live map plus per-record callbacks. See `BlitConnection.syncFs`.
   */
  async syncFs(
    connectionId: ConnectionId,
    path: string,
    options?: FsSyncOptions,
  ): Promise<FsSyncHandle> {
    return this.requireConnection(connectionId).syncFs(path, options);
  }

  /**
   * Open a git repository on one connection's server (docs/git.md): live
   * state plus oid-addressed reads. See `BlitConnection.openRepo`.
   */
  async openRepo(
    connectionId: ConnectionId,
    path: string,
    options?: GitOpenOptions,
  ): Promise<GitRepoHandle> {
    return this.requireConnection(connectionId).openRepo(path, options);
  }

  /**
   * Repositories under `path` on one connection's server — "what is checked
   * out here" in one call, rather than a ladder of candidate paths probed
   * with an `FS_SYNC` per level. Allocates no repo id, so it costs nothing
   * against the per-connection repo budget. See
   * `BlitConnection.discoverRepos`.
   */
  async discoverRepos(
    connectionId: ConnectionId,
    path: string,
    options?: GitDiscoverOptions,
  ): Promise<GitFoundRepo[]> {
    return this.requireConnection(connectionId).discoverRepos(path, options);
  }

  /** Fuzzy file search under `root` on one connection; up to `limit` matches,
   *  best first. See `BlitConnection.searchFiles`. */
  async searchFiles(
    connectionId: ConnectionId,
    root: string,
    query: string,
    limit?: number,
  ): Promise<string[]> {
    return this.requireConnection(connectionId).searchFiles(root, query, limit);
  }

  /** Candidate file list under `root` on one connection, for client-side
   *  fuzzy search. See `BlitConnection.indexFiles`. */
  async indexFiles(
    connectionId: ConnectionId,
    root: string,
  ): Promise<FsFileIndex> {
    return this.requireConnection(connectionId).indexFiles(root);
  }

  /** Content search under `root` on one connection, hits grouped by file
   *  (tracked first, gitignored last). See `BlitConnection.grep`. */
  async grep(
    connectionId: ConnectionId,
    root: string,
    query: string,
    opts?: FsGrepOptions,
  ): Promise<FsGrepResult> {
    return this.requireConnection(connectionId).grep(root, query, opts);
  }

  /** A session's live working directory (server reads the pty's cwd). "" when
   *  unavailable. See `BlitConnection.sessionCwd`. */
  async sessionCwd(
    connectionId: ConnectionId,
    sessionId: SessionId,
  ): Promise<string> {
    return this.requireConnection(connectionId).sessionCwd(sessionId);
  }

  /** Subscribe to server-pushed cwd changes across every connection
   *  (`S2C_TERM_CWD_EVENT`, docs/protocol.md): consumers can suppress
   *  `sessionCwd` polling while pushes flow. Returns an unsubscribe
   *  function. */
  onTermCwd(listener: (sessionId: SessionId, cwd: string) => void): () => void {
    this.termCwdListeners.add(listener);
    return () => {
      this.termCwdListeners.delete(listener);
    };
  }

  /** The most recent server-pushed cwd for a session, or null when none
   *  has arrived since (re)connect. See `BlitConnection.lastPushedCwd`. */
  lastPushedCwd(sessionId: SessionId): string | null {
    const session = this.getSession(sessionId);
    if (!session) return null;
    return (
      this.connections.get(session.connectionId)?.lastPushedCwd(sessionId) ??
      null
    );
  }

  /**
   * Attach language intelligence on one connection's server (docs/lsp.md):
   * live server state + diagnostics plus point-in-time queries. See
   * `BlitConnection.openLsp`.
   */
  async openLsp(
    connectionId: ConnectionId,
    path: string,
    options?: LspOpenOptions,
  ): Promise<LspHandle> {
    return this.requireConnection(connectionId).openLsp(path, options);
  }

  /** CAS put into one connection's server KV store (docs/design/kv.md).
   *  See `BlitConnection.kvPut`. */
  async kvPut(
    connectionId: ConnectionId,
    key: string,
    value: Uint8Array,
    options?: KvPutOptions,
  ): Promise<{ hash: bigint; mtimeNs: bigint }> {
    return this.requireConnection(connectionId).kvPut(key, value, options);
  }

  /** Delete a key from one connection's server KV store.
   *  See `BlitConnection.kvDelete`. */
  async kvDelete(
    connectionId: ConnectionId,
    key: string,
    options?: { ifHash?: bigint },
  ): Promise<void> {
    return this.requireConnection(connectionId).kvDelete(key, options);
  }

  /** Fetch one value from one connection's server KV store; null when
   *  absent. See `BlitConnection.kvFetch`. */
  async kvFetch(
    connectionId: ConnectionId,
    key: string,
  ): Promise<KvFetchResult | null> {
    return this.requireConnection(connectionId).kvFetch(key);
  }

  /** Watch a literal byte prefix of one connection's server KV store.
   *  See `BlitConnection.watchKv`. */
  async watchKv(
    connectionId: ConnectionId,
    prefix: string,
    options?: KvWatchOptions,
  ): Promise<KvWatchHandle> {
    return this.requireConnection(connectionId).watchKv(prefix, options);
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

  /** Move a scrolled view by `lines` instead of to a position; `offset` is
   *  the caller's idea of where that lands.  See
   *  {@link BlitConnection.scrollSessionBy}. */
  scrollSessionBy(sessionId: SessionId, offset: number, lines: number): void {
    const session = this.getSession(sessionId);
    if (!session) return;
    this.requireConnection(session.connectionId).scrollSessionBy(
      sessionId,
      offset,
      lines,
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

  /** Enable the per-frame histories used only by the UI debug pane. */
  setSurfaceDiagnosticsEnabled(enabled: boolean): void {
    if (enabled === this.surfaceDiagnosticsEnabled) return;
    this.surfaceDiagnosticsEnabled = enabled;
    for (const connection of this.connections.values()) {
      connection.surfaceStore.setDiagnosticsEnabled(enabled);
    }
  }

  getConnectionDebugStats(
    connectionId: ConnectionId,
    sessionId: SessionId | null,
  ): ReturnType<BlitConnection["getDebugStats"]> | null {
    const stats = this.connections.get(connectionId)?.getDebugStats(sessionId);
    if (!stats) return null;
    // Aggregate surfaces from *all* connections so the debug panel shows
    // every surface regardless of which connection owns it.
    const allSurfaces: typeof stats.surfaces = [];
    for (const conn of this.connections.values()) {
      allSurfaces.push(...conn.surfaceStore.getDebugStats());
    }
    return { ...stats, surfaces: allSurfaces };
  }

  private emit(): void {
    for (const listener of this.listeners) listener();
  }

  private _syncingFocus = false;

  private recomputeSnapshot(): void {
    const connections = [...this.connections.values()].map((connection) =>
      connection.getSnapshot(),
    );
    const sessions = connections.flatMap((connection) => connection.sessions);
    const focusedSessionId = this.resolveFocusedSessionId(
      connections,
      sessions,
    );
    const previousFocusedSessionId = this.snapshot.focusedSessionId;
    this.snapshot = {
      connections,
      sessions,
      focusedSessionId,
      ready:
        connections.length > 0 &&
        connections.every((connection) => connection.ready),
    };
    this.emit();

    // When the workspace resolves focus via fallback (e.g. initial connect or
    // session close), sync it to the owning connection so C2S_FOCUS reaches
    // the server and client.lead is set correctly.
    if (
      focusedSessionId &&
      focusedSessionId !== previousFocusedSessionId &&
      !this._syncingFocus
    ) {
      this._syncingFocus = true;
      try {
        const session = sessions.find((s) => s.id === focusedSessionId);
        if (session) {
          this.connections
            .get(session.connectionId)
            ?.focusSession(focusedSessionId);
        }
      } finally {
        this._syncingFocus = false;
      }
    }
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
      // The old session ID is gone or closed — try to find a live session
      // for the same underlying PTY so focus survives reconnects.
      if (focused && focused.state === "closed") {
        const replacement = sessions.find(
          (s) =>
            s.state !== "closed" &&
            s.connectionId === focused.connectionId &&
            s.ptyId === focused.ptyId,
        );
        if (replacement) return replacement.id;
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
  return (
    value != null &&
    "type" in value &&
    typeof (value as TransportConfig).type === "string"
  );
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
      return new WebSocketTransport(
        config.url,
        config.passphrase,
        config.options,
      );
    case "webtransport":
      return new WebTransportTransport(
        config.url,
        config.passphrase,
        config.options,
      );
    case "share":
      return createShareTransport(
        config.hubUrl,
        config.passphrase,
        config.debug,
      );
    case "custom":
      return config.transport;
  }
}
