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
  FEATURE_AUDIO,
  FEATURE_COMPOSITOR,
  FEATURE_COPY_RANGE,
  FEATURE_CREATE_NONCE,
  FEATURE_RESIZE_BATCH,
  FEATURE_RESTART,
  S2C_AUDIO_FRAME,
  PROTOCOL_VERSION,
  S2C_CLIPBOARD_CONTENT,
  S2C_CLOSED,
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_EXITED,
  S2C_HELLO,
  S2C_LIST,
  S2C_READY,
  S2C_SEARCH_RESULTS,
  S2C_SURFACE_APP_ID,
  S2C_SURFACE_CURSOR,
  S2C_SURFACE_CREATED,
  S2C_SURFACE_DESTROYED,
  S2C_SURFACE_ENCODER,
  S2C_SURFACE_FRAME,
  S2C_SURFACE_RESIZED,
  S2C_SURFACE_TITLE,
  S2C_PING,
  S2C_QUIT,
  S2C_TEXT,
  S2C_TITLE,
  S2C_UPDATE,
  C2S_PING,
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
  buildSurfaceInputMessage,
  buildSurfaceTextMessage,
  buildSurfacePointerMessage,
  buildSurfaceAxisMessage,
  buildSurfaceResizeMessage,
  buildSurfaceFocusMessage,
  buildSurfaceCloseMessage,
  buildSurfaceSubscribeMessage,
  buildSurfaceUnsubscribeMessage,
  buildSurfaceAckMessage,
  buildClipboardMessage,
  buildClientFeaturesMessage,
  buildAudioSubscribeMessage,
  buildAudioUnsubscribeMessage,
} from "./protocol";
import { AudioPlayer } from "./AudioPlayer";
import { SurfaceStore } from "./SurfaceStore";
import { TerminalStore, type BlitWasmModule } from "./TerminalStore";
import { detectCodecSupport } from "./BlitSurfaceCanvas";

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
  logger?: import("./BlitWorkspace").BlitLogger;
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
  command?: string;
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

/** Per-surface subscription state.  One entry per visible surface on
 *  this connection.  `refCount` tracks how many mounts (e.g. BSP view
 *  plus side-panel preview) share the stream: the wire UNSUBSCRIBE
 *  fires only when the last mount goes away.  Without ref-counting,
 *  unmounting one of two mounts tears down the stream for both. */
interface SurfaceSub {
  surfaceId: number;
  /** Number of live mounts currently referencing this sub. */
  refCount: number;
  /** Quality override set via {@link BlitConnection.sendSurfaceResubscribe}. */
  qualityOverride: number | null;
  /** Last quality value sent on the wire, for dedup. */
  lastSentQuality: number | null;
  /** When the last mount has gone away we schedule a deferred wire
   *  UNSUBSCRIBE instead of firing it immediately.  Moving a surface
   *  between two UI locations (e.g. side-panel preview → BSP) causes
   *  an unmount + mount pair; without the grace window the server
   *  tears down the encoder in between and the new mount waits for a
   *  full re-init + keyframe.  A fresh subscribe within the window
   *  cancels the pending UNSUB and the stream continues uninterrupted. */
  pendingUnsub: ReturnType<typeof setTimeout> | null;
}

export class BlitConnection {
  readonly id: ConnectionId;

  readonly transport: BlitTransport;
  private readonly store: TerminalStore;
  readonly surfaceStore = new SurfaceStore();
  readonly audioPlayer = new AudioPlayer();

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

  /** Default video quality for new surface subscriptions (0 = server default). */
  defaultSurfaceQuality = 0;
  /** Default audio bitrate in kbps for audio subscriptions (0 = server default). */
  defaultAudioBitrateKbps = 0;
  /** When false, surface subscribe messages are suppressed (ref-counts
   *  still tracked so re-enabling restores subscriptions). */
  surfaceStreamingEnabled = true;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private readonly pingIntervalMs = 10_000;

  private snapshot: BlitConnectionSnapshot;
  private sessions: InternalSession[] = [];
  private _publicSessions: BlitSession[] = [];
  private _publicSessionsDirty = false;
  private _logger: import("./BlitWorkspace").BlitLogger;

  constructor({
    id,
    transport,
    wasm,
    autoConnect = true,
    logger,
  }: CreateBlitConnectionOptions) {
    this.id = id;
    this.transport = transport;
    // Inline fallback to avoid circular import of consoleLogger at module load.
    this._logger = logger ?? {
      info: (m, ...a) => console.log(`[blit] ${m}`, ...a),
      warn: (m, ...a) => console.warn(`[blit] ${m}`, ...a),
    };
    this.surfaceStore.setConnectionId(id);
    this.surfaceStore.setAckSender((surfaceId) => {
      if (this.transport.status === "connected") {
        this.transport.send(buildSurfaceAckMessage(surfaceId));
      }
    });
    this.surfaceStore.setKeyframeSender((surfaceId) => {
      // Re-subscribing triggers surface_needs_keyframe on the server,
      // which forces the next encoded frame to be a keyframe.
      if (
        this.transport.status !== "connected" ||
        !this.surfaceStreamingEnabled
      ) {
        return;
      }
      const sub = this.surfaceSubs.get(surfaceId);
      if (sub) {
        sub.lastSentQuality = null;
        this.maybeSendSurfaceSubscribe(sub);
      }
    });
    this.store = new TerminalStore(
      {
        send: (data) => {
          if (this.transport.status === "connected") {
            this.transport.send(data);
          }
        },
        getStatus: () => this.transport.status,
        log: (msg) => this._logger.info(`${this.id}: ${msg}`),
      },
      wasm,
    );
    this.snapshot = {
      id,
      // When the transport is already connected, the blit handshake hasn't
      // completed yet — report "authenticating" until S2C_READY arrives.
      status:
        transport.status === "connected" ? "authenticating" : transport.status,
      ready: false,
      supportsRestart: false,
      supportsCopyRange: false,
      supportsCompositor: false,
      supportsAudio: false,
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

    // Propagate AudioPlayer state changes (e.g. reset on reconnect) into the
    // connection's listener chain so the reactive graph re-evaluates audio
    // subscription intent.  Without this, audioPlayer.reset() sets _subscribed
    // to false but nothing in the SolidJS reactive graph notices, so the
    // Workspace audio effect never re-runs to re-subscribe.
    this.audioPlayer.onChange(() => this.emit());

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
    if (this.transport.reconnect) {
      this.transport.reconnect();
    } else {
      this.connect();
    }
  }

  close(): void {
    this.transport.close();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this.pingTimer !== null) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
    this.transport.removeEventListener("message", this.handleMessage);
    this.transport.removeEventListener("statuschange", this.handleStatusChange);
    this.rejectPendingCreates(
      connectionError("Connection disposed before PTY creation completed"),
    );
    this.rejectPendingSearches(connectionError("Connection disposed"));
    this.rejectPendingReads(connectionError("Connection disposed"));
    this.resolveAllPendingCloses();
    this.clearSurfaceSubs();
    this.store.destroy();
    this.surfaceStore.destroy();
    this.audioPlayer.destroy();
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

  getDebugStats(sessionId: SessionId | null): ReturnType<
    TerminalStore["getDebugStats"]
  > & {
    surfaces: ReturnType<
      import("./SurfaceStore").SurfaceStore["getDebugStats"]
    >;
  } {
    const session = sessionId ? this.sessionsById.get(sessionId) : null;
    return {
      ...this.store.getDebugStats(session?.ptyId ?? null),
      surfaces: this.surfaceStore.getDebugStats(),
    };
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

      this.pendingCreates.set(nonce, {
        resolve,
        reject,
        command: options.command,
      });
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
      } while (this.pendingCreates.has(nonce) || this.pendingReads.has(nonce));
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

    // Mark the session closed immediately so the UI updates without
    // waiting for the server round-trip.  This prevents a visual glitch
    // in BSP layouts where the session briefly appears in the off-screen
    // sidebar (triggering the preview panel and shifting splits) between
    // being unassigned from a pane and the server confirming the close.
    // The retain-count mechanism in TerminalStore ensures the terminal
    // data isn't freed while a BlitTerminalSurface still references it.
    this.markSessionClosed(sessionId);

    if (this.transport.status !== "connected") return;
    this.transport.send(buildCloseMessage(session.ptyId));
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
      this.clearSessionSize(sessionId);
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

  sendSurfaceInput(surfaceId: number, keycode: number, pressed: boolean): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceInputMessage(surfaceId, keycode, pressed));
  }

  sendSurfaceText(surfaceId: number, text: string): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceTextMessage(surfaceId, text));
  }

  sendSurfacePointer(
    surfaceId: number,
    type: number,
    button: number,
    x: number,
    y: number,
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(
      buildSurfacePointerMessage(surfaceId, type, button, x, y),
    );
  }

  sendSurfaceAxis(surfaceId: number, axis: number, valueX100: number): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceAxisMessage(surfaceId, axis, valueX100));
  }

  sendSurfaceResize(
    surfaceId: number,
    width: number,
    height: number,
    scale120: number = 0,
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(
      buildSurfaceResizeMessage(surfaceId, width, height, scale120),
    );
  }

  sendSurfaceFocus(surfaceId: number): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceFocusMessage(surfaceId));
  }

  sendSurfaceClose(surfaceId: number): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceCloseMessage(surfaceId));
  }

  // Per-surface subscription state.  Multiple views (main BSP tile + a
  // side-panel thumbnail + a popup preview…) can subscribe to the same
  // surface simultaneously; each holds an opaque token so the connection
  // can maintain a correct per-subscriber view rather than a collapsed
  // refcount.  The effective subscribe sent on the wire is derived:
  //   * target: if any subscriber wants unscaled (target = null),
  //     subscribe unscaled; otherwise pick the largest requested target
  //     (smaller subscribers can downscale from the larger stream
  //     client-side, but the reverse would be lossy).
  //   * quality: per-surface override (set by sendSurfaceResubscribe)
  //     falling back to defaultSurfaceQuality.
  // Keyed by sub_id (client-allocated u32).  Each `BlitSurfaceCanvas`
  // (or equivalent caller) allocates its own sub_id and owns the
  // subscribe/unsubscribe lifecycle for that id.
  /** Active surface subscriptions keyed by surface id. */
  private surfaceSubs = new Map<number, SurfaceSub>();

  /**
   * True once {@link detectCodecSupport} has resolved and
   * {@link sendClientFeatures} has informed the server which video codecs
   * this client can decode.
   *
   * Surface subscribes are sent immediately (with codec_support=0 =
   * "accept anything") so the server can start encoding the first frame
   * without waiting for the async codec probe.  Once the probe resolves
   * we send C2S_CLIENT_FEATURES and re-subscribe any active surfaces so
   * the server can switch to the optimal encoder.  This eliminates a
   * round-trip from the time-to-first-frame on remote connections.
   */
  private _codecFeaturesSent = false;

  /** Grace window before a refCount=0 subscription's wire UNSUB fires.
   *  Chosen to comfortably cover typical Solid re-render ordering where
   *  the old mount's `onCleanup` fires before the new mount's
   *  `onMount`, but keeps dropped-stream latency tight if the user
   *  really did stop watching. */
  private static readonly SUB_UNSUB_GRACE_MS = 250;

  /** Cancel any pending deferred unsubscribe timers and reset
   *  `lastSentQuality` so the next refresh fires a wire subscribe.
   *  Called on reconnect / S2C_HELLO: the refCounts (one per live
   *  mount) are authoritative and must survive a reconnect — wiping
   *  the map would leave the existing mounts with no way to reclaim
   *  their subscriptions (`refreshSurfaceSubscribe` would no-op). */
  private resetSurfaceSubsForReconnect(): void {
    for (const sub of this.surfaceSubs.values()) {
      if (sub.pendingUnsub !== null) {
        clearTimeout(sub.pendingUnsub);
        sub.pendingUnsub = null;
      }
      sub.lastSentQuality = null;
    }
  }

  /** Called from `dispose()` — the connection is going away permanently.
   *  Drop everything, including ref-counts. */
  private clearSurfaceSubs(): void {
    for (const sub of this.surfaceSubs.values()) {
      if (sub.pendingUnsub !== null) {
        clearTimeout(sub.pendingUnsub);
        sub.pendingUnsub = null;
      }
    }
    this.surfaceSubs.clear();
  }

  private maybeSendSurfaceSubscribe(sub: SurfaceSub): void {
    if (this.transport.status !== "connected") return;
    if (!this.surfaceStreamingEnabled) return;
    const quality = sub.qualityOverride ?? this.defaultSurfaceQuality;
    if (sub.lastSentQuality === quality) return;
    sub.lastSentQuality = quality;
    this._logger.info(`surface sub ${this.id}:${sub.surfaceId}`);
    this.transport.send(
      buildSurfaceSubscribeMessage(sub.surfaceId, 0, quality),
    );
  }

  /** Subscribe to frames for a surface.  A single subscription exists
   *  per (connection, surface); additional views of the same surface
   *  share it.  Callers should ref-count mounts above this layer. */
  sendSurfaceSubscribe(surfaceId: number): void {
    let sub = this.surfaceSubs.get(surfaceId);
    if (!sub) {
      sub = {
        surfaceId,
        refCount: 1,
        qualityOverride: null,
        lastSentQuality: null,
        pendingUnsub: null,
      };
      this.surfaceSubs.set(surfaceId, sub);
    } else {
      sub.refCount += 1;
      // Cancel any pending deferred UNSUB — the new mount wants the
      // live stream and the server's encoder is still valid.
      if (sub.pendingUnsub !== null) {
        clearTimeout(sub.pendingUnsub);
        sub.pendingUnsub = null;
      }
    }
    this.maybeSendSurfaceSubscribe(sub);
  }

  /** Resend the wire subscribe without bumping the ref-count.  Used
   *  after reconnect, where the server lost its subscription table but
   *  the client still has all its mounts active — bumping the count
   *  would leak references. */
  refreshSurfaceSubscribe(surfaceId: number): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub) return;
    sub.lastSentQuality = null;
    this.maybeSendSurfaceSubscribe(sub);
  }

  /** Re-subscribe active subs after the codec probe resolves so the
   *  server can switch to the optimal encoder for this client's
   *  capabilities.  Subs subscribed with codec_support=0 ("accept
   *  anything") before the probe completed get updated. */
  private resubscribeWithCodecSupport(): void {
    if (this.transport.status !== "connected") return;
    if (!this.surfaceStreamingEnabled) return;
    for (const sub of this.surfaceSubs.values()) {
      sub.lastSentQuality = null;
      this.maybeSendSurfaceSubscribe(sub);
    }
  }

  sendSurfaceUnsubscribe(surfaceId: number): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub) return;
    sub.refCount -= 1;
    if (sub.refCount > 0) return;
    // Defer the wire UNSUB so a remount within the grace window
    // (typical when moving a surface between UI locations, e.g.
    // BSP ↔ side-panel preview) finds the server-side encoder still
    // alive and can resume without a full re-init + keyframe wait.
    if (sub.pendingUnsub !== null) clearTimeout(sub.pendingUnsub);
    sub.pendingUnsub = setTimeout(() => {
      const cur = this.surfaceSubs.get(surfaceId);
      if (!cur || cur.refCount > 0 || cur.pendingUnsub === null) return;
      cur.pendingUnsub = null;
      if (this.transport.status === "connected") {
        this._logger.info(`surface unsub ${this.id}:${surfaceId}`);
        this.transport.send(buildSurfaceUnsubscribeMessage(surfaceId));
      }
      this.surfaceSubs.delete(surfaceId);
    }, BlitConnection.SUB_UNSUB_GRACE_MS);
  }

  /** Set a per-surface quality override and re-send the subscribe.
   *  The server treats a second SURFACE_SUBSCRIBE at the same sid
   *  as a quality/codec update.  No-op when the sid is unknown. */
  sendSurfaceResubscribe(surfaceId: number, quality: number): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub) return;
    sub.qualityOverride = quality;
    sub.lastSentQuality = null;
    this.maybeSendSurfaceSubscribe(sub);
  }

  /**
   * Enable or disable surface video streaming.  When disabled, per-sub
   * state is preserved but no subscribe messages are sent.  Re-enabling
   * sends subscribe for every active sub.
   */
  setSurfaceStreamingEnabled(enabled: boolean): void {
    if (this.surfaceStreamingEnabled === enabled) return;
    this.surfaceStreamingEnabled = enabled;
    if (this.transport.status !== "connected") return;
    if (enabled) {
      for (const sub of this.surfaceSubs.values()) {
        sub.lastSentQuality = null;
        this.maybeSendSurfaceSubscribe(sub);
      }
    } else {
      for (const sub of this.surfaceSubs.values()) {
        this.transport.send(buildSurfaceUnsubscribeMessage(sub.surfaceId));
        sub.lastSentQuality = null;
      }
    }
  }

  /**
   * Subscribe to audio frames, optionally specifying bitrate.
   * Can be called repeatedly to adjust bitrate without unsubscribing first.
   * `bitrateKbps`: 0 = server default, otherwise desired Opus bitrate in kbps.
   */
  sendAudioSubscribe(bitrateKbps: number = 0): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildAudioSubscribeMessage(bitrateKbps));
    this.audioPlayer.setSubscribed(true);
  }

  sendAudioUnsubscribe(): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildAudioUnsubscribeMessage());
    this.audioPlayer.setSubscribed(false);
  }

  /**
   * Reset the audio pipeline to recover from stalled or broken audio.
   * The server subscription stays active — audio rebuilds automatically
   * on the next incoming frame without a re-subscribe round-trip.
   */
  resetAudio(): void {
    this._logger.info(`${this.id}: audio pipeline reset`);
    this.audioPlayer.resetPipeline();
  }

  sendClipboard(mimeType: string, data: Uint8Array): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildClipboardMessage(mimeType, data));
  }

  /**
   * Advertise client capabilities to the server.  Currently carries the
   * video codec support bitmask so the server picks a compatible encoder.
   * Called automatically when the connection is established and codec
   * probing completes.
   */
  sendClientFeatures(codecSupport: number): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildClientFeaturesMessage(codecSupport));
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
      case S2C_PING:
        // Application-level keepalive — no action needed.
        return;
      case S2C_QUIT:
        // Server is shutting down.  Immediately dismiss all sessions and
        // surfaces so the UI doesn't show stale windows while reconnecting.
        // This mirrors the S2C_HELLO reset path but happens *before* the
        // transport drops, so the UI clears instantly.
        this.surfaceStore.reset();
        this.audioPlayer.reset();
        this.resetSurfaceSubsForReconnect();
        for (const session of this.sessions) {
          if (session.state !== "closed") {
            this.markSessionClosed(session.id, false);
          }
        }
        this.snapshot = {
          ...this.snapshot,
          ready: false,
          sessions: this.publicSessions,
          focusedSessionId: null,
        };
        this.emit();
        // Immediately reconnect so the UI recovers as fast as possible
        // when the server restarts.  Do NOT call transport.close() — that
        // permanently disposes the transport.  transport.reconnect() tears
        // down the current connection and starts a fresh attempt right
        // away, bypassing the backoff delay that transport-level disconnect
        // detection would otherwise impose.
        if (this.transport.reconnect) {
          this.transport.reconnect();
        }
        return;
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
        let command: string | null = null;
        if (
          (this.features & FEATURE_CREATE_NONCE) === 0 &&
          this.pendingCreates.size > 0
        ) {
          const [firstNonce, pending] = this.pendingCreates.entries().next()
            .value as [number, PendingCreate];
          command = pending.command?.trim() || null;
          this.pendingCreates.delete(firstNonce);
          const session = this.upsertLiveSession(ptyId, tag, "active", command);
          pending.resolve(toPublicSession(session));
        } else {
          this.upsertLiveSession(ptyId, tag, "active");
        }

        return;
      }
      case S2C_CREATED_N: {
        if (bytes.length < 5) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const ptyId = bytes[3] | (bytes[4] << 8);
        const tag = textDecoder.decode(bytes.subarray(5));
        const pending = this.pendingCreates.get(nonce);
        const command = pending?.command?.trim() || null;
        const session = this.upsertLiveSession(ptyId, tag, "active", command);
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
      case S2C_READY: {
        // S2C_READY is the last message in the server's initial
        // handshake sequence (after S2C_SURFACE_CREATED and S2C_LIST).
        // Setting `ready` here instead of in S2C_LIST ensures the
        // surface store is already populated when the BSP reconciliation
        // runs, preventing surface assignments from being wiped.
        //
        // Also promote the snapshot status to "connected" — until now it
        // was held at "authenticating" (see handleStatusChange) because
        // the transport being open doesn't mean the remote blit server
        // is reachable and functional.
        if (!this.snapshot.ready || this.snapshot.status !== "connected") {
          this.snapshot = {
            ...this.snapshot,
            ready: true,
            status:
              this.transport.status === "connected"
                ? "connected"
                : this.snapshot.status,
          };
          this.emit();
        }
        // Prune closed sessions that have been superseded by a live
        // session for the same PTY.  This MUST happen after the emit()
        // above so the synchronous reactive flush (which runs BSP
        // reconciliation) still sees the closed sessions and can build
        // the old→new session-ID replacement map.  Deferring to a
        // microtask ensures the prune fires after the current reactive
        // cycle completes.
        queueMicrotask(() => this.pruneSupersededSessions());
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
        // S2C_HELLO is the first message on every new server connection.
        // Reset all surfaces and close stale sessions — the server's
        // initial message sequence (S2C_SURFACE_CREATED, S2C_LIST,
        // S2C_READY) will rebuild both.  S2C_READY marks the end of
        // the initial burst and sets `ready: true`.  This also handles
        // transparent gateway reconnects where the transport never went
        // through "disconnected".
        this.surfaceStore.reset();
        this.audioPlayer.reset();
        this.resetSurfaceSubsForReconnect();
        for (const session of this.sessions) {
          if (session.state !== "closed") {
            this.markSessionClosed(session.id, false);
          }
        }
        this.snapshot = {
          ...this.snapshot,
          // S2C_HELLO means a new handshake is starting — the connection
          // is not yet fully operational until S2C_READY arrives.
          status:
            this.snapshot.status === "connected"
              ? "authenticating"
              : this.snapshot.status,
          ready: false,
          supportsRestart: (features & FEATURE_RESTART) !== 0,
          supportsCopyRange: (features & FEATURE_COPY_RANGE) !== 0,
          supportsCompositor: (features & FEATURE_COMPOSITOR) !== 0,
          supportsAudio: (features & FEATURE_AUDIO) !== 0,
        };
        this.emit();
        return;
      }
      case S2C_SURFACE_CREATED: {
        try {
          if (bytes.length < 11) return;
          const view = new DataView(data);
          const surfaceId = view.getUint16(1, true);
          const parentId = view.getUint16(3, true);
          const width = view.getUint16(5, true);
          const height = view.getUint16(7, true);
          const titleLen = view.getUint16(9, true);
          const title = textDecoder.decode(bytes.subarray(11, 11 + titleLen));
          let appId = "";
          const appIdOffset = 11 + titleLen;
          if (bytes.length >= appIdOffset + 2) {
            const appIdLen = view.getUint16(appIdOffset, true);
            appId = textDecoder.decode(
              bytes.subarray(appIdOffset + 2, appIdOffset + 2 + appIdLen),
            );
          }
          this.surfaceStore.handleSurfaceCreated(
            surfaceId,
            parentId,
            width,
            height,
            title,
            appId,
          );
        } catch {
          // Surface errors must never block terminal message processing.
        }
        return;
      }
      case S2C_SURFACE_DESTROYED: {
        try {
          if (bytes.length < 3) return;
          const surfaceId = bytes[1] | (bytes[2] << 8);
          this.surfaceStore.handleSurfaceDestroyed(surfaceId);
        } catch {}
        return;
      }
      case S2C_SURFACE_FRAME: {
        // Layout: [type][sid 2][timestamp 4][flags 1][w 2][h 2][data…]
        if (bytes.length < 12) return;
        const view = new DataView(data);
        const surfaceId = view.getUint16(1, true);
        const timestamp = view.getUint32(3, true);
        const flags = bytes[7];
        const width = view.getUint16(8, true);
        const height = view.getUint16(10, true);
        try {
          // The store sends ACKs itself, deferring them when the decode
          // queue is deep to apply backpressure on the server.
          this.surfaceStore.handleSurfaceFrame(
            surfaceId,
            timestamp,
            flags,
            width,
            height,
            bytes.subarray(12),
          );
        } catch {
          // Swallowed decode errors must still ACK so the server's pacing
          // window doesn't permanently stall.
          this.surfaceStore.sendAckFallback(surfaceId);
        }
        // Feed the video frame's server timestamp to the audio player
        // for A/V sync.  Video is never delayed — the audio player uses
        // this to steer its playback rate.
        this.audioPlayer.notifyVideoTimestamp(timestamp);
        return;
      }
      case S2C_SURFACE_TITLE: {
        try {
          if (bytes.length < 3) return;
          const surfaceId = bytes[1] | (bytes[2] << 8);
          const title = textDecoder.decode(bytes.subarray(3));
          this.surfaceStore.handleSurfaceTitle(surfaceId, title);
        } catch {}
        return;
      }
      case S2C_SURFACE_CURSOR: {
        try {
          if (bytes.length < 4) return;
          const surfaceId = bytes[1] | (bytes[2] << 8);
          const cursorType = bytes[3];
          if (cursorType === 0) {
            // Named CSS cursor
            const nameLen = bytes[4];
            if (bytes.length < 5 + nameLen) return;
            const shape = textDecoder.decode(bytes.subarray(5, 5 + nameLen));
            this.surfaceStore.handleSurfaceCursor(surfaceId, shape);
          } else if (cursorType === 1) {
            // Hidden
            this.surfaceStore.handleSurfaceCursor(surfaceId, "none");
          } else if (cursorType === 2) {
            // Custom image: hotx(2) + hoty(2) + w(2) + h(2) + png
            if (bytes.length < 12) return;
            const view = new DataView(data);
            const hotX = view.getUint16(4, true);
            const hotY = view.getUint16(6, true);
            const pngData = bytes.subarray(12);
            const blob = new Blob([pngData], { type: "image/png" });
            const url = URL.createObjectURL(blob);
            this.surfaceStore.handleSurfaceCursor(
              surfaceId,
              `url(${url}) ${hotX} ${hotY}, auto`,
            );
          }
        } catch {}
        return;
      }
      case S2C_SURFACE_ENCODER: {
        try {
          // Layout: [type][sid 2][name + 0 + codec_str]
          if (bytes.length < 3) return;
          const view = new DataView(data);
          const surfaceId = view.getUint16(1, true);
          const encoderName = textDecoder.decode(bytes.subarray(3));
          this.surfaceStore.handleSurfaceEncoder(surfaceId, encoderName);
        } catch {}
        return;
      }
      case S2C_SURFACE_APP_ID: {
        try {
          if (bytes.length < 3) return;
          const surfaceId = bytes[1] | (bytes[2] << 8);
          const appId = textDecoder.decode(bytes.subarray(3));
          this.surfaceStore.handleSurfaceAppId(surfaceId, appId);
        } catch {}
        return;
      }
      case S2C_SURFACE_RESIZED: {
        try {
          if (bytes.length < 7) return;
          const view = new DataView(data);
          const surfaceId = view.getUint16(1, true);
          const width = view.getUint16(3, true);
          const height = view.getUint16(5, true);
          this.surfaceStore.handleSurfaceResized(surfaceId, width, height);
        } catch {}
        return;
      }
      case S2C_AUDIO_FRAME: {
        try {
          if (bytes.length < 6) return;
          const view = new DataView(data);
          const timestamp = view.getUint32(1, true);
          const flags = bytes[5];
          const audioData = bytes.subarray(6);
          this.audioPlayer.handleAudioFrame(timestamp, flags, audioData);
        } catch {}
        return;
      }
      case S2C_CLIPBOARD_CONTENT: {
        try {
          if (bytes.length < 7) return;
          const view = new DataView(data);
          const mimeLen = view.getUint16(1, true);
          if (bytes.length < 3 + mimeLen + 4) return;
          const mimeType = textDecoder.decode(bytes.subarray(3, 3 + mimeLen));
          const dataLen = view.getUint32(3 + mimeLen, true);
          const dataStart = 7 + mimeLen;
          if (bytes.length < dataStart + dataLen) return;
          if (mimeType.startsWith("text/") || mimeType === "UTF8_STRING") {
            const text = textDecoder.decode(
              bytes.subarray(dataStart, dataStart + dataLen),
            );
            navigator.clipboard.writeText(text).catch(() => {});
          }
        } catch {}
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
      (status === "error" || status === "disconnected") &&
      this.transport.lastError
        ? this.transport.lastError
        : null;
    const authRejected = status === "error" && this.transport.authRejected;

    if (status === "connected") {
      this.hasConnected = true;
      this.retryCount = 0;
      this.lastError = null;
      this._codecFeaturesSent = false;
      // Start application-level keepalive.
      if (this.pingTimer === null && this.pingIntervalMs > 0) {
        this.pingTimer = setInterval(() => {
          if (this.transport.status === "connected") {
            this.transport.send(new Uint8Array([C2S_PING]));
          }
        }, this.pingIntervalMs);
      }
      // Detect supported codecs and inform the server.  Surface subscribes
      // are sent immediately (with codec_support=0) so the first frame
      // arrives without waiting for this async probe.  Once the probe
      // resolves, we send C2S_CLIENT_FEATURES and re-subscribe active
      // surfaces so the server can switch to the optimal encoder.
      detectCodecSupport().then((mask) => {
        this.sendClientFeatures(mask);
        this._codecFeaturesSent = true;
        this.resubscribeWithCodecSupport();
      });
    } else if (
      (status === "error" ||
        status === "disconnected" ||
        status === "closed") &&
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

    // When the transport connects, the blit protocol handshake (S2C_HELLO →
    // S2C_LIST → S2C_READY) hasn't completed yet.  Report "authenticating"
    // so the UI doesn't show a connection as online until S2C_READY confirms
    // the remote is actually reachable and functional.
    const snapshotStatus =
      status === "connected" && !this.snapshot.ready
        ? ("authenticating" as ConnectionStatus)
        : status;

    this.snapshot = {
      ...this.snapshot,
      status: snapshotStatus,
      retryCount: this.retryCount,
      error: this.lastError,
    };

    if (
      status === "disconnected" ||
      status === "closed" ||
      status === "error"
    ) {
      if (this.pingTimer !== null) {
        clearInterval(this.pingTimer);
        this.pingTimer = null;
      }
      this.rejectPendingCreates(
        connectionError(`Transport ${status} before PTY creation completed`),
      );
      this.rejectPendingSearches(connectionError(`Transport ${status}`));
      this.rejectPendingReads(connectionError(`Transport ${status}`));
      this.resolveAllPendingCloses();
      this.surfaceStore.handleDisconnect();
      this.audioPlayer.reset();
      // All server-side surface subscriptions are implicitly dropped
      // when the transport dies, but the CLIENT-SIDE ref-counts (one
      // per live mount) must be preserved: each mount is still there
      // and will call `refreshSurfaceSubscribe` when the store's
      // generation ticks forward, which is how the wire subscribe gets
      // re-sent on reconnect.  Just reset `lastSentQuality` so the
      // refresh actually fires.
      this.resetSurfaceSubsForReconnect();
      // Dismiss all sessions so the UI doesn't show stale terminals from a
      // server that crashed without sending S2C_QUIT.  On reconnect the
      // server's S2C_HELLO + S2C_LIST sequence rebuilds the session list
      // from scratch.
      for (const session of this.sessions) {
        if (session.state !== "closed") {
          this.markSessionClosed(session.id, false);
        }
      }
      this.snapshot = {
        ...this.snapshot,
        ready: false,
        sessions: this.publicSessions,
        focusedSessionId: null,
      };
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
    const previousSession = previousFocus
      ? (this.sessionsById.get(previousFocus) ?? null)
      : null;
    let nextFocus: SessionId | null = null;
    if (previousSession && previousSession.state !== "closed") {
      nextFocus = previousFocus;
    } else if (previousSession && previousSession.state === "closed") {
      // The focused session was closed during reconnect — find the live
      // replacement for the same PTY so focus survives transparently.
      const replacementId = this.currentSessionIdByPtyId.get(
        previousSession.ptyId,
      );
      const replacement = replacementId
        ? (this.sessionsById.get(replacementId) ?? null)
        : null;
      nextFocus =
        replacement && replacement.state !== "closed"
          ? replacement.id
          : this.firstLiveSessionId();
    } else {
      nextFocus = this.firstLiveSessionId();
    }

    this.snapshot = {
      ...this.snapshot,
      focusedSessionId: nextFocus,
    };
    this.store.setLead(
      nextFocus ? (this.sessionsById.get(nextFocus)?.ptyId ?? null) : null,
    );

    this.emit();

    // Always re-send focus to the server. After a reconnection the server
    // has a fresh ClientState with lead=None and needs to be told which
    // session this client is focused on, even if the focus didn't change
    // from the client's perspective.
    if (nextFocus) {
      const session = this.sessionsById.get(nextFocus);
      if (session && this.transport.status === "connected") {
        this.transport.send(buildFocusMessage(session.ptyId));
      }
    }

    // Pruning of superseded sessions is normally deferred until S2C_READY
    // (see the S2C_READY handler) because BSP reconciliation is gated on
    // `ready === true`.  If we pruned here, the closed sessions would be
    // removed before the UI built the old→new session-ID replacement map,
    // wiping pane assignments instead of remapping them.
    //
    // However, if `ready` is already true (e.g. a mid-session re-list,
    // currently not sent by the server but guarding defensively), the
    // emit() above already triggered reconciliation synchronously, so it
    // is safe to prune now.
    if (this.snapshot.ready) {
      queueMicrotask(() => this.pruneSupersededSessions());
    }
  }

  /**
   * Remove closed sessions from `sessionsById`, `sessions`, and `viewSizes`
   * when a live session already exists for the same ptyId.  This prevents
   * stale closed sessions from accumulating across reconnect cycles.
   */
  private pruneSupersededSessions(): void {
    // Collect ptyIds that currently have a live session.
    const livePtyIds = new Set<number>();
    for (const session of this.sessions) {
      if (session.state !== "closed") {
        livePtyIds.add(session.ptyId);
      }
    }

    const toPrune: SessionId[] = [];
    for (const session of this.sessions) {
      if (session.state === "closed" && livePtyIds.has(session.ptyId)) {
        toPrune.push(session.id);
      }
    }

    if (toPrune.length === 0) return;

    for (const id of toPrune) {
      this.sessionsById.delete(id);
      this.viewSizes.delete(id);
    }
    const pruneSet = new Set(toPrune);
    this.sessions = this.sessions.filter(
      (session) => !pruneSet.has(session.id),
    );
    this.invalidatePublicSessions();
    this.snapshot = {
      ...this.snapshot,
      sessions: this.publicSessions,
    };
    this.emit();
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
