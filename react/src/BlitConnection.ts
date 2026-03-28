import type {
  BlitConnectionSnapshot,
  BlitSearchResult,
  BlitSession,
  BlitTransport,
  ConnectionId,
  ConnectionStatus,
  SessionId,
} from "./types";
import {
  FEATURE_CREATE_NONCE,
  FEATURE_RESTART,
  PROTOCOL_VERSION,
  S2C_CLOSED,
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_EXITED,
  S2C_HELLO,
  S2C_LIST,
  S2C_SEARCH_RESULTS,
  S2C_TITLE,
  S2C_UPDATE,
} from "./types";
import {
  buildCloseMessage,
  buildCreate2Message,
  buildFocusMessage,
  buildInputMessage,
  buildMouseMessage,
  buildResizeMessage,
  buildRestartMessage,
  buildScrollMessage,
  buildSearchMessage,
} from "./protocol";
import { TerminalStore, type BlitWasmModule } from "./TerminalStore";

const textDecoder = new TextDecoder();

export const SEARCH_SOURCE_TITLE = 0;
export const SEARCH_SOURCE_VISIBLE = 1;
export const SEARCH_SOURCE_SCROLLBACK = 2;
export const SEARCH_MATCH_TITLE = 1 << 0;
export const SEARCH_MATCH_VISIBLE = 1 << 1;
export const SEARCH_MATCH_SCROLLBACK = 1 << 2;

export interface CreateBlitConnectionOptions {
  id: ConnectionId;
  transport: BlitTransport;
  wasm: BlitWasmModule | Promise<BlitWasmModule>;
  autoConnect?: boolean;
}

export interface CreateSessionOptions {
  rows: number;
  cols: number;
  tag?: string;
  command?: string;
  cwdFromPtyId?: number;
}

type PendingCreate = {
  resolve: (session: BlitSession) => void;
  reject: (error: Error) => void;
};

type PendingSearch = {
  resolve: (results: BlitSearchResult[]) => void;
  reject: (error: Error) => void;
};

function connectionError(message: string): Error {
  return new Error(message);
}

function isLiveSession(session: BlitSession): boolean {
  return session.state === "creating" || session.state === "active" || session.state === "exited";
}

export class BlitConnection {
  readonly id: ConnectionId;

  private readonly transport: BlitTransport;
  private readonly store: TerminalStore;

  private readonly listeners = new Set<() => void>();
  private readonly sessionsById = new Map<SessionId, BlitSession>();
  private readonly currentSessionIdByPtyId = new Map<number, SessionId>();
  private readonly pendingCreates = new Map<number, PendingCreate>();
  private readonly pendingCloses = new Map<SessionId, Array<() => void>>();
  private readonly pendingSearches = new Map<number, PendingSearch>();

  private sessionCounter = 0;
  private nonceCounter = 0;
  private searchCounter = 0;
  private features = 0;
  private disposed = false;
  private hasConnected = false;
  private retryCount = 0;

  private snapshot: BlitConnectionSnapshot;
  private sessions: BlitSession[] = [];

  constructor({
    id,
    transport,
    wasm,
    autoConnect = true,
  }: CreateBlitConnectionOptions) {
    this.id = id;
    this.transport = transport;
    this.store = new TerminalStore(
      {
        send: (data) => {
          if (this.transport.status === "connected") {
            this.transport.send(data);
          }
        },
        getStatus: () => this.transport.status,
      },
      wasm,
    );
    this.snapshot = {
      id,
      status: transport.status,
      ready: false,
      supportsRestart: false,
      retryCount: 0,
      error: null,
      sessions: [],
      focusedSessionId: null,
    };

    if (transport.status === "connected") {
      this.hasConnected = true;
    }

    this.transport.addEventListener("message", this.handleMessage);
    this.transport.addEventListener("statuschange", this.handleStatusChange);
    this.store.handleStatusChange(this.transport.status);

    if (autoConnect) {
      this.connect();
    }
  }

  subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };

  getSnapshot = (): BlitConnectionSnapshot => this.snapshot;

  connect(): void {
    if (this.disposed) return;
    this.transport.connect();
  }

  reconnect(): void {
    this.connect();
  }

  close(): void {
    this.transport.close();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.transport.removeEventListener("message", this.handleMessage);
    this.transport.removeEventListener("statuschange", this.handleStatusChange);
    this.rejectPendingCreates(
      connectionError("Connection disposed before PTY creation completed"),
    );
    this.rejectPendingSearches(connectionError("Connection disposed"));
    this.resolveAllPendingCloses();
    this.store.destroy();
  }

  setVisibleSessionIds(sessionIds: Iterable<SessionId>): void {
    const desired = new Set<number>();
    for (const sessionId of sessionIds) {
      const session = this.sessionsById.get(sessionId);
      if (session && session.state !== "closed") {
        desired.add(session.ptyId);
      }
    }
    this.store.setDesiredSubscriptions(desired);
  }

  getSession(sessionId: SessionId): BlitSession | null {
    return this.sessionsById.get(sessionId) ?? null;
  }

  getDebugStats(sessionId: SessionId | null): ReturnType<TerminalStore["getDebugStats"]> {
    const session = sessionId ? this.sessionsById.get(sessionId) : null;
    return this.store.getDebugStats(session?.ptyId ?? null);
  }

  async createSession(options: CreateSessionOptions): Promise<BlitSession> {
    if (this.transport.status !== "connected") {
      throw connectionError(
        `Cannot create PTY while transport is ${this.transport.status}`,
      );
    }

    return new Promise<BlitSession>((resolve, reject) => {
      let nonce = 0;
      do {
        nonce = (this.nonceCounter = (this.nonceCounter + 1) & 0xffff);
      } while (this.pendingCreates.has(nonce));

      this.pendingCreates.set(nonce, { resolve, reject });
      this.transport.send(
        buildCreate2Message(nonce, options.rows, options.cols, {
          tag: options.tag,
          command: options.command,
          srcPtyId: options.cwdFromPtyId,
        }),
      );
    });
  }

  async closeSession(sessionId: SessionId): Promise<void> {
    const session = this.sessionsById.get(sessionId);
    if (!session || session.state === "closed") return;
    if (this.transport.status !== "connected") return;

    return new Promise<void>((resolve) => {
      const resolvers = this.pendingCloses.get(sessionId);
      if (resolvers) {
        resolvers.push(resolve);
      } else {
        this.pendingCloses.set(sessionId, [resolve]);
      }
      this.transport.send(buildCloseMessage(session.ptyId));
    });
  }

  restartSession(sessionId: SessionId): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || session.state === "closed" || this.transport.status !== "connected") {
      return;
    }
    this.transport.send(buildRestartMessage(session.ptyId));
  }

  focusSession(sessionId: SessionId | null): void {
    if (sessionId === null) {
      if (this.snapshot.focusedSessionId !== null) {
        this.snapshot = {
          ...this.snapshot,
          focusedSessionId: null,
        };
        this.store.setLead(null);
        this.emit();
      }
      return;
    }

    const session = this.sessionsById.get(sessionId);
    if (!session || session.state === "closed") return;
    const changed = this.snapshot.focusedSessionId !== sessionId;
    this.snapshot = {
      ...this.snapshot,
      focusedSessionId: sessionId,
    };
    this.store.setLead(session.ptyId);
    if (this.transport.status === "connected") {
      this.transport.send(buildFocusMessage(session.ptyId));
    }
    if (changed) {
      this.emit();
    }
  }

  sendInput(sessionId: SessionId, data: Uint8Array): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || !isLiveSession(session) || this.transport.status !== "connected") {
      return;
    }
    this.transport.send(buildInputMessage(session.ptyId, data));
  }

  resizeSession(sessionId: SessionId, rows: number, cols: number): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || !isLiveSession(session) || this.transport.status !== "connected") {
      return;
    }
    this.transport.send(buildResizeMessage(session.ptyId, rows, cols));
  }

  scrollSession(sessionId: SessionId, offset: number): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || !isLiveSession(session) || this.transport.status !== "connected") {
      return;
    }
    this.transport.send(buildScrollMessage(session.ptyId, offset));
  }

  sendMouse(
    sessionId: SessionId,
    type: number,
    button: number,
    col: number,
    row: number,
  ): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || !isLiveSession(session) || this.transport.status !== "connected") {
      return;
    }
    this.transport.send(buildMouseMessage(session.ptyId, type, button, col, row));
  }

  async search(query: string): Promise<BlitSearchResult[]> {
    if (this.transport.status !== "connected") {
      throw connectionError(
        `Cannot search while transport is ${this.transport.status}`,
      );
    }

    return new Promise<BlitSearchResult[]>((resolve, reject) => {
      let requestId = 0;
      do {
        requestId = (this.searchCounter = (this.searchCounter + 1) & 0xffff);
      } while (this.pendingSearches.has(requestId));

      this.pendingSearches.set(requestId, { resolve, reject });
      this.transport.send(buildSearchMessage(requestId, query));
    });
  }

  getTerminal(ptyId: number) {
    return this.store.getTerminal(ptyId);
  }

  getStore(): TerminalStore {
    return this.store;
  }

  isReady(): boolean {
    return this.store.isReady();
  }

  onReady(listener: () => void): () => void {
    return this.store.onReady(listener);
  }

  private emit(): void {
    for (const listener of this.listeners) listener();
  }

  private handleMessage = (data: ArrayBuffer): void => {
    const bytes = new Uint8Array(data);
    if (bytes.length === 0) return;

    const type = bytes[0];
    switch (type) {
      case S2C_UPDATE: {
        if (bytes.length < 3) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        this.store.handleUpdate(ptyId, bytes.subarray(3));
        this.syncTitleFromTerminal(ptyId);
        return;
      }
      case S2C_CREATED: {
        if (bytes.length < 3) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        const tag = textDecoder.decode(bytes.subarray(3));
        const session = this.upsertLiveSession(ptyId, tag, "active");
        if ((this.features & FEATURE_CREATE_NONCE) === 0 && this.pendingCreates.size > 0) {
          const [firstNonce, pending] = this.pendingCreates.entries().next().value as [
            number,
            PendingCreate,
          ];
          this.pendingCreates.delete(firstNonce);
          pending.resolve(session);
        }
        this.ensureFocusedSession(session.id);
        return;
      }
      case S2C_CREATED_N: {
        if (bytes.length < 5) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const ptyId = bytes[3] | (bytes[4] << 8);
        const tag = textDecoder.decode(bytes.subarray(5));
        const session = this.upsertLiveSession(ptyId, tag, "active");
        const pending = this.pendingCreates.get(nonce);
        if (pending) {
          this.pendingCreates.delete(nonce);
          pending.resolve(session);
        }
        this.ensureFocusedSession(session.id);
        return;
      }
      case S2C_CLOSED: {
        if (bytes.length < 3) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        const sessionId = this.currentSessionIdByPtyId.get(ptyId);
        if (sessionId) {
          this.markSessionClosed(sessionId);
        }
        return;
      }
      case S2C_EXITED: {
        if (bytes.length < 3) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        const sessionId = this.currentSessionIdByPtyId.get(ptyId);
        if (sessionId) {
          this.updateSession(sessionId, { state: "exited" });
        }
        return;
      }
      case S2C_LIST: {
        this.handleListMessage(bytes);
        return;
      }
      case S2C_TITLE: {
        if (bytes.length < 3) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        const sessionId = this.currentSessionIdByPtyId.get(ptyId);
        if (!sessionId) return;
        this.updateSession(sessionId, {
          title: textDecoder.decode(bytes.subarray(3)),
        });
        return;
      }
      case S2C_SEARCH_RESULTS: {
        this.handleSearchResults(bytes);
        return;
      }
      case S2C_HELLO: {
        if (bytes.length < 7) return;
        const version = bytes[1] | (bytes[2] << 8);
        const features =
          bytes[3] | (bytes[4] << 8) | (bytes[5] << 16) | (bytes[6] << 24);
        if (version > PROTOCOL_VERSION) {
          this.transport.close();
          return;
        }
        this.features = features;
        this.snapshot = {
          ...this.snapshot,
          supportsRestart: (features & FEATURE_RESTART) !== 0,
        };
        this.emit();
        return;
      }
      default:
        return;
    }
  };

  private handleStatusChange = (status: ConnectionStatus): void => {
    this.store.handleStatusChange(status);

    // Read transport-level error info if available.
    const transportAny = this.transport as unknown as Record<string, unknown>;
    const lastError =
      status === "error" && typeof transportAny.lastError === "string"
        ? transportAny.lastError
        : null;
    const authRejected =
      status === "error" && transportAny.authRejected === true;

    if (status === "connected") {
      this.hasConnected = true;
      this.retryCount = 0;
    } else if (
      (status === "error" || status === "disconnected") &&
      (this.snapshot.status === "connecting" || this.snapshot.status === "authenticating")
    ) {
      this.retryCount++;
    }

    this.snapshot = {
      ...this.snapshot,
      status,
      retryCount: this.retryCount,
      error: authRejected ? "auth" : lastError,
    };

    if (status === "disconnected" || status === "error") {
      this.rejectPendingCreates(
        connectionError(`Transport ${status} before PTY creation completed`),
      );
      this.rejectPendingSearches(connectionError(`Transport ${status}`));
      this.resolveAllPendingCloses();
    }

    this.emit();
  };

  private handleListMessage(bytes: Uint8Array): void {
    if (bytes.length < 3) return;

    const count = bytes[1] | (bytes[2] << 8);
    const entries: Array<{ ptyId: number; tag: string }> = [];
    let offset = 3;
    for (let index = 0; index < count; index++) {
      if (offset + 4 > bytes.length) break;
      const ptyId = bytes[offset] | (bytes[offset + 1] << 8);
      const tagLen = bytes[offset + 2] | (bytes[offset + 3] << 8);
      offset += 4;
      const tag = textDecoder.decode(bytes.subarray(offset, offset + tagLen));
      offset += tagLen;
      entries.push({ ptyId, tag });
    }

    const livePtys = new Set(entries.map((entry) => entry.ptyId));
    for (const session of this.sessions) {
      if (isLiveSession(session) && !livePtys.has(session.ptyId)) {
        this.markSessionClosed(session.id, false);
      }
    }

    for (const entry of entries) {
      const existingSessionId = this.currentSessionIdByPtyId.get(entry.ptyId);
      const existingSession = existingSessionId
        ? this.sessionsById.get(existingSessionId) ?? null
        : null;
      if (!existingSession || existingSession.state === "closed") {
        this.upsertLiveSession(entry.ptyId, entry.tag, "active");
        continue;
      }
      this.updateSession(existingSession.id, {
        tag: entry.tag,
        state: existingSession.state === "exited" ? "exited" : "active",
      });
    }

    const previousFocus = this.snapshot.focusedSessionId;
    const nextFocus =
      this.snapshot.focusedSessionId &&
      this.sessionsById.get(this.snapshot.focusedSessionId)?.state !== "closed"
        ? this.snapshot.focusedSessionId
        : this.firstLiveSessionId();

    this.snapshot = {
      ...this.snapshot,
      ready: true,
      focusedSessionId: nextFocus,
    };
    this.store.setLead(nextFocus ? this.sessionsById.get(nextFocus)?.ptyId ?? null : null);
    this.emit();

    if (nextFocus && nextFocus !== previousFocus) {
      const session = this.sessionsById.get(nextFocus);
      if (session && this.transport.status === "connected") {
        this.transport.send(buildFocusMessage(session.ptyId));
      }
    }
  }

  private handleSearchResults(bytes: Uint8Array): void {
    if (bytes.length < 5) return;
    const requestId = bytes[1] | (bytes[2] << 8);
    const count = bytes[3] | (bytes[4] << 8);
    const pending = this.pendingSearches.get(requestId);
    if (!pending) return;

    const results: BlitSearchResult[] = [];
    let offset = 5;
    for (let index = 0; index < count; index++) {
      if (offset + 14 > bytes.length) break;
      const ptyId = bytes[offset] | (bytes[offset + 1] << 8);
      const score =
        bytes[offset + 2] |
        (bytes[offset + 3] << 8) |
        (bytes[offset + 4] << 16) |
        ((bytes[offset + 5] << 24) >>> 0);
      const primarySource = bytes[offset + 6];
      const matchedSources = bytes[offset + 7];
      const rawScroll =
        (bytes[offset + 8] |
          (bytes[offset + 9] << 8) |
          (bytes[offset + 10] << 16) |
          (bytes[offset + 11] << 24)) >>> 0;
      const scrollOffset = rawScroll === 0xffffffff ? null : rawScroll;
      const contextLen = bytes[offset + 12] | (bytes[offset + 13] << 8);
      offset += 14;
      const context = textDecoder.decode(bytes.subarray(offset, offset + contextLen));
      offset += contextLen;

      const sessionId = this.currentSessionIdByPtyId.get(ptyId);
      if (!sessionId) continue;

      results.push({
        sessionId,
        connectionId: this.id,
        ptyId,
        score,
        primarySource,
        matchedSources,
        scrollOffset,
        context,
      });
    }

    this.pendingSearches.delete(requestId);
    pending.resolve(results);
  }

  private syncTitleFromTerminal(ptyId: number): void {
    const sessionId = this.currentSessionIdByPtyId.get(ptyId);
    if (!sessionId) return;

    queueMicrotask(() => {
      const currentSessionId = this.currentSessionIdByPtyId.get(ptyId);
      if (currentSessionId !== sessionId) return;
      const terminal = this.store.getTerminal(ptyId);
      if (!terminal) return;
      const title = terminal.title();
      const session = this.sessionsById.get(sessionId);
      if (!session || session.title === title) return;
      this.updateSession(sessionId, { title });
    });
  }

  private upsertLiveSession(
    ptyId: number,
    tag: string,
    state: BlitSession["state"],
  ): BlitSession {
    const currentId = this.currentSessionIdByPtyId.get(ptyId);
    const current = currentId ? this.sessionsById.get(currentId) ?? null : null;
    if (current && current.state !== "closed") {
      return this.updateSession(current.id, { tag, state });
    }

    const session: BlitSession = {
      id: `${this.id}:${++this.sessionCounter}`,
      connectionId: this.id,
      ptyId,
      tag,
      title: current?.title ?? null,
      state,
    };
    this.currentSessionIdByPtyId.set(ptyId, session.id);
    this.sessionsById.set(session.id, session);
    this.sessions = [...this.sessions, session];
    this.snapshot = {
      ...this.snapshot,
      sessions: this.sessions,
    };
    this.emit();
    return session;
  }

  private updateSession(
    sessionId: SessionId,
    patch: Partial<Omit<BlitSession, "id" | "connectionId" | "ptyId">> &
      Partial<Pick<BlitSession, "ptyId">>,
  ): BlitSession {
    const current = this.sessionsById.get(sessionId);
    if (!current) {
      throw connectionError(`Unknown session ${sessionId}`);
    }

    const next: BlitSession = { ...current, ...patch };
    this.sessionsById.set(sessionId, next);
    this.sessions = this.sessions.map((session) =>
      session.id === sessionId ? next : session,
    );
    this.snapshot = {
      ...this.snapshot,
      sessions: this.sessions,
    };
    this.emit();
    return next;
  }

  private markSessionClosed(sessionId: SessionId, emit = true): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || session.state === "closed") return;

    const next: BlitSession = {
      ...session,
      state: "closed",
    };
    this.sessionsById.set(sessionId, next);
    this.sessions = this.sessions.map((entry) =>
      entry.id === sessionId ? next : entry,
    );
    if (this.currentSessionIdByPtyId.get(session.ptyId) === sessionId) {
      this.currentSessionIdByPtyId.delete(session.ptyId);
    }
    this.store.freeTerminal(session.ptyId);

    const focusedWasClosed = this.snapshot.focusedSessionId === sessionId;
    const nextFocus =
      focusedWasClosed
        ? this.firstLiveSessionId()
        : this.snapshot.focusedSessionId;

    this.snapshot = {
      ...this.snapshot,
      sessions: this.sessions,
      focusedSessionId: nextFocus ?? null,
    };
    this.store.setLead(nextFocus ? this.sessionsById.get(nextFocus)?.ptyId ?? null : null);

    const resolvers = this.pendingCloses.get(sessionId);
    if (resolvers) {
      this.pendingCloses.delete(sessionId);
      for (const resolve of resolvers) resolve();
    }

    if (emit) {
      if (focusedWasClosed && nextFocus && this.transport.status === "connected") {
        const nextSession = this.sessionsById.get(nextFocus);
        if (nextSession) {
          this.transport.send(buildFocusMessage(nextSession.ptyId));
        }
      }
      this.emit();
    }
  }

  private ensureFocusedSession(sessionId: SessionId): void {
    if (this.snapshot.focusedSessionId) return;
    this.focusSession(sessionId);
  }

  private firstLiveSessionId(): SessionId | null {
    const session = this.sessions.find((entry) => entry.state !== "closed");
    return session?.id ?? null;
  }

  private rejectPendingCreates(error: Error): void {
    for (const pending of this.pendingCreates.values()) {
      pending.reject(error);
    }
    this.pendingCreates.clear();
  }

  private rejectPendingSearches(error: Error): void {
    for (const pending of this.pendingSearches.values()) {
      pending.reject(error);
    }
    this.pendingSearches.clear();
  }

  private resolveAllPendingCloses(): void {
    for (const resolvers of this.pendingCloses.values()) {
      for (const resolve of resolvers) resolve();
    }
    this.pendingCloses.clear();
  }
}
