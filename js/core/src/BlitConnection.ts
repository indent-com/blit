import type {
  BlitConnectionSnapshot,
  BlitSearchResult,
  BlitSession,
  BlitTransport,
  ConnectionId,
  ConnectionStatus,
  SessionId,
  TerminalPalette,
} from "./types";
import {
  FEATURE_COPY_RANGE,
  FEATURE_CREATE_NONCE,
  FEATURE_RESIZE_BATCH,
  FEATURE_RESTART,
  PROTOCOL_VERSION,
  S2C_CLOSED,
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_EXITED,
  S2C_HELLO,
  S2C_LIST,
  S2C_SEARCH_RESULTS,
  S2C_TEXT,
  S2C_TITLE,
  S2C_UPDATE,
} from "./types";
import {
  buildCloseMessage,
  buildClearResizeBatchMessage,
  buildClearResizeMessage,
  buildCopyRangeMessage,
  buildCreate2Message,
  buildFocusMessage,
  buildInputMessage,
  buildMouseMessage,
  buildResizeBatchMessage,
  buildResizeMessage,
  buildKillMessage,
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
  cwdFromSessionId?: SessionId;
}

type ResizeSessionOptions = {
  sessionId: SessionId;
  rows: number;
  cols: number;
};

type PendingCreate = {
  resolve: (session: BlitSession) => void;
  reject: (error: Error) => void;
};

type PendingSearch = {
  resolve: (results: BlitSearchResult[]) => void;
  reject: (error: Error) => void;
};

type InternalSession = BlitSession;

function connectionError(message: string): Error {
  return new Error(message);
}

function isLiveSession(session: InternalSession): boolean {
  return (
    session.state === "creating" ||
    session.state === "active" ||
    session.state === "exited"
  );
}

function toPublicSession(s: InternalSession): BlitSession {
  return s;
}

export class BlitConnection {
  readonly id: ConnectionId;

  private readonly transport: BlitTransport;
  private readonly store: TerminalStore;

  private readonly listeners = new Set<() => void>();
  private readonly sessionsById = new Map<SessionId, InternalSession>();
  private readonly currentSessionIdByPtyId = new Map<number, SessionId>();
  private readonly pendingCreates = new Map<number, PendingCreate>();
  private readonly pendingCloses = new Map<SessionId, Array<() => void>>();
  private readonly pendingSearches = new Map<number, PendingSearch>();
  private readonly pendingReads = new Map<
    number,
    { resolve: (text: string) => void; reject: (error: Error) => void }
  >();

  private sessionCounter = 0;
  private nonceCounter = 0;
  private searchCounter = 0;
  private features = 0;
  private disposed = false;
  /** Per-session, per-view size registry for computing minimum resize. */
  private viewSizes = new Map<
    SessionId,
    Map<string, { rows: number; cols: number }>
  >();
  private viewIdCounter = 0;
  private hasConnected = false;
  private retryCount = 0;
  private lastError: string | null = null;

  private snapshot: BlitConnectionSnapshot;
  private sessions: InternalSession[] = [];
  private _publicSessions: BlitSession[] = [];
  private _publicSessionsDirty = false;

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
      supportsCopyRange: false,
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

  private get publicSessions(): BlitSession[] {
    if (this._publicSessionsDirty) {
      this._publicSessions = this.sessions.map(toPublicSession);
      this._publicSessionsDirty = false;
    }
    return this._publicSessions;
  }

  private invalidatePublicSessions(): void {
    this._publicSessionsDirty = true;
  }

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
    this.rejectPendingReads(connectionError("Connection disposed"));
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
    const s = this.sessionsById.get(sessionId);
    return s ? toPublicSession(s) : null;
  }

  getDebugStats(
    sessionId: SessionId | null,
  ): ReturnType<TerminalStore["getDebugStats"]> {
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
        nonce = this.nonceCounter = (this.nonceCounter + 1) & 0xffff;
      } while (this.pendingCreates.has(nonce));

      let srcPtyId: number | undefined;
      if (options.cwdFromSessionId) {
        const src = this.sessionsById.get(options.cwdFromSessionId);
        if (src) srcPtyId = src.ptyId;
      }

      this.pendingCreates.set(nonce, { resolve, reject });
      this.transport.send(
        buildCreate2Message(nonce, options.rows, options.cols, {
          tag: options.tag,
          command: options.command,
          srcPtyId,
        }),
      );
    });
  }

  copyRange(
    sessionId: SessionId,
    startTail: number,
    startCol: number,
    endTail: number,
    endCol: number,
  ): Promise<string> {
    if (this.transport.status !== "connected") {
      return Promise.reject(
        connectionError(
          `Cannot copy while transport is ${this.transport.status}`,
        ),
      );
    }
    const session = this.sessionsById.get(sessionId);
    if (!session) {
      return Promise.reject(connectionError("Unknown session"));
    }
    return new Promise<string>((resolve, reject) => {
      let nonce = 0;
      do {
        nonce = this.nonceCounter = (this.nonceCounter + 1) & 0xffff;
      } while (
        this.pendingCreates.has(nonce) ||
        this.pendingReads.has(nonce)
      );
      this.pendingReads.set(nonce, { resolve, reject });
      this.transport.send(
        buildCopyRangeMessage(
          nonce,
          session.ptyId,
          startTail,
          startCol,
          endTail,
          endCol,
        ),
      );
    });
  }

  supportsCopyRange(): boolean {
    return (this.features & FEATURE_COPY_RANGE) !== 0;
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
    if (
      !session ||
      session.state === "closed" ||
      this.transport.status !== "connected"
    ) {
      return;
    }
    this.transport.send(buildRestartMessage(session.ptyId));
  }

  killSession(sessionId: SessionId, signal = 15): void {
    const session = this.sessionsById.get(sessionId);
    if (
      !session ||
      session.state !== "active" ||
      this.transport.status !== "connected"
    ) {
      return;
    }
    this.transport.send(buildKillMessage(session.ptyId, signal));
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
    if (
      !session ||
      !isLiveSession(session) ||
      this.transport.status !== "connected"
    ) {
      return;
    }
    this.transport.send(buildInputMessage(session.ptyId, data));
  }

  resizeSession(sessionId: SessionId, rows: number, cols: number): void {
    this.resizeSessions([{ sessionId, rows, cols }]);
  }

  clearSessionSize(sessionId: SessionId): void {
    this.clearSessionSizes([sessionId]);
  }

  clearSessionSizes(sessionIds: Iterable<SessionId>): void {
    if (this.transport.status !== "connected") {
      return;
    }
    const ptyIds: number[] = [];
    for (const sessionId of sessionIds) {
      const session = this.sessionsById.get(sessionId);
      if (!session || !isLiveSession(session)) {
        continue;
      }
      ptyIds.push(session.ptyId);
    }
    if (ptyIds.length === 0 || (this.features & FEATURE_RESIZE_BATCH) === 0) {
      return;
    }
    if (ptyIds.length === 1) {
      this.transport.send(buildClearResizeMessage(ptyIds[0]!));
      return;
    }
    this.transport.send(buildClearResizeBatchMessage(ptyIds));
  }

  resizeSessions(entries: Iterable<ResizeSessionOptions>): void {
    if (this.transport.status !== "connected") {
      return;
    }
    const resolved: Array<{ ptyId: number; rows: number; cols: number }> = [];
    for (const entry of entries) {
      const session = this.sessionsById.get(entry.sessionId);
      if (!session || !isLiveSession(session)) {
        continue;
      }
      resolved.push({
        ptyId: session.ptyId,
        rows: entry.rows,
        cols: entry.cols,
      });
    }
    if (resolved.length === 0) {
      return;
    }
    if ((this.features & FEATURE_RESIZE_BATCH) !== 0) {
      this.transport.send(buildResizeBatchMessage(resolved));
      return;
    }
    for (const entry of resolved) {
      this.transport.send(
        buildResizeMessage(entry.ptyId, entry.rows, entry.cols),
      );
    }
  }

  scrollSession(sessionId: SessionId, offset: number): void {
    const session = this.sessionsById.get(sessionId);
    if (
      !session ||
      !isLiveSession(session) ||
      this.transport.status !== "connected"
    ) {
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
    if (
      !session ||
      !isLiveSession(session) ||
      this.transport.status !== "connected"
    ) {
      return;
    }
    this.transport.send(
      buildMouseMessage(session.ptyId, type, button, col, row),
    );
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
        requestId = this.searchCounter = (this.searchCounter + 1) & 0xffff;
      } while (this.pendingSearches.has(requestId));

      this.pendingSearches.set(requestId, { resolve, reject });
      this.transport.send(buildSearchMessage(requestId, query));
    });
  }

  private ptyId(sessionId: SessionId): number | undefined {
    return this.sessionsById.get(sessionId)?.ptyId;
  }

  getTerminal(sessionId: SessionId) {
    const id = this.ptyId(sessionId);
    return id != null ? this.store.getTerminal(id) : null;
  }

  /** Allocate a unique view ID for multi-pane size tracking. */
  allocViewId(): string {
    return `v${++this.viewIdCounter}`;
  }

  /** Register/update a view's size for a session. Sends the minimum to the server. */
  setViewSize(
    sessionId: SessionId,
    viewId: string,
    rows: number,
    cols: number,
  ): void {
    let views = this.viewSizes.get(sessionId);
    if (!views) {
      views = new Map();
      this.viewSizes.set(sessionId, views);
    }
    views.set(viewId, { rows, cols });
    this.sendMinSize(sessionId);
  }

  /** Unregister a view. Recalculates and sends the new minimum. */
  removeView(sessionId: SessionId, viewId: string): void {
    const views = this.viewSizes.get(sessionId);
    if (!views) return;
    views.delete(viewId);
    if (views.size === 0) {
      this.viewSizes.delete(sessionId);
    } else {
      this.sendMinSize(sessionId);
    }
  }

  private sendMinSize(sessionId: SessionId): void {
    const views = this.viewSizes.get(sessionId);
    if (!views || views.size === 0) return;
    let minRows = Infinity;
    let minCols = Infinity;
    for (const { rows, cols } of views.values()) {
      if (rows < minRows) minRows = rows;
      if (cols < minCols) minCols = cols;
    }
    // views.size > 0 guarantees minRows/minCols are finite.
    if (minRows > 0 && minCols > 0) {
      this.resizeSession(sessionId, minRows, minCols);
    }
  }

  metricsGeneration(): number {
    return this.store.metricsGeneration;
  }

  bumpMetricsGeneration(): number {
    return ++this.store.metricsGeneration;
  }

  getRetainCount(sessionId: SessionId): number {
    const id = this.ptyId(sessionId);
    return id != null ? this.store.getRetainCount(id) : 0;
  }

  retain(sessionId: SessionId): void {
    const id = this.ptyId(sessionId);
    if (id != null) this.store.retain(id);
  }

  release(sessionId: SessionId): void {
    const id = this.ptyId(sessionId);
    if (id != null) this.store.release(id);
  }

  freeze(sessionId: SessionId): void {
    const id = this.ptyId(sessionId);
    if (id != null) this.store.freeze(id);
  }

  thaw(sessionId: SessionId): void {
    const id = this.ptyId(sessionId);
    if (id != null) this.store.thaw(id);
  }

  isFrozen(sessionId: SessionId): boolean {
    const id = this.ptyId(sessionId);
    return id != null && this.store.isFrozen(id);
  }

  addDirtyListener(sessionId: SessionId, listener: () => void): () => void {
    const id = this.ptyId(sessionId);
    if (id == null) return () => {};
    return this.store.addDirtyListener((dirtyId) => {
      if (dirtyId === id) listener();
    });
  }

  drainPending(sessionId: SessionId): boolean {
    const id = this.ptyId(sessionId);
    return id != null ? this.store.drainPending(id) : false;
  }

  getSharedRenderer() {
    return this.store.getSharedRenderer();
  }
  setCellSize(pw: number, ph: number): void {
    this.store.setCellSize(pw, ph);
  }
  getCellSize() {
    return this.store.getCellSize();
  }
  wasmMemory() {
    return this.store.wasmMemory();
  }
  noteFrameRendered(): void {
    this.store.noteFrameRendered();
  }
  invalidateAtlas(): void {
    this.store.invalidateAtlas();
  }
  setFontFamily(f: string): void {
    this.store.setFontFamily(f);
  }
  setFontSize(s: number): void {
    this.store.setFontSize(s);
  }
  setPalette(p: TerminalPalette): void {
    this.store.setPalette(p);
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
        if (
          (this.features & FEATURE_CREATE_NONCE) === 0 &&
          this.pendingCreates.size > 0
        ) {
          const [firstNonce, pending] = this.pendingCreates.entries().next()
            .value as [number, PendingCreate];
          this.pendingCreates.delete(firstNonce);
          pending.resolve(toPublicSession(session));
        }

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
          pending.resolve(toPublicSession(session));
        }

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
          supportsCopyRange: (features & FEATURE_COPY_RANGE) !== 0,
        };
        this.emit();
        return;
      }
      case S2C_TEXT: {
        if (bytes.length < 13) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const text = textDecoder.decode(bytes.subarray(13));
        const pending = this.pendingReads.get(nonce);
        if (pending) {
          this.pendingReads.delete(nonce);
          pending.resolve(text);
        }
        return;
      }
      default:
        return;
    }
  };

  private handleStatusChange = (status: ConnectionStatus): void => {
    this.store.handleStatusChange(status);

    const lastError =
      status === "error" && this.transport.lastError
        ? this.transport.lastError
        : null;
    const authRejected = status === "error" && this.transport.authRejected;

    if (status === "connected") {
      this.hasConnected = true;
      this.retryCount = 0;
      this.lastError = null;
    } else if (
      (status === "error" || status === "disconnected" || status === "closed") &&
      (this.snapshot.status === "connecting" ||
        this.snapshot.status === "authenticating")
    ) {
      this.retryCount++;
    }

    // Persist the error until a successful connection clears it.
    if (authRejected) {
      this.lastError = "auth";
    } else if (lastError) {
      this.lastError = lastError;
    }

    this.snapshot = {
      ...this.snapshot,
      status,
      retryCount: this.retryCount,
      error: this.lastError,
    };

    if (status === "disconnected" || status === "closed" || status === "error") {
      this.rejectPendingCreates(
        connectionError(`Transport ${status} before PTY creation completed`),
      );
      this.rejectPendingSearches(connectionError(`Transport ${status}`));
      this.rejectPendingReads(connectionError(`Transport ${status}`));
      this.resolveAllPendingCloses();
    }

    this.emit();
  };

  private handleListMessage(bytes: Uint8Array): void {
    if (bytes.length < 3) return;

    const count = bytes[1] | (bytes[2] << 8);
    const entries: Array<{
      ptyId: number;
      tag: string;
      command: string | null;
    }> = [];
    let offset = 3;
    for (let index = 0; index < count; index++) {
      if (offset + 4 > bytes.length) break;
      const ptyId = bytes[offset] | (bytes[offset + 1] << 8);
      const tagLen = bytes[offset + 2] | (bytes[offset + 3] << 8);
      offset += 4;
      const tag = textDecoder.decode(bytes.subarray(offset, offset + tagLen));
      offset += tagLen;
      let command: string | null = null;
      if (offset + 2 <= bytes.length) {
        const cmdLen = bytes[offset] | (bytes[offset + 1] << 8);
        offset += 2;
        if (cmdLen > 0 && offset + cmdLen <= bytes.length) {
          command = textDecoder.decode(bytes.subarray(offset, offset + cmdLen));
        }
        offset += cmdLen;
      }
      entries.push({ ptyId, tag, command });
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
        ? (this.sessionsById.get(existingSessionId) ?? null)
        : null;
      if (!existingSession || existingSession.state === "closed") {
        this.upsertLiveSession(entry.ptyId, entry.tag, "active", entry.command);
        continue;
      }
      this.updateSession(existingSession.id, {
        tag: entry.tag,
        command: entry.command,
        state: existingSession.state === "exited" ? "exited" : "active",
      });
    }

    const previousFocus = this.snapshot.focusedSessionId;
    const nextFocus =
      this.snapshot.focusedSessionId &&
      this.sessionsById.get(this.snapshot.focusedSessionId)?.state !== "closed"
        ? this.snapshot.focusedSessionId
        : null;

    this.snapshot = {
      ...this.snapshot,
      ready: true,
      focusedSessionId: nextFocus,
    };
    this.store.setLead(
      nextFocus ? (this.sessionsById.get(nextFocus)?.ptyId ?? null) : null,
    );
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
          (bytes[offset + 11] << 24)) >>>
        0;
      const scrollOffset = rawScroll === 0xffffffff ? null : rawScroll;
      const contextLen = bytes[offset + 12] | (bytes[offset + 13] << 8);
      offset += 14;
      const context = textDecoder.decode(
        bytes.subarray(offset, offset + contextLen),
      );
      offset += contextLen;

      const sessionId = this.currentSessionIdByPtyId.get(ptyId);
      if (!sessionId) continue;

      results.push({
        sessionId,
        connectionId: this.id,
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
    command: string | null = null,
  ): InternalSession {
    const currentId = this.currentSessionIdByPtyId.get(ptyId);
    const current = currentId
      ? (this.sessionsById.get(currentId) ?? null)
      : null;
    if (current && current.state !== "closed") {
      return this.updateSession(current.id, { tag, command, state });
    }

    const session: InternalSession = {
      id: `${this.id}:${++this.sessionCounter}`,
      connectionId: this.id,
      ptyId,
      tag,
      title: current?.title ?? null,
      command,
      state,
    };
    this.currentSessionIdByPtyId.set(ptyId, session.id);
    this.sessionsById.set(session.id, session);
    this.sessions = [...this.sessions, session];
    this.invalidatePublicSessions();
    this.snapshot = {
      ...this.snapshot,
      sessions: this.publicSessions,
    };
    this.emit();
    return session;
  }

  private updateSession(
    sessionId: SessionId,
    patch: Partial<Omit<InternalSession, "id" | "connectionId" | "ptyId">>,
  ): InternalSession {
    const current = this.sessionsById.get(sessionId);
    if (!current) {
      throw connectionError(`Unknown session ${sessionId}`);
    }

    // Skip no-op updates.
    if (
      Object.keys(patch).every(
        (k) =>
          (current as Record<string, unknown>)[k] ===
          (patch as Record<string, unknown>)[k],
      )
    ) {
      return current;
    }

    const next: InternalSession = { ...current, ...patch };
    this.sessionsById.set(sessionId, next);
    this.sessions = this.sessions.map((session) =>
      session.id === sessionId ? next : session,
    );
    this.invalidatePublicSessions();
    this.snapshot = {
      ...this.snapshot,
      sessions: this.publicSessions,
    };
    this.emit();
    return next;
  }

  private markSessionClosed(sessionId: SessionId, emit = true): void {
    const session = this.sessionsById.get(sessionId);
    if (!session || session.state === "closed") return;

    const next: InternalSession = {
      ...session,
      state: "closed",
    };
    this.sessionsById.set(sessionId, next);
    this.invalidatePublicSessions();
    this.sessions = this.sessions.map((entry) =>
      entry.id === sessionId ? next : entry,
    );
    if (this.currentSessionIdByPtyId.get(session.ptyId) === sessionId) {
      this.currentSessionIdByPtyId.delete(session.ptyId);
    }
    this.store.freeTerminal(session.ptyId);

    const focusedWasClosed = this.snapshot.focusedSessionId === sessionId;
    const nextFocus = focusedWasClosed
      ? this.firstLiveSessionId()
      : this.snapshot.focusedSessionId;

    this.snapshot = {
      ...this.snapshot,
      sessions: this.publicSessions,
      focusedSessionId: nextFocus ?? null,
    };
    this.store.setLead(
      nextFocus ? (this.sessionsById.get(nextFocus)?.ptyId ?? null) : null,
    );

    const resolvers = this.pendingCloses.get(sessionId);
    if (resolvers) {
      this.pendingCloses.delete(sessionId);
      for (const resolve of resolvers) resolve();
    }

    if (emit) {
      if (
        focusedWasClosed &&
        nextFocus &&
        this.transport.status === "connected"
      ) {
        const nextSession = this.sessionsById.get(nextFocus);
        if (nextSession) {
          this.transport.send(buildFocusMessage(nextSession.ptyId));
        }
      }
      this.emit();
    }
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

  private rejectPendingReads(error: Error): void {
    for (const pending of this.pendingReads.values()) {
      pending.reject(error);
    }
    this.pendingReads.clear();
  }

  private resolveAllPendingCloses(): void {
    for (const resolvers of this.pendingCloses.values()) {
      for (const resolve of resolvers) resolve();
    }
    this.pendingCloses.clear();
  }
}
