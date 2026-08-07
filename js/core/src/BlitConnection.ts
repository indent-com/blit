import type {
  BlitConnectionSnapshot,
  BlitSearchResult,
  BlitSession,
  BlitTransport,
  ConnectionId,
  ConnectionStatus,
  CopyRangeResult,
  SessionId,
  TerminalPalette,
} from "./types";
import { EXIT_STATUS_UNKNOWN } from "./exit-status";
import {
  FEATURE_AUDIO,
  FEATURE_COMPOSITOR,
  FEATURE_COPY_RANGE,
  FEATURE_CREATE_NONCE,
  FEATURE_CREATE_STATUS,
  FEATURE_KILL_MODE,
  FEATURE_RESIZE_BATCH,
  FEATURE_SCROLL_BY,
  FEATURE_RESTART,
  statusText,
  S2C_AUDIO_FRAME,
  PROTOCOL_VERSION,
  S2C_CLIPBOARD_CONTENT,
  S2C_CLOSED,
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_CREATE_FAILED,
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
  S2C_FRAGMENT,
  FRAGMENT_FLAG_LAST,
  S2C_PING,
  S2C_QUIT,
  S2C_TEXT,
  S2C_TITLE,
  S2C_TERM_CWD,
  S2C_TERM_CWD_EVENT,
  S2C_UPDATE,
  S2C_USED_ROWS,
  S2C_SCROLL_OFFSET,
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
  buildTermCwdMessage,
  parseTermCwdReply,
  parseTermCwdEvent,
  buildRestartMessage,
  buildScrollMessage,
  buildScrollByMessage,
  buildSearchMessage,
  buildSurfaceInputMessage,
  buildSurfacePreeditMessage,
  buildSurfaceTextMessage,
  buildSurfacePointerMessage,
  buildSurfaceAxisMessage,
  buildSurfaceAxis2Message,
  type SurfaceAxisEvent,
  buildSurfaceResizeMessage,
  buildSurfaceFocusMessage,
  buildSurfaceCloseMessage,
  buildSurfaceSubscribeMessage,
  buildSurfaceUnsubscribeMessage,
  buildSurfaceAckMessage,
  buildClipboardMessage,
  buildPrimaryMessage,
  buildClientFeaturesMessage,
  buildAudioSubscribeMessage,
  buildAudioUnsubscribeMessage,
} from "./protocol";
import { AudioPlayer } from "./AudioPlayer";
import { SurfaceStore } from "./SurfaceStore";
import { TerminalStore, type BlitWasmModule } from "./TerminalStore";
import {
  detectCodecSupport,
  demoteCodecSupport,
  restoreCodecSupport,
  getMaxDecodeSize,
} from "./BlitSurfaceCanvas";
import {
  FEATURE_FS,
  FS_CLOSED_CLIENT_REQUEST,
  FS_CLOSED_CONNECTION_LOST,
  FS_DONE_CONFLICT,
  FS_DONE_INVALID,
  FS_DONE_OK,
  FS_FILE_OK,
  FS_OP_HARDLINK,
  FS_OP_MKDIR,
  FS_OP_MKPARENTS,
  FS_OP_NO_CAS,
  FS_OP_REMOVE,
  FS_OP_RENAME,
  FS_OP_SYMLINK,
  FS_STATUS_OK,
  FS_SYNC_CONTENT,
  FS_SYNC_CROSS_FILESYSTEM,
  FS_SYNC_DOTIGNORE,
  FS_SYNC_EXCLUDE_GIT,
  FS_SYNC_GITIGNORE,
  FS_SYNC_RECURSIVE,
  FS_SYNC_SINGLE,
  FS_WRITE_CONTENT_DELTA,
  FS_WRITE_CONTENT_FULL,
  FS_WRITE_DURABLE,
  FS_WRITE_MKPARENTS,
  FS_WRITE_NO_CAS,
  FS_UPDATE_RESET,
  FS_UPDATE_SYNC,
  FsMirror,
  FS_INDEX_TRUNCATED,
  S2C_FS_CLOSED,
  S2C_FS_FILE,
  S2C_FS_INDEX,
  S2C_FS_SEARCH,
  S2C_FS_SYNCED,
  S2C_FS_UPDATE,
  S2C_FS_DONE,
  buildFsAckMessage,
  buildFsFetchMessage,
  buildFsIndexMessage,
  buildFsSearchMessage,
  buildFsOpMessage,
  buildFsStopMessage,
  buildFsSyncMessage,
  buildFsWriteMessage,
  parseFsIndexResult,
  buildFsGrepMessage,
  parseFsGrepResult,
  S2C_FS_GREP,
  FS_GREP_TRUNCATED,
  parseFsSearchResult,
  fsDoneStatusText,
  fsFileStatusText,
  parseFsDoneMessage,
  parseFsFileMessage,
  encodeFsDelta,
  FS_MAX_DECOMPRESSED,
  FsConflictError,
  FsOpenError,
  type FsFileIndex,
  type FsGrepResult,
  type FsGrepOptions,
  type FsNode,
  type FsRecord,
  type FsSyncHandle,
  type FsSyncOptions,
  type FsWriteOptions,
  type FsWriteResult,
} from "./fs";
import {
  FEATURE_GIT,
  GIT_CLOSED_CONNECTION_LOST,
  GIT_OPEN_IGNORED,
  GIT_OPEN_STATUS,
  GIT_OID_NONE,
  GIT_OPEN_REMOTES,
  GIT_STATUS_CANCELLED,
  GitStatusError,
  msgGitBlame,
  msgGitCancel,
  msgGitDiscover,
  msgGitFetch,
  msgGitReflog,
  parseGitDiscoverResp,
  gitDiscoverRecords,
  GIT_DISCOVER_BARE,
  GIT_DISCOVER_NESTED,
  GIT_DISCOVER_TRUNCATED,
  GIT_FOUND_BARE,
  GIT_FOUND_LINKED,
  GIT_FOUND_SUBMODULE,
  GIT_OPEN_TRACKING,
  GIT_OPEN_UNTRACKED,
  GIT_OPEN_WATCH,
  GIT_PATCH_STRUCTURED,
  GIT_STATUS_OK,
  GitStateMirror,
  type GitLogPage,
  type GitLogSubscription,
  type GitLogWatchOptions,
  type GitDiscoverOptions,
  type GitFoundRepo,
  type GitOpenOptions,
  type GitRepoHandle,
  S2C_GIT_BASE,
  S2C_GIT_BLAME,
  S2C_GIT_DISCOVER,
  S2C_GIT_BLOB,
  S2C_GIT_CLOSED,
  S2C_GIT_COMMITS,
  S2C_GIT_DIFF,
  S2C_GIT_INDEX,
  S2C_GIT_LOG_PAGE,
  S2C_GIT_FETCH,
  S2C_GIT_PATCH,
  S2C_GIT_REFLOG,
  S2C_GIT_REPO,
  S2C_GIT_RESOLVE,
  S2C_GIT_STATE,
  S2C_GIT_TREE,
  gitDiffRecords,
  gitIndexRecords,
  gitOidHex,
  gitPatchRecords,
  gitBlameRecords,
  gitFetchRecords,
  gitReflogRecords,
  gitTreeRecords,
  msgGitAck,
  msgGitBase,
  msgGitBlob,
  msgGitClose,
  msgGitDiff,
  msgGitIndex,
  msgGitLog,
  msgGitLogAck,
  msgGitLogUnwatch,
  msgGitLogWatch,
  msgGitOpen,
  msgGitPatch,
  msgGitResolve,
  msgGitTree,
  parseGitBaseResp,
  parseGitBlameResp,
  parseGitBlobResp,
  parseGitClosed,
  parseGitCommits,
  parseGitDiffResp,
  parseGitIndexResp,
  parseGitLogPage,
  parseGitFetchResp,
  parseGitPatchResp,
  parseGitReflogResp,
  parseGitRepo,
  parseGitResolveResp,
  parseGitTreeResp,
} from "./git";
import {
  FEATURE_LSP,
  LSP_BUFFER_RELEASE,
  LSP_CLOSED_CONNECTION_LOST,
  LSP_OPEN_DIAGS,
  LSP_OPEN_WATCH,
  LSP_QUERY_COMPLETION,
  LSP_QUERY_DEFINITION,
  LSP_QUERY_DOC_SYMBOLS,
  LSP_QUERY_HOVER,
  LSP_QUERY_REFERENCES,
  LSP_QUERY_RENAME,
  LSP_QUERY_SIGNATURE,
  LSP_QUERY_WS_SYMBOLS,
  LSP_REFS_INCLUDE_DECLARATION,
  LSP_RESP_INCOMPLETE,
  LSP_RESP_TRUNCATED,
  LSP_STATUS_OK,
  LSP_STREAM_DIAG,
  LSP_STREAM_STATE,
  LspDiagMirror,
  LspStateMirror,
  type LspHandle,
  type LspOpenOptions,
  type LspQueryResult,
  S2C_LSP_CLOSED,
  S2C_LSP_DIAG,
  S2C_LSP_OPENED,
  S2C_LSP_QUERY,
  S2C_LSP_STATE,
  lspQueryRecords,
  lspStatusText,
  msgLspAck,
  msgLspBuffer,
  msgLspClose,
  msgLspOpen,
  msgLspQuery,
  parseLspClosed,
  parseLspOpened,
  parseLspQueryResp,
} from "./lsp";
import {
  FEATURE_KV,
  KV_PUT_DELETE,
  KV_PUT_DURABLE,
  KV_PUT_NO_CAS,
  KV_STATUS_CONFLICT,
  KV_STATUS_NOT_FOUND,
  KV_STATUS_OK,
  KvMirror,
  S2C_KV_CLOSED,
  S2C_KV_DONE,
  S2C_KV_OPENED,
  S2C_KV_UPDATE,
  S2C_KV_VALUE,
  buildKvAckMessage,
  buildKvFetchMessage,
  buildKvOpenMessage,
  buildKvPutMessage,
  buildKvStopMessage,
  kvClosedText,
  kvStatusText,
  parseKvDoneMessage,
  parseKvOpenedMessage,
  parseKvValueMessage,
  type KvFetchResult,
  type KvPutOptions,
  type KvWatchHandle,
  type KvWatchOptions,
} from "./kv";
import { Notifier } from "./reactive";

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
  /** Working directory for the new session. Interpreted on the target server. */
  cwd?: string;
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

type ListEntry = {
  ptyId: number;
  tag: string;
  command: string | null;
};

type ParsedList = {
  entries: ListEntry[];
  complete: boolean;
};

/** One `syncFs` caller on a shared wire sync: its callbacks + reactive
 *  surface. Wire-identical opens share one sync (docs/fs-watch.md), so
 *  callbacks fan out per consumer while the mirror is shared. */
type FsSyncConsumer = {
  options: FsSyncOptions;
  notifier: Notifier;
  /** Callbacks held back until this consumer's opener holds its handle
   *  (`dispatchFs`); null once they run inline. */
  held: HeldFsCallback[] | null;
  /** Hash of this consumer's most recent write per path, for self-echo
   *  suppression (docs/design/fs-write.md "Echo and attribution"). Scoped
   *  to the consumer, not the share: another handle's write on the same
   *  shared sync is an external change to this one — two editors on one
   *  file must not both swallow the echo. Entries are dropped once the
   *  matching echo upsert is observed. */
  lastWritten: Map<string, bigint>;
};

/** A consumer callback waiting for its opener. `mirrored` marks the ones a
 *  restage of the mirror reproduces (RESET / records / SYNC / onUpdate), so
 *  a replay — or a consumer that stopped meanwhile — can drop them; a close
 *  notification is never reproducible and always runs. */
type HeldFsCallback = { deliver: () => void; mirrored: boolean };

/** One wire sync, shared by every consumer whose open was wire-identical. */
type FsSyncShare = {
  /** Coalescing key: flags + latency + inline cap + src pty + path. */
  key: string;
  syncId: number;
  root: string;
  mirror: FsMirror;
  /** True once any `SYNC` has been seen — the live map is coherent. */
  synced: boolean;
  consumers: Set<FsSyncConsumer>;
};

/** An unanswered `C2S_FS_SYNC` and every caller waiting on it. */
type PendingFsSync = {
  key: string;
  waiters: Array<{
    resolve: (handle: FsSyncHandle) => void;
    reject: (error: Error) => void;
    options: FsSyncOptions;
  }>;
};

/** Synthesize the upsert a joiner would have received for a mirrored node. */
function fsNodeUpsert(path: string, node: FsNode): FsRecord {
  return {
    kind: "upsert",
    path,
    entryFlags: node.entryFlags,
    size: node.size,
    mtimeNs: node.mtimeNs,
    mode: node.mode,
    hash: node.hash,
    content:
      node.content !== null
        ? { kind: "full", data: node.content }
        : { kind: "none" },
  };
}

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

/** A fixed encode size one view wants for a surface, in pixels. */
export interface SurfaceTarget {
  width: number;
  height: number;
}

/** Per-surface subscription state.  One entry per visible surface on
 *  this connection.  `views` tracks the live mounts (e.g. BSP view plus
 *  side-panel preview) sharing the stream: the wire UNSUBSCRIBE fires
 *  only when the last one goes away.  Without that, unmounting one of
 *  two mounts tears down the stream for both. */
interface SurfaceSub {
  surfaceId: number;
  /** Live mounts, keyed by the token allocSurfaceViewId() handed out.
   *  The value is the fixed encode size that view wants, or null when it
   *  wants the mediated surface at full size.  Held per view rather than
   *  collapsed to a count because the effective request is derived from
   *  all of them — and on unmount we have to know which one left. */
  views: Map<string, SurfaceTarget | null>;
  /** Bandwidth override set via {@link BlitConnection.sendSurfaceResubscribe}. */
  bandwidthOverride: number | null;
  /** Speed override set via {@link BlitConnection.sendSurfaceResubscribe}. */
  speedOverride: number | null;
  /** Last subscribe sent on the wire, for dedup. */
  lastSent: {
    bandwidth: number;
    speed: number;
    width: number;
    height: number;
  } | null;
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
  private readonly scrollAnchorListeners = new Set<{
    ptyId: number;
    listener: (offset: number) => void;
  }>();
  private readonly sessionsById = new Map<SessionId, InternalSession>();
  private readonly currentSessionIdByPtyId = new Map<number, SessionId>();
  private readonly pendingCreates = new Map<number, PendingCreate>();
  private readonly pendingCloses = new Map<SessionId, Array<() => void>>();
  private readonly pendingSearches = new Map<number, PendingSearch>();
  private readonly pendingReads = new Map<
    number,
    {
      resolve: (result: CopyRangeResult) => void;
      reject: (error: Error) => void;
    }
  >();
  /** Unanswered `C2S_FS_SYNC`s by nonce; `pendingFsSyncsByKey` indexes the
   *  same entries so wire-identical opens coalesce while in flight. */
  private readonly pendingFsSyncs = new Map<number, PendingFsSync>();
  private readonly pendingFsSyncsByKey = new Map<string, PendingFsSync>();
  /** Live syncs by server `sync_id`; `fsSyncsByKey` indexes the same
   *  shares for coalescing until their last consumer stops. */
  private readonly fsSyncs = new Map<number, FsSyncShare>();
  private readonly fsSyncsByKey = new Map<string, FsSyncShare>();
  private readonly pendingFsFetches = new Map<
    number,
    { resolve: (data: Uint8Array) => void; reject: (error: Error) => void }
  >();
  private readonly pendingFsSearches = new Map<
    number,
    { resolve: (paths: string[]) => void; reject: (error: Error) => void }
  >();
  private readonly pendingFsIndexes = new Map<
    number,
    { resolve: (index: FsFileIndex) => void; reject: (error: Error) => void }
  >();
  private readonly pendingFsGreps = new Map<
    number,
    { resolve: (result: FsGrepResult) => void; reject: (error: Error) => void }
  >();
  private readonly pendingCwds = new Map<
    number,
    { resolve: (cwd: string) => void; reject: (error: Error) => void }
  >();
  private cwdNonceCounter = 0;
  /** Latest server-pushed cwd per session (`S2C_TERM_CWD_EVENT`); cleared
   *  on reset/HELLO — pushes do not survive a server session change. */
  private readonly termCwds = new Map<SessionId, string>();
  private readonly termCwdListeners = new Set<
    (sessionId: SessionId, cwd: string) => void
  >();
  private readonly pendingFsWrites = new Map<
    number,
    {
      resolve: (result: FsWriteResult) => void;
      reject: (error: Error) => void;
      /** Set on `writeFile`/`mkdir` so a successful reply records the hash
       *  in the issuing consumer's `lastWritten`; unset for remove/rename. */
      record?: { consumer: FsSyncConsumer; path: string };
      /** Set on a delta write: an `FS_DONE` INVALID re-sends it as a full
       *  write instead of rejecting (a pre-delta server's refusal). */
      onInvalid?: () => void;
    }
  >();
  private readonly pendingKvOpens = new Map<
    number,
    {
      resolve: (handle: KvWatchHandle) => void;
      reject: (error: Error) => void;
      options: KvWatchOptions;
    }
  >();
  private readonly kvWatches = new Map<
    number,
    { mirror: KvMirror; options: KvWatchOptions }
  >();
  private readonly pendingKvPuts = new Map<
    number,
    {
      resolve: (result: { hash: bigint; mtimeNs: bigint }) => void;
      reject: (error: Error) => void;
    }
  >();
  private readonly pendingKvFetches = new Map<
    number,
    {
      resolve: (result: KvFetchResult | null) => void;
      reject: (error: Error) => void;
    }
  >();
  private readonly pendingGitOpens = new Map<
    number,
    {
      resolve: (handle: GitRepoHandle) => void;
      reject: (error: Error) => void;
      options: GitOpenOptions;
    }
  >();
  private readonly gitRepos = new Map<
    number,
    { mirror: GitStateMirror; options: GitOpenOptions; notifier: Notifier }
  >();
  /** Connection-wide blob cache: oid-addressed content is immutable
   *  (docs/design/git.md "GIT_BLOB"), so entries outlive repo handles and
   *  reconnects. Promises coalesce concurrent fetches of one oid; Map
   *  order is the LRU order for the byte budget. */
  private readonly gitBlobCache = new Map<
    string,
    { promise: Promise<Uint8Array>; bytes: number; settled: boolean }
  >();
  private gitBlobCacheBytes = 0;
  /** Byte budget for {@link gitBlobCache}; tests shrink it to exercise
   *  eviction without moving real megabytes. */
  private gitBlobCacheBudget = 64 * 1024 * 1024;
  private readonly pendingGitRequests = new Map<
    number,
    {
      opcode: number;
      resolve: (msg: Uint8Array) => void;
      reject: (error: Error) => void;
      /** Set once the caller aborted: the promise is already rejected and
       *  the entry stays only to reserve the nonce. The wire promises
       *  exactly one response per nonce and answers a duplicate in-flight
       *  nonce with INVALID, so the id cannot be reused until the real
       *  reply lands and is dropped. */
      abandoned?: boolean;
    }
  >();
  /** Live log subscriptions keyed by client-assigned `log_id`. */
  private readonly gitLogSubs = new Map<
    number,
    { repoId: number; onUpdate: (page: GitLogPage) => void }
  >();
  private readonly pendingLspOpens = new Map<
    number,
    {
      resolve: (handle: LspHandle) => void;
      reject: (error: Error) => void;
      options: LspOpenOptions;
    }
  >();
  private readonly lspAttachments = new Map<
    number,
    {
      state: LspStateMirror;
      diags: LspDiagMirror;
      options: LspOpenOptions;
      notifier: Notifier;
    }
  >();
  private readonly pendingLspRequests = new Map<
    number,
    {
      resolve: (msg: Uint8Array) => void;
      reject: (error: Error) => void;
    }
  >();

  private sessionCounter = 0;
  private nonceCounter = 0;
  private searchCounter = 0;
  private fsNonceCounter = 0;
  private gitLogIdCounter = 0;
  private features = 0;
  private disposed = false;
  /** Per-session, per-view size registry for computing minimum resize. */
  private viewSizes = new Map<
    SessionId,
    Map<string, { rows: number; cols: number }>
  >();
  private viewIdCounter = 0;
  private hasReceivedList = false;
  private retryCount = 0;
  private generation = 0;
  private lastError: string | null = null;

  /** Default video bandwidth for new surface subscriptions (0 = server default). */
  defaultSurfaceBandwidth = 0;
  /** Default encoder speed for new surface subscriptions (0 = server default). */
  defaultSurfaceSpeed = 0;
  /** Default audio bitrate in kbps for audio subscriptions (0 = server default). */
  defaultAudioBitrateKbps = 0;
  /** When false, surface subscribe messages are suppressed (ref-counts
   *  still tracked so re-enabling restores subscriptions). */
  surfaceStreamingEnabled = true;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private readonly pingIntervalMs = 10_000;

  /**
   * Reassembly buffer for `S2C_FRAGMENT` messages.  TCP preserves order
   * and the server only splits one bulk message at a time, so a single
   * buffer is enough — fragments of different messages never interleave.
   * Audio frames and other small messages may arrive between fragments
   * and are dispatched immediately, bypassing this buffer.
   */
  private fragmentChunks: Uint8Array[] = [];
  private fragmentBytes = 0;

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
        sub.lastSent = null;
        this.maybeSendSurfaceSubscribe(sub);
      }
    });
    this.surfaceStore.setCodecDemoter((surfaceId, bits) => {
      // A stream that keeps failing to decode after keyframe recoveries is
      // one this platform's decoder rejects outright — the probe's tiny
      // test frames passed on a decoder that rejects the real thing.
      // Withdraw the codec-support bits that selected it and renegotiate:
      // the server re-runs encoder selection against the reduced mask and
      // every subscribed surface switches to a stream we can decode.
      const mask = demoteCodecSupport(bits);
      if (mask === null) return;
      this._logger.warn(
        `surface ${surfaceId}: repeated decode failures, dropping codec ` +
          `support 0x${bits.toString(16)} (now 0x${mask.toString(16)}) and renegotiating`,
      );
      if (this.transport.status === "connected") {
        this.sendClientFeatures(mask);
        this.resubscribeWithCodecSupport();
      }
      this.scheduleCodecProbation(bits);
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
      // When the transport is already connected, the blit server may not have
      // sent its first frame yet — report "authenticating" until server
      // activity proves the upstream is responsive.
      status:
        transport.status === "connected" ? "authenticating" : transport.status,
      ready: false,
      supportsRestart: false,
      supportsCopyRange: false,
      supportsCompositor: false,
      supportsAudio: false,
      supportsFsSync: false,
      supportsGit: false,
      supportsLsp: false,
      supportsKv: false,
      retryCount: 0,
      bootGeneration: null,
      serverVersion: null,
      generation: 0,
      error: null,
      sessions: [],
      focusedSessionId: null,
    };

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
    this.resetFsSyncs(connectionError("Connection disposed"));
    this.resetGitRepos(connectionError("Connection disposed"));
    this.resetLspAttachments(connectionError("Connection disposed"));
    this.resetKv(connectionError("Connection disposed"));
    this.resetFragmentReassembly();
    for (const p of this.codecProbation.values()) {
      if (p.timer !== null) clearTimeout(p.timer);
    }
    this.codecProbation.clear();
    this.termCwds.clear();
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
          cwd: options.cwd,
          wantStatus: (this.features & FEATURE_CREATE_STATUS) !== 0,
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
  ): Promise<CopyRangeResult> {
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
    return new Promise<CopyRangeResult>((resolve, reject) => {
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

  /**
   * Signals a terminal. Reaches the child's process group by default, which
   * is what a "kill this terminal" affordance means and what the kernel does
   * for a real `^C`. `leaderOnly` addresses the session leader alone; it is
   * dropped against a server without {@link FEATURE_KILL_MODE}, which is
   * leader-only regardless.
   */
  killSession(sessionId: SessionId, signal = 15, leaderOnly = false): void {
    const session = this.sessionsById.get(sessionId);
    if (
      !session ||
      session.state !== "active" ||
      this.transport.status !== "connected"
    ) {
      return;
    }
    this.transport.send(
      buildKillMessage(
        session.ptyId,
        signal,
        leaderOnly && (this.features & FEATURE_KILL_MODE) !== 0,
      ),
    );
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

  /**
   * Move a scrolled view by `lines` rather than to a position.
   *
   * A gesture is relative — a notch, a page, a drag — and the absolute
   * offset it works out to only means what the user intended for as long as
   * the live bottom it counts from stays put. Under a chatty app the server
   * moves that bottom, and re-anchors this client, while the request is in
   * flight; the stale absolute then lands short by however many lines
   * scrolled in between.
   *
   * `offset` is where the caller believes the move lands, used verbatim
   * against a server too old to know the relative form.
   */
  scrollSessionBy(sessionId: SessionId, offset: number, lines: number): void {
    const session = this.sessionsById.get(sessionId);
    if (
      !session ||
      !isLiveSession(session) ||
      this.transport.status !== "connected"
    ) {
      return;
    }
    this.transport.send(
      (this.features & FEATURE_SCROLL_BY) !== 0
        ? buildScrollByMessage(session.ptyId, lines)
        : buildScrollMessage(session.ptyId, offset),
    );
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

  /**
   * Mirror a server-side directory tree (docs/fs-watch.md). Resolves once
   * the server accepts the sync; the handle's `live` map fills as the
   * staged snapshot streams in and `onSync` fires when it is coherent.
   * Updates are applied and acknowledged automatically.
   *
   * No callback of this open runs before the opener holds its handle: the
   * accept echo and the first snapshot can arrive in one transport chunk,
   * and a chunk's frames are dispatched in one synchronous loop, so the
   * callbacks of a not-yet-handed-off consumer are held and released on a
   * task of their own (`dispatchFs`). Only the mirror and the wire acks
   * advance synchronously.
   *
   * Wire-identical opens (same path and options) share one server sync:
   * each caller gets its own handle and callbacks, and the wire stop goes
   * out when the last handle stops. A caller joining an established sync
   * has its snapshot replayed from the mirror (`onReset`/`onRecord`/
   * `onSync`) on that same later task, so per-record consumers stay
   * coherent.
   */
  async syncFs(
    path: string,
    options: FsSyncOptions = {},
  ): Promise<FsSyncHandle> {
    if (this.transport.status !== "connected") {
      throw connectionError(
        `Cannot sync while transport is ${this.transport.status}`,
      );
    }
    if ((this.features & FEATURE_FS) === 0) {
      throw connectionError("Server does not support filesystem sync");
    }
    if (options.single && options.recursive) {
      throw connectionError(
        "A single-file sync cannot be recursive (docs/design/fs-watch.md)",
      );
    }
    // Exclusion narrows enumeration and a single-file sync enumerates
    // nothing; the server refuses the pair, so say so here rather than
    // spend a round trip on it.
    const exclude = (options.exclude ?? []).join("\n");
    if (
      options.single &&
      (options.ignore ||
        options.gitignore ||
        options.dotIgnore ||
        options.excludeGit ||
        exclude)
    ) {
      throw connectionError(
        "A single-file sync cannot exclude anything (docs/design/fs-watch.md)",
      );
    }
    let flags = 0;
    // `single` and the recursive default are exclusive: a single-file
    // root has nothing to recurse into, and the server rejects the
    // flag combination. The bit rides `flags`, so the coalescing key
    // below separates single and directory opens automatically.
    if (options.single) flags |= FS_SYNC_SINGLE;
    else if (options.recursive !== false) flags |= FS_SYNC_RECURSIVE;
    if (options.content) flags |= FS_SYNC_CONTENT;
    if (options.crossFilesystem) flags |= FS_SYNC_CROSS_FILESYSTEM;
    if (options.ignore || options.gitignore) flags |= FS_SYNC_GITIGNORE;
    if (options.ignore || options.dotIgnore) flags |= FS_SYNC_DOTIGNORE;
    if (options.ignore || options.excludeGit) flags |= FS_SYNC_EXCLUDE_GIT;
    const srcPtyId = this.srcPtyForOpen(options.fromSessionId);
    const latencyMs = options.latencyMs ?? 0;
    const inlineMax = options.inlineMax ?? 0;
    // The pattern list is part of what the sync *is* — two opens that
    // exclude different things mirror different trees — so it joins the
    // coalescing key alongside the flags.
    const key = `${flags}:${latencyMs}:${inlineMax}:${srcPtyId ?? ""}:${exclude.length}:${exclude}${path}`;
    const share = this.fsSyncsByKey.get(key);
    if (share) {
      return this.joinFsShare(share, options);
    }
    const pending = this.pendingFsSyncsByKey.get(key);
    if (pending) {
      return new Promise<FsSyncHandle>((resolve, reject) => {
        pending.waiters.push({ resolve, reject, options });
      });
    }
    return new Promise<FsSyncHandle>((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingFsSyncs);
      const entry: PendingFsSync = {
        key,
        waiters: [{ resolve, reject, options }],
      };
      this.pendingFsSyncs.set(nonce, entry);
      this.pendingFsSyncsByKey.set(key, entry);
      this.transport.send(
        buildFsSyncMessage(
          nonce,
          flags,
          latencyMs,
          inlineMax,
          path,
          srcPtyId,
          exclude,
        ),
      );
    });
  }

  /** Attach a consumer to an established share, replaying the snapshot it
   *  missed. Like a fresh open, its callbacks are held until the caller
   *  holds the handle (`dispatchFs`); the replay runs on that same task and
   *  re-reads the mirror, so it subsumes — and therefore drops — whatever
   *  the wire delivered in between. The replay is always a valid restage of
   *  everything that preceded it. */
  private joinFsShare(
    share: FsSyncShare,
    options: FsSyncOptions,
  ): FsSyncHandle {
    const consumer: FsSyncConsumer = {
      options,
      notifier: new Notifier(),
      held: [],
      lastWritten: new Map<string, bigint>(),
    };
    share.consumers.add(consumer);
    const handle = this.makeFsSyncHandle(share, consumer);
    setTimeout(() => {
      // Stopped or closed already: only the close notification is owed.
      if (share.consumers.has(consumer)) {
        this.dropHeldFsRecords(consumer);
        const staged = share.mirror.staged;
        if (staged !== null) {
          // Mid-restage: replay the staging map — it equals the records the
          // other consumers saw since RESET — and let the wire SYNC land.
          options.onReset?.();
          if (options.onRecord) {
            for (const [p, node] of staged) {
              options.onRecord(fsNodeUpsert(p, node));
            }
          }
        } else if (share.synced) {
          // Coherent: replay the live map as the restage the joiner missed.
          options.onReset?.();
          if (options.onRecord) {
            for (const [p, node] of share.mirror.live) {
              options.onRecord(fsNodeUpsert(p, node));
            }
          }
          options.onSync?.();
          // Revision before onUpdate, matching the wire path: revision-keyed
          // caches re-read inside onUpdate must observe the bump.
          consumer.notifier.emit();
          options.onUpdate?.();
        }
        // Neither: the initial snapshot has not started; the wire
        // RESET…SYNC reaches this consumer like any other.
      }
      this.releaseHeldFs(consumer);
    }, 0);
    return handle;
  }

  /** Deliver one consumer callback, honoring the open contract: nothing
   *  fires before that consumer's opener holds its handle. The opener's
   *  promise continuation and the frames of one transport chunk are not
   *  interleaved — a chunk is dispatched in a single synchronous loop
   *  (webtransport / mux / unix), so an `FS_UPDATE` riding the same chunk
   *  as the `FS_SYNCED` that resolves the open is handled first. Held
   *  callbacks keep their order and are released together. */
  private dispatchFs(
    consumer: FsSyncConsumer,
    deliver: () => void,
    mirrored = true,
  ): void {
    if (consumer.held) consumer.held.push({ deliver, mirrored });
    else deliver();
  }

  /** Release a consumer's held callbacks, in order. Callbacks appended
   *  while draining (a callback that stops the handle) stay in order; the
   *  gate opens only once the queue has run dry. */
  private releaseHeldFs(consumer: FsSyncConsumer): void {
    const held = consumer.held;
    if (!held) return;
    for (let i = 0; i < held.length; i++) held[i].deliver();
    consumer.held = null;
  }

  /** Drop the held callbacks a mirror restage reproduces, keeping the close
   *  notifications. Used when the replay that supersedes them is about to
   *  run, and when the consumer is going away. */
  private dropHeldFsRecords(consumer: FsSyncConsumer): void {
    const held = consumer.held;
    if (!held) return;
    // In place: the array being drained is the queue itself, so a drop from
    // inside a callback still takes effect on the rest of the drain.
    let kept = 0;
    for (const callback of held) {
      if (!callback.mirrored) held[kept++] = callback;
    }
    held.length = kept;
  }

  private makeFsSyncHandle(
    share: FsSyncShare,
    consumer: FsSyncConsumer,
  ): FsSyncHandle {
    const { syncId, mirror } = share;
    const notifier = consumer.notifier;
    return {
      syncId,
      root: share.root,
      subscribe: notifier.subscribe,
      get revision() {
        return notifier.revision;
      },
      get live() {
        // The mirror *replaces* its live map when a staged snapshot
        // swaps in, so the handle must dereference on every access.
        return mirror.live;
      },
      fetch: (path: string) => this.fsFetch(syncId, path),
      writeFile: (path, data, options = {}) =>
        this.fsWrite(syncId, consumer, path, data, options),
      mkdir: (path, options = {}) =>
        this.fsOp(
          syncId,
          FS_OP_MKDIR,
          path,
          "",
          0n,
          options.mode ?? 0,
          options.createParents ? FS_OP_MKPARENTS : 0,
          { consumer, path },
        ),
      remove: (path, options = {}) =>
        this.fsOp(
          syncId,
          FS_OP_REMOVE,
          path,
          "",
          options.ifHash ?? 0n,
          0,
          0,
        ).then(() => undefined),
      rename: (from, to, options = {}) =>
        this.fsOp(
          syncId,
          FS_OP_RENAME,
          from,
          to,
          0n,
          0,
          options.createParents ? FS_OP_MKPARENTS : 0,
        ).then(() => undefined),
      symlink: (target, path, options = {}) =>
        this.fsOp(
          syncId,
          FS_OP_SYMLINK,
          target,
          path,
          options.force ? 0n : (options.ifHash ?? 0n),
          0,
          (options.force ? FS_OP_NO_CAS : 0) |
            (options.createParents ? FS_OP_MKPARENTS : 0),
          { consumer, path },
        ),
      hardlink: (source, path, options = {}) =>
        this.fsOp(
          syncId,
          FS_OP_HARDLINK,
          source,
          path,
          options.force ? 0n : (options.ifHash ?? 0n),
          0,
          (options.force ? FS_OP_NO_CAS : 0) |
            (options.createParents ? FS_OP_MKPARENTS : 0),
          { consumer, path },
        ),
      lastWrittenHash: (path: string) => consumer.lastWritten.get(path),
      stop: () => this.releaseFsConsumer(share, consumer),
    };
  }

  /** Detach one consumer; the last one releases the wire sync. */
  private releaseFsConsumer(
    share: FsSyncShare,
    consumer: FsSyncConsumer,
  ): void {
    if (!share.consumers.has(consumer)) return;
    if (share.consumers.size > 1) {
      share.consumers.delete(consumer);
      // Nothing this consumer never saw is owed to it any more, but its
      // close still is: stand in for the server confirmation the surviving
      // consumers will keep absorbing.
      this.dropHeldFsRecords(consumer);
      queueMicrotask(() => {
        this.dispatchFs(
          consumer,
          () => {
            consumer.options.onClosed?.(FS_CLOSED_CLIENT_REQUEST);
            consumer.notifier.emit();
          },
          false,
        );
      });
      return;
    }
    // Last consumer: release the wire sync. Unindex the key first so a
    // new open starts fresh instead of joining a dying sync; FS_CLOSED
    // (or the connection teardown) fires the remaining onClosed.
    if (this.fsSyncsByKey.get(share.key) === share) {
      this.fsSyncsByKey.delete(share.key);
    }
    if (this.transport.status === "connected") {
      this.transport.send(buildFsStopMessage(share.syncId));
    }
  }

  private nextFsNonce(pending: ReadonlyMap<number, unknown>): number {
    let nonce = 0;
    do {
      nonce = this.fsNonceCounter = (this.fsNonceCounter + 1) & 0xffff;
    } while (pending.has(nonce));
    return nonce;
  }

  private fsFetch(syncId: number, path: string): Promise<Uint8Array> {
    return new Promise<Uint8Array>((resolve, reject) => {
      if (!this.fsSyncs.has(syncId)) {
        reject(connectionError("Sync is closed"));
        return;
      }
      const nonce = this.nextFsNonce(this.pendingFsFetches);
      this.pendingFsFetches.set(nonce, { resolve, reject });
      this.transport.send(buildFsFetchMessage(nonce, syncId, path));
    });
  }

  /** Resolve a session's live working directory (server reads the pty's cwd).
   *  Resolves "" when the session/pty is gone or the cwd can't be read. */
  sessionCwd(sessionId: SessionId): Promise<string> {
    if (this.transport.status !== "connected") return Promise.resolve("");
    const session = this.sessionsById.get(sessionId);
    if (!session) return Promise.resolve("");
    // Servers predating TERM_CWD never answer, so pendings only leave this
    // map on a connection reset — cap them like pendingFsIndexes. "" is
    // the documented can't-read result.
    if (this.pendingCwds.size >= 8) return Promise.resolve("");
    return new Promise<string>((resolve, reject) => {
      let nonce = 0;
      do {
        nonce = this.cwdNonceCounter = (this.cwdNonceCounter + 1) & 0xffff;
      } while (this.pendingCwds.has(nonce));
      this.pendingCwds.set(nonce, { resolve, reject });
      this.transport.send(buildTermCwdMessage(nonce, session.ptyId));
    });
  }

  /** Subscribe to server-pushed cwd changes (`S2C_TERM_CWD_EVENT`,
   *  docs/protocol.md): the server watches OSC 7 reports, so consumers
   *  can suppress `sessionCwd` polling while pushes flow. Returns an
   *  unsubscribe function. */
  onTermCwd(listener: (sessionId: SessionId, cwd: string) => void): () => void {
    this.termCwdListeners.add(listener);
    return () => {
      this.termCwdListeners.delete(listener);
    };
  }

  /** The most recent server-pushed cwd for a session, or null when none
   *  has arrived since the session (or server connection) was
   *  established. The `sessionCwd()` poll is independent of pushes. */
  lastPushedCwd(sessionId: SessionId): string | null {
    return this.termCwds.get(sessionId) ?? null;
  }

  /** Fuzzy file search under `root`; resolves with up to `limit` root-relative
   *  paths, best match first. No sync — a one-shot server-side walk. */
  searchFiles(root: string, query: string, limit = 50): Promise<string[]> {
    if (this.transport.status !== "connected") {
      return Promise.reject(
        connectionError(
          `Cannot search while transport is ${this.transport.status}`,
        ),
      );
    }
    if ((this.features & FEATURE_FS) === 0) {
      return Promise.reject(
        connectionError("Server does not support file search"),
      );
    }
    return new Promise<string[]>((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingFsSearches);
      this.pendingFsSearches.set(nonce, { resolve, reject });
      this.transport.send(
        buildFsSearchMessage(nonce, Math.min(limit, 0xffff), root, query),
      );
    });
  }

  /** Fetch the candidate file list under `root` for client-side fuzzy
   *  search (docs/design/fs-search.md): root-relative paths, sorted,
   *  gitignore-filtered. `truncated` means a budget clipped the list, so
   *  callers should keep `searchFiles` for this root. Servers predating
   *  `FS_INDEX` never answer — the promise just stays pending until the
   *  connection resets, so callers should race it against a fallback. */
  indexFiles(root: string): Promise<FsFileIndex> {
    if (this.transport.status !== "connected") {
      return Promise.reject(
        connectionError(
          `Cannot index files while transport is ${this.transport.status}`,
        ),
      );
    }
    if ((this.features & FEATURE_FS) === 0) {
      return Promise.reject(
        connectionError("Server does not support file indexing"),
      );
    }
    // A pre-FS_INDEX server never answers, so pendings can only leave this
    // map on a connection reset — cap them so a polling caller can't grow
    // the map without bound (every other fs pending is guaranteed a reply).
    if (this.pendingFsIndexes.size >= 8) {
      return Promise.reject(
        connectionError("Too many file index requests in flight"),
      );
    }
    return new Promise<FsFileIndex>((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingFsIndexes);
      this.pendingFsIndexes.set(nonce, { resolve, reject });
      this.transport.send(buildFsIndexMessage(nonce, root));
    });
  }

  /** Content search under `root` (docs/design/fs-grep.md). Resolves with
   *  hits grouped by file — tracked files first, then gitignored ones,
   *  which are ranked rather than excluded. No sync: a one-shot
   *  server-side walk.
   *
   *  Rejects on a non-OK status, carrying the server's own message where
   *  it has one — an uncompilable regex reports the engine's wording,
   *  which is the useful thing to show someone mid-typing. Like
   *  `indexFiles`, a server predating `FS_GREP` drops the opcode and never
   *  answers, so the in-flight cap keeps a repeating caller bounded. */
  grep(
    root: string,
    query: string,
    opts: FsGrepOptions = {},
  ): Promise<FsGrepResult> {
    if (this.transport.status !== "connected") {
      return Promise.reject(
        connectionError(
          `Cannot search while transport is ${this.transport.status}`,
        ),
      );
    }
    if ((this.features & FEATURE_FS) === 0) {
      return Promise.reject(
        connectionError("Server does not support content search"),
      );
    }
    if (this.pendingFsGreps.size >= 8) {
      return Promise.reject(
        connectionError("Too many content searches in flight"),
      );
    }
    return new Promise<FsGrepResult>((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingFsGreps);
      this.pendingFsGreps.set(nonce, { resolve, reject });
      this.transport.send(buildFsGrepMessage(nonce, root, query, opts));
    });
  }

  // -- Server KV store (docs/design/kv.md) ----------------------------------

  private kvGuard(): Error | null {
    if (this.transport.status !== "connected") {
      return connectionError(
        `Cannot use kv while transport is ${this.transport.status}`,
      );
    }
    if ((this.features & FEATURE_KV) === 0) {
      return connectionError("Server does not support the kv store");
    }
    return null;
  }

  /** CAS put: `ifHash` → compare-and-swap, `create` → create-exclusive,
   *  neither → unconditional. Conflicts reject with {@link FsConflictError}
   *  whose `hash` is the current value hash (rebase and retry). */
  kvPut(
    key: string,
    value: Uint8Array,
    options: KvPutOptions = {},
  ): Promise<{ hash: bigint; mtimeNs: bigint }> {
    const err = this.kvGuard();
    if (err) return Promise.reject(err);
    let flags = 0;
    let base = 0n;
    if (options.ifHash !== undefined) {
      base = options.ifHash;
    } else if (!options.create) {
      flags |= KV_PUT_NO_CAS;
    }
    if (options.durable) flags |= KV_PUT_DURABLE;
    return new Promise((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingKvPuts);
      this.pendingKvPuts.set(nonce, { resolve, reject });
      this.transport.send(
        buildKvPutMessage({ nonce, flags, base, key, value }),
      );
    });
  }

  /** Delete a key: `ifHash` → delete-iff-unchanged, absent → unconditional
   *  (idempotent on a missing key). */
  kvDelete(key: string, options: { ifHash?: bigint } = {}): Promise<void> {
    const err = this.kvGuard();
    if (err) return Promise.reject(err);
    let flags = KV_PUT_DELETE;
    let base = 0n;
    if (options.ifHash !== undefined) base = options.ifHash;
    else flags |= KV_PUT_NO_CAS;
    return new Promise((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingKvPuts);
      this.pendingKvPuts.set(nonce, {
        resolve: () => resolve(),
        reject,
      });
      this.transport.send(
        buildKvPutMessage({
          nonce,
          flags,
          base,
          key,
          value: new Uint8Array(0),
        }),
      );
    });
  }

  /** Fetch one value; null when the key is absent. */
  kvFetch(key: string): Promise<KvFetchResult | null> {
    const err = this.kvGuard();
    if (err) return Promise.reject(err);
    return new Promise((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingKvFetches);
      this.pendingKvFetches.set(nonce, { resolve, reject });
      this.transport.send(buildKvFetchMessage(nonce, key));
    });
  }

  /** Subscribe to a literal byte prefix (empty = whole store). The handle's
   *  mirror fills from the snapshot and tracks live changes; updates are
   *  acknowledged automatically. Subscriptions do not survive re-establish —
   *  `onClosed` fires and the caller re-`watchKv`s (the fs-family rule). */
  watchKv(
    prefix: string,
    options: KvWatchOptions = {},
  ): Promise<KvWatchHandle> {
    const err = this.kvGuard();
    if (err) return Promise.reject(err);
    return new Promise((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingKvOpens);
      this.pendingKvOpens.set(nonce, { resolve, reject, options });
      this.transport.send(
        buildKvOpenMessage(nonce, 0, options.inlineMax ?? 0, prefix),
      );
    });
  }

  /** Reject pending kv requests and close watches (disconnect or
   *  re-establish; nothing kv survives either). */
  private resetKv(error: Error): void {
    for (const pending of this.pendingKvOpens.values()) pending.reject(error);
    this.pendingKvOpens.clear();
    for (const pending of this.pendingKvPuts.values()) pending.reject(error);
    this.pendingKvPuts.clear();
    for (const pending of this.pendingKvFetches.values()) pending.reject(error);
    this.pendingKvFetches.clear();
    const watches = [...this.kvWatches.values()];
    this.kvWatches.clear();
    for (const watch of watches) watch.options.onClosed?.(error);
  }

  private fsWrite(
    syncId: number,
    consumer: FsSyncConsumer,
    path: string,
    data: Uint8Array,
    options: FsWriteOptions,
  ): Promise<FsWriteResult> {
    return new Promise<FsWriteResult>((resolve, reject) => {
      if (!this.fsSyncs.has(syncId)) {
        reject(connectionError("Sync is closed"));
        return;
      }
      let flags = 0;
      if (options.createParents) flags |= FS_WRITE_MKPARENTS;
      if (options.durable) flags |= FS_WRITE_DURABLE;
      // Precondition: create-exclusive (base 0), CAS (base = ifHash), or —
      // by default or under force — an unconditional overwrite.
      let base = 0n;
      if (options.force) {
        flags |= FS_WRITE_NO_CAS;
      } else if (options.create) {
        base = 0n;
      } else if (options.ifHash !== undefined) {
        base = options.ifHash;
      } else {
        flags |= FS_WRITE_NO_CAS;
      }
      // A delta applies against the exact bytes the CAS precondition
      // names (docs/design/fs-write.md content_kind 2), so it demands a
      // real nonzero-hash anchor: no `force`, no create-exclusive, no
      // unconditional write.
      if (
        options.deltaBase !== undefined &&
        (options.force || options.ifHash === undefined || options.ifHash === 0n)
      ) {
        reject(
          connectionError(
            "deltaBase requires a nonzero ifHash precondition (without force)",
          ),
        );
        return;
      }
      const send = (
        contentKind: number,
        content: Uint8Array,
        onInvalid?: () => void,
      ): void => {
        const nonce = this.nextFsNonce(this.pendingFsWrites);
        this.pendingFsWrites.set(nonce, {
          resolve,
          reject,
          record: { consumer, path },
          onInvalid,
        });
        // Delta ops and full bytes ride the same LZ4 path: the builder
        // compresses `content` regardless of kind.
        this.transport.send(
          buildFsWriteMessage({
            nonce,
            syncId,
            flags,
            base,
            mode: options.mode ?? 0,
            contentKind,
            path,
            content,
          }),
        );
      };
      if (options.deltaBase !== undefined) {
        const ops = encodeFsDelta(options.deltaBase, data);
        // The server's own worthwhile heuristic (crates/fssync/src/lib.rs):
        // send the delta only when clearly smaller than the full content.
        if (ops.length * 8 < data.length * 7) {
          // A pre-delta server answers INVALID for content_kind 2: retry
          // once as a full write with the same precondition, surfacing
          // only the retry's outcome. (CONFLICT is a real CAS failure
          // and never retries.)
          send(FS_WRITE_CONTENT_DELTA, ops, () =>
            send(FS_WRITE_CONTENT_FULL, data),
          );
          return;
        }
      }
      send(FS_WRITE_CONTENT_FULL, data);
    });
  }

  private fsOp(
    syncId: number,
    op: number,
    a: string,
    b: string,
    base: bigint,
    mode: number,
    flags: number,
    record?: { consumer: FsSyncConsumer; path: string },
  ): Promise<FsWriteResult> {
    return new Promise<FsWriteResult>((resolve, reject) => {
      if (!this.fsSyncs.has(syncId)) {
        reject(connectionError("Sync is closed"));
        return;
      }
      const nonce = this.nextFsNonce(this.pendingFsWrites);
      this.pendingFsWrites.set(nonce, { resolve, reject, record });
      this.transport.send(
        buildFsOpMessage({ nonce, syncId, op, flags, base, mode, a, b }),
      );
    });
  }

  /**
   * Open a repository on the server (docs/git.md). Resolves once the
   * server accepts; state snapshots (when watching) apply to the handle's
   * mirror and acknowledge automatically.
   */
  async openRepo(
    path: string,
    options: GitOpenOptions = {},
  ): Promise<GitRepoHandle> {
    if (this.transport.status !== "connected") {
      throw connectionError(
        `Cannot open a repo while transport is ${this.transport.status}`,
      );
    }
    if ((this.features & FEATURE_GIT) === 0) {
      throw connectionError("Server does not support git introspection");
    }
    let flags = 0;
    if (options.watch) flags |= GIT_OPEN_WATCH;
    if (options.status || options.untracked || options.ignored)
      flags |= GIT_OPEN_STATUS;
    if (options.untracked || options.ignored) flags |= GIT_OPEN_UNTRACKED;
    if (options.ignored) flags |= GIT_OPEN_IGNORED;
    if (options.tracking) flags |= GIT_OPEN_TRACKING;
    if (options.remotes) flags |= GIT_OPEN_REMOTES;
    const srcPtyId = this.srcPtyForOpen(options.fromSessionId);
    return new Promise<GitRepoHandle>((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingGitOpens);
      this.pendingGitOpens.set(nonce, { resolve, reject, options });
      this.transport.send(
        msgGitOpen({
          nonce,
          flags,
          refsLatencyMs: options.refsLatencyMs ?? 0,
          statusLatencyMs: options.statusLatencyMs ?? 0,
          srcPtyId,
          parentRepoId: options.parentRepoId,
          refPrefixes: options.refPrefixes,
          path,
        }),
      );
    });
  }

  /**
   * Repositories under `path` (docs/design/git.md `GIT_DISCOVER`): the
   * answer to "what is checked out here" in one call, instead of a ladder
   * of candidate paths probed with an `FS_SYNC` per level.
   *
   * It hangs off the connection rather than a repo handle because it
   * allocates no repo id — an enumeration, not an open — so it cannot
   * exhaust the per-connection repo budget.
   *
   * A capped walk says where it stopped, and this follows that cursor to
   * the end by default: the caller asked what is under a path, not for one
   * page of it. `onPage` sees each page as it lands, for a caller that
   * wants to render progressively; `maxPages` bounds a walk over a tree
   * that is being written to underneath it.
   */
  async discoverRepos(
    path: string,
    options: GitDiscoverOptions = {},
  ): Promise<GitFoundRepo[]> {
    if (this.transport.status !== "connected") {
      throw connectionError(
        `Cannot discover repos while transport is ${this.transport.status}`,
      );
    }
    if ((this.features & FEATURE_GIT) === 0) {
      throw connectionError("Server does not support git introspection");
    }
    let flags = 0;
    if (options.nested) flags |= GIT_DISCOVER_NESTED;
    if (options.bare) flags |= GIT_DISCOVER_BARE;
    const found: GitFoundRepo[] = [];
    let after = "";
    const maxPages = options.maxPages ?? 64;
    for (let page = 0; page < maxPages; page++) {
      const msg = await this.gitCall(
        S2C_GIT_DISCOVER,
        (nonce) =>
          msgGitDiscover({
            nonce,
            flags,
            depth: options.depth ?? 0,
            path,
            after,
          }),
        options.signal,
        "Discover",
      );
      const parsed = parseGitDiscoverResp(msg);
      if (!parsed) throw connectionError("Malformed discover from server");
      const [, status, respFlags, records] = parsed;
      if (status !== GIT_STATUS_OK)
        throw new GitStatusError("Discover", status);
      const pageRepos: GitFoundRepo[] = [];
      let cursor: string | null = null;
      for (const record of gitDiscoverRecords(records)) {
        if (record.kind === "repo") {
          pageRepos.push({
            workdir: record.workdir,
            gitdir: record.gitdir,
            bare: (record.flags & GIT_FOUND_BARE) !== 0,
            linked: (record.flags & GIT_FOUND_LINKED) !== 0,
            submodule: (record.flags & GIT_FOUND_SUBMODULE) !== 0,
          });
        } else {
          cursor = record.after;
        }
      }
      found.push(...pageRepos);
      options.onPage?.(pageRepos);
      // Truncated with no cursor, or one that has not moved, is as far as
      // this walk goes — paging on it again would spin.
      if ((respFlags & GIT_DISCOVER_TRUNCATED) === 0) break;
      if (cursor === null || cursor === after) break;
      after = cursor;
    }
    return found;
  }

  /** One nonce-correlated git request; resolves with the raw response. */
  private gitRequest(
    repoId: number,
    opcode: number,
    build: (nonce: number) => Uint8Array,
    signal?: AbortSignal,
    op = "Request",
  ): Promise<Uint8Array> {
    if (!this.gitRepos.has(repoId)) {
      return Promise.reject(connectionError("Repo is closed"));
    }
    return this.gitCall(opcode, build, signal, op);
  }

  /** A nonce-correlated git request that is not scoped to an open repo —
   *  `GIT_DISCOVER` is the only one, since it enumerates repositories
   *  rather than using one and allocates no repo id. */
  private gitCall(
    opcode: number,
    build: (nonce: number) => Uint8Array,
    signal?: AbortSignal,
    op = "Request",
  ): Promise<Uint8Array> {
    return new Promise<Uint8Array>((resolve, reject) => {
      if (signal?.aborted) {
        reject(new GitStatusError(op, GIT_STATUS_CANCELLED));
        return;
      }
      const nonce = this.nextFsNonce(this.pendingGitRequests);
      const entry = { opcode, resolve, reject };
      this.pendingGitRequests.set(nonce, entry);
      signal?.addEventListener(
        "abort",
        () => {
          const live = this.pendingGitRequests.get(nonce);
          if (!live || live !== entry || live.abandoned) return;
          // Tell the server to stop, hand the caller its rejection now,
          // and keep the nonce reserved as a tombstone until the reply
          // arrives — releasing it early would let the next request reuse
          // an id the server still considers live.
          this.transport.send(msgGitCancel(nonce));
          live.abandoned = true;
          reject(new GitStatusError(op, GIT_STATUS_CANCELLED));
        },
        { once: true },
      );
      this.transport.send(build(nonce));
    });
  }

  /** Serve one oid's bytes from the connection-wide cache, coalescing
   *  concurrent fetches; a hit refreshes LRU recency. */
  private cachedGitBlob(
    cacheKey: string,
    fetch: () => Promise<Uint8Array>,
  ): Promise<Uint8Array> {
    const hit = this.gitBlobCache.get(cacheKey);
    if (hit) {
      this.gitBlobCache.delete(cacheKey);
      this.gitBlobCache.set(cacheKey, hit);
      return hit.promise;
    }
    const entry = { promise: fetch(), bytes: 0, settled: false };
    this.gitBlobCache.set(cacheKey, entry);
    entry.promise.then(
      (data) => {
        entry.settled = true;
        if (this.gitBlobCache.get(cacheKey) !== entry) return;
        entry.bytes = data.byteLength;
        this.gitBlobCacheBytes += entry.bytes;
        this.evictGitBlobs();
      },
      () => {
        // Failures are not cached.
        if (this.gitBlobCache.get(cacheKey) === entry) {
          this.gitBlobCache.delete(cacheKey);
        }
      },
    );
    return entry.promise;
  }

  /** Drop least-recently-used settled blobs until back under budget. */
  private evictGitBlobs(): void {
    if (this.gitBlobCacheBytes <= this.gitBlobCacheBudget) return;
    for (const [key, entry] of this.gitBlobCache) {
      if (!entry.settled) continue; // in-flight entries keep coalescing
      this.gitBlobCache.delete(key);
      this.gitBlobCacheBytes -= entry.bytes;
      if (this.gitBlobCacheBytes <= this.gitBlobCacheBudget) return;
    }
  }

  private makeGitRepoHandle(
    repoId: number,
    info: { oidFormat: number; flags: number; workdir: string; gitdir: string },
    mirror: GitStateMirror,
    notifier: Notifier,
  ): GitRepoHandle {
    const expectOk = (status: number, op = "Request"): void => {
      if (status !== GIT_STATUS_OK) {
        throw new GitStatusError(op, status);
      }
    };
    return {
      repoId,
      oidFormat: info.oidFormat,
      repoFlags: info.flags,
      workdir: info.workdir,
      gitdir: info.gitdir,
      state: mirror,
      subscribe: notifier.subscribe,
      get revision() {
        return notifier.revision;
      },
      log: async (req = {}, opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_COMMITS,
          (nonce) =>
            msgGitLog({
              nonce,
              repoId,
              flags: req.flags ?? 0,
              limit: req.limit ?? 0,
              path: req.path ?? "",
              tips: req.tips ?? [],
              hides: req.hides ?? [],
            }),
          opts.signal,
          "Log",
        );
        const page = parseGitCommits(msg);
        if (!page) throw connectionError("Malformed commits from server");
        expectOk(page.status, "Log");
        return page;
      },
      tree: async (oid, path = "", opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_TREE,
          (nonce) =>
            msgGitTree({ nonce, repoId, oid, path, after: opts.after }),
          opts.signal,
          "Tree",
        );
        const parsed = parseGitTreeResp(msg);
        if (!parsed) throw connectionError("Malformed tree from server");
        expectOk(parsed[1], "Tree");
        return [...gitTreeRecords(parsed[3])];
      },
      blob: (oid, path = "", maxLen = 0, opts = {}) => {
        const fetchBlob = async (): Promise<Uint8Array> => {
          const msg = await this.gitRequest(
            repoId,
            S2C_GIT_BLOB,
            (nonce) =>
              msgGitBlob({
                nonce,
                repoId,
                oid,
                path,
                maxLen,
                offset: opts.offset,
                flags: opts.flags,
              }),
            opts.signal,
            "Blob",
          );
          const parsed = parseGitBlobResp(msg);
          if (!parsed) throw connectionError("Malformed blob from server");
          expectOk(parsed[1], "Blob");
          return parsed[3];
        };
        // Only a whole-object direct oid pull is content-addressed. A `path`
        // resolves through whatever object the oid names, and a window is a
        // slice of the object rather than the object — both bypass the cache,
        // which is keyed by oid alone.
        if (path !== "" || opts.offset) return fetchBlob();
        return this.cachedGitBlob(gitOidHex(oid, info.oidFormat), fetchBlob);
      },
      diff: async (old, newEndpoint, opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_DIFF,
          (nonce) =>
            msgGitDiff({
              nonce,
              repoId,
              flags: opts.flags ?? 0,
              rename: opts.rename,
              old,
              new: newEndpoint,
              path: opts.path ?? "",
              after: opts.after,
            }),
          opts.signal,
          "Diff",
        );
        const parsed = parseGitDiffResp(msg);
        if (!parsed) throw connectionError("Malformed diff from server");
        expectOk(parsed[1], "Diff");
        return [...gitDiffRecords(parsed[3])];
      },
      patch: async (old, newEndpoint, opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_PATCH,
          (nonce) =>
            msgGitPatch({
              nonce,
              repoId,
              flags: opts.flags ?? 0,
              context: opts.context ?? 0,
              rename: opts.rename,
              old,
              new: newEndpoint,
              path: opts.path ?? "",
              maxLen: opts.maxLen ?? 0,
              after: opts.after,
              afterPos: opts.afterPos,
            }),
          opts.signal,
          "Patch",
        );
        const parsed = parseGitPatchResp(msg);
        if (!parsed) throw connectionError("Malformed patch from server");
        expectOk(parsed[1], "Patch");
        const [, , flags, data] = parsed;
        return {
          flags,
          records:
            flags & GIT_PATCH_STRUCTURED ? [...gitPatchRecords(data)] : [],
          text: flags & GIT_PATCH_STRUCTURED ? new Uint8Array(0) : data,
        };
      },
      index: async (path = "", opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_INDEX,
          (nonce) => msgGitIndex({ nonce, repoId, path, after: opts.after }),
          opts.signal,
          "Index",
        );
        const parsed = parseGitIndexResp(msg);
        if (!parsed) throw connectionError("Malformed index from server");
        expectOk(parsed[1], "Index");
        return [...gitIndexRecords(parsed[3])];
      },
      mergeBase: async (oids, opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_BASE,
          (nonce) => msgGitBase(nonce, repoId, oids),
          opts.signal,
          "MergeBase",
        );
        const parsed = parseGitBaseResp(msg);
        if (!parsed) throw connectionError("Malformed base from server");
        expectOk(parsed[1], "MergeBase");
        return parsed[2];
      },
      resolve: async (spec, opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_RESOLVE,
          (nonce) => msgGitResolve(nonce, repoId, spec),
          opts.signal,
          "Resolve",
        );
        const parsed = parseGitResolveResp(msg);
        if (!parsed) throw connectionError("Malformed resolve from server");
        expectOk(parsed.status, "Resolve");
        return { tips: parsed.tips, hides: parsed.hides };
      },
      blame: async (path, opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_BLAME,
          (nonce) =>
            msgGitBlame({
              nonce,
              repoId,
              flags: opts.flags,
              oid: opts.oid ?? GIT_OID_NONE,
              startLine: opts.startLine,
              lineCount: opts.lineCount,
              path,
            }),
          opts.signal,
          "Blame",
        );
        const parsed = parseGitBlameResp(msg);
        if (!parsed) throw connectionError("Malformed blame from server");
        expectOk(parsed[1], "Blame");
        return [...gitBlameRecords(parsed[3])];
      },
      reflog: async (refName = "", opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_REFLOG,
          (nonce) =>
            msgGitReflog({
              nonce,
              repoId,
              flags: opts.flags,
              limit: opts.limit,
              refName,
              afterPos: opts.afterPos,
            }),
          opts.signal,
          "Reflog",
        );
        const parsed = parseGitReflogResp(msg);
        if (!parsed) throw connectionError("Malformed reflog from server");
        expectOk(parsed[1], "Reflog");
        return [...gitReflogRecords(parsed[3])];
      },
      fetch: async (opts = {}) => {
        const msg = await this.gitRequest(
          repoId,
          S2C_GIT_FETCH,
          (nonce) =>
            msgGitFetch({
              nonce,
              repoId,
              flags: opts.flags,
              timeoutMs: opts.timeoutMs,
              remote: opts.remote,
              refspecs: opts.refspecs,
            }),
          opts.signal,
          "Fetch",
        );
        const parsed = parseGitFetchResp(msg);
        if (!parsed) throw connectionError("Malformed fetch from server");
        expectOk(parsed[1], "Fetch");
        return [...gitFetchRecords(parsed[3])];
      },
      watchLog: (spec, opts, onUpdate) =>
        this.watchGitLog(repoId, spec, opts, onUpdate),
      close: () => {
        this.closeGitLogSubs(repoId);
        if (this.transport.status === "connected") {
          this.transport.send(msgGitClose(repoId));
        }
      },
    };
  }

  /** Start a live log subscription; the server pushes pages we auto-ack. */
  private watchGitLog(
    repoId: number,
    spec: string,
    opts: GitLogWatchOptions,
    onUpdate: (page: GitLogPage) => void,
  ): GitLogSubscription {
    if (!this.gitRepos.has(repoId)) {
      throw connectionError("Repo is closed");
    }
    let logId = 0;
    do {
      logId = this.gitLogIdCounter = (this.gitLogIdCounter + 1) & 0xffff;
    } while (logId === 0 || this.gitLogSubs.has(logId));
    this.gitLogSubs.set(logId, { repoId, onUpdate });
    this.transport.send(
      msgGitLogWatch(logId, repoId, opts.flags ?? 0, opts.limit ?? 0, spec),
    );
    return {
      logId,
      close: () => {
        if (!this.gitLogSubs.delete(logId)) return;
        if (this.transport.status === "connected") {
          this.transport.send(msgGitLogUnwatch(logId, repoId));
        }
      },
    };
  }

  /** Drop every log subscription bound to a repo (close or teardown). */
  private closeGitLogSubs(repoId: number): void {
    for (const [logId, sub] of this.gitLogSubs) {
      if (sub.repoId === repoId) this.gitLogSubs.delete(logId);
    }
  }

  /** Tear down all git repo state (reconnect or dispose). */
  private resetGitRepos(error: Error): void {
    for (const pending of this.pendingGitOpens.values()) {
      pending.reject(error);
    }
    this.pendingGitOpens.clear();
    for (const pending of this.pendingGitRequests.values()) {
      pending.reject(error);
    }
    this.pendingGitRequests.clear();
    this.gitLogSubs.clear();
    const repos = [...this.gitRepos.values()];
    this.gitRepos.clear();
    for (const repo of repos) {
      repo.options.onClosed?.(GIT_CLOSED_CONNECTION_LOST);
      repo.notifier.emit();
    }
  }

  /**
   * Attach to the workspace containing a path (docs/design/lsp.md).
   * Resolves once the server accepts; state snapshots and diagnostics
   * (when subscribed) apply to the handle's mirrors and acknowledge
   * automatically.
   */
  async openLsp(
    path: string,
    options: LspOpenOptions = {},
  ): Promise<LspHandle> {
    if (this.transport.status !== "connected") {
      throw connectionError(
        `Cannot open an attachment while transport is ${this.transport.status}`,
      );
    }
    if ((this.features & FEATURE_LSP) === 0) {
      throw connectionError(
        "Server does not support language intelligence (upgrade blit on the remote)",
      );
    }
    let flags = 0;
    if (options.watch || options.diagnostics) flags |= LSP_OPEN_WATCH;
    if (options.diagnostics) flags |= LSP_OPEN_DIAGS;
    const srcPtyId = this.srcPtyForOpen(options.fromSessionId);
    return new Promise<LspHandle>((resolve, reject) => {
      const nonce = this.nextFsNonce(this.pendingLspOpens);
      this.pendingLspOpens.set(nonce, { resolve, reject, options });
      this.transport.send(
        msgLspOpen(nonce, flags, options.diagLatencyMs ?? 0, path, srcPtyId),
      );
    });
  }

  /** One nonce-correlated LSP query; resolves with the raw response. */
  private lspRequest(
    lspId: number,
    build: (nonce: number) => Uint8Array,
  ): Promise<Uint8Array> {
    return new Promise<Uint8Array>((resolve, reject) => {
      if (!this.lspAttachments.has(lspId)) {
        reject(connectionError("Attachment is closed"));
        return;
      }
      const nonce = this.nextFsNonce(this.pendingLspRequests);
      this.pendingLspRequests.set(nonce, { resolve, reject });
      this.transport.send(build(nonce));
    });
  }

  private makeLspHandle(
    lspId: number,
    root: string,
    state: LspStateMirror,
    diags: LspDiagMirror,
    notifier: Notifier,
  ): LspHandle {
    // Every query funnels through one shape. Non-OK statuses resolve so
    // callers can inspect `status` (WARMING is retryable); only
    // connection loss rejects.
    const query = async (
      kind: number,
      flags: number,
      line: number,
      col: number,
      path: string,
      arg: string,
    ): Promise<LspQueryResult> => {
      const msg = await this.lspRequest(lspId, (nonce) =>
        msgLspQuery({ nonce, lspId, kind, flags, line, col, path, arg }),
      );
      const parsed = parseLspQueryResp(msg);
      if (!parsed)
        throw connectionError("Malformed query response from server");
      const [, status, respFlags, detail, records] = parsed;
      return {
        status,
        detail,
        truncated: (respFlags & LSP_RESP_TRUNCATED) !== 0,
        incomplete: (respFlags & LSP_RESP_INCOMPLETE) !== 0,
        records: [...lspQueryRecords(records)],
      };
    };
    return {
      lspId,
      root,
      state,
      diags,
      subscribe: notifier.subscribe,
      get revision() {
        return notifier.revision;
      },
      definition: (path, line, col) =>
        query(LSP_QUERY_DEFINITION, 0, line, col, path, ""),
      references: (path, line, col, includeDeclaration = false) =>
        query(
          LSP_QUERY_REFERENCES,
          includeDeclaration ? LSP_REFS_INCLUDE_DECLARATION : 0,
          line,
          col,
          path,
          "",
        ),
      hover: (path, line, col) =>
        query(LSP_QUERY_HOVER, 0, line, col, path, ""),
      documentSymbols: (path) =>
        query(LSP_QUERY_DOC_SYMBOLS, 0, 0, 0, path, ""),
      workspaceSymbols: (search) =>
        query(LSP_QUERY_WS_SYMBOLS, 0, 0, 0, "", search),
      rename: (path, line, col, newName) =>
        query(LSP_QUERY_RENAME, 0, line, col, path, newName),
      completion: (path, line, col) =>
        query(LSP_QUERY_COMPLETION, 0, line, col, path, ""),
      signatureHelp: (path, line, col) =>
        query(LSP_QUERY_SIGNATURE, 0, line, col, path, ""),
      // Fire-and-forget overlay writes (docs/design/lsp.md
      // "LSP_BUFFER"): transport ordering, not acknowledgment, is what
      // queries rely on, so these send-and-return like input.
      buffer: (path, text) => {
        if (
          this.transport.status === "connected" &&
          this.lspAttachments.has(lspId)
        ) {
          this.transport.send(msgLspBuffer(lspId, 0, path, text));
        }
      },
      releaseBuffer: (path) => {
        if (
          this.transport.status === "connected" &&
          this.lspAttachments.has(lspId)
        ) {
          this.transport.send(
            msgLspBuffer(lspId, LSP_BUFFER_RELEASE, path, new Uint8Array()),
          );
        }
      },
      close: () => {
        if (this.transport.status === "connected") {
          this.transport.send(msgLspClose(lspId));
        }
      },
    };
  }

  /** Tear down all LSP attachment state (reconnect or dispose). */
  private resetLspAttachments(error: Error): void {
    for (const pending of this.pendingLspOpens.values()) {
      pending.reject(error);
    }
    this.pendingLspOpens.clear();
    for (const pending of this.pendingLspRequests.values()) {
      pending.reject(error);
    }
    this.pendingLspRequests.clear();
    const attachments = [...this.lspAttachments.values()];
    this.lspAttachments.clear();
    for (const attachment of attachments) {
      attachment.options.onClosed?.(LSP_CLOSED_CONNECTION_LOST);
      attachment.notifier.emit();
    }
  }

  /** Tear down all fs sync state (reconnect or dispose). */
  private resetFsSyncs(error: Error): void {
    for (const pending of this.pendingFsSyncs.values()) {
      for (const waiter of pending.waiters) waiter.reject(error);
    }
    this.pendingFsSyncs.clear();
    this.pendingFsSyncsByKey.clear();
    for (const pending of this.pendingFsFetches.values()) {
      pending.reject(error);
    }
    this.pendingFsFetches.clear();
    for (const pending of this.pendingFsSearches.values()) {
      pending.reject(error);
    }
    this.pendingFsSearches.clear();
    for (const pending of this.pendingFsGreps.values()) {
      pending.reject(error);
    }
    this.pendingFsGreps.clear();
    for (const pending of this.pendingFsIndexes.values()) {
      pending.reject(error);
    }
    this.pendingFsIndexes.clear();
    for (const pending of this.pendingCwds.values()) {
      pending.reject(error);
    }
    this.pendingCwds.clear();
    for (const pending of this.pendingFsWrites.values()) {
      pending.reject(error);
    }
    this.pendingFsWrites.clear();
    const shares = [...this.fsSyncs.values()];
    this.fsSyncs.clear();
    this.fsSyncsByKey.clear();
    for (const share of shares) {
      for (const consumer of share.consumers) {
        this.dispatchFs(
          consumer,
          () => {
            consumer.options.onClosed?.(FS_CLOSED_CONNECTION_LOST);
            consumer.notifier.emit();
          },
          false,
        );
      }
      share.consumers.clear();
    }
  }

  private ptyId(sessionId: SessionId): number | undefined {
    return this.sessionsById.get(sessionId)?.ptyId;
  }

  /**
   * The pty a `fromSessionId` open resolves its root from (fs/git/lsp
   * FROM_PTY, docs/ide.md Decision 3), or `undefined` for a plain
   * path-based open.
   *
   * A caller that asked to follow a terminal must never silently get a
   * path-based open instead: those opens carry a *pty-relative* path (the
   * dock's follow-terminal root is `""`), so dropping FROM_PTY rebases them
   * onto the server's own cwd — and for git, `open("")` is refused outright,
   * which left the commit log loading forever. SessionIds are minted fresh on
   * every re-establish and superseded ones are pruned, so an unresolvable id
   * means the caller is holding one from a past generation: fail loudly.
   */
  private srcPtyForOpen(sessionId: SessionId | undefined): number | undefined {
    if (!sessionId) return undefined;
    const ptyId = this.ptyId(sessionId);
    if (ptyId === undefined) {
      throw connectionError(
        `Source terminal is gone: session ${sessionId} is no longer known`,
      );
    }
    return ptyId;
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

  addDirtyListener(sessionId: SessionId, listener: () => void): () => void {
    const id = this.ptyId(sessionId);
    if (id == null) return () => {};
    return this.store.addDirtyListener((dirtyId) => {
      if (dirtyId === id) listener();
    });
  }

  /**
   * Called when the server re-anchors this session's scrolled-back view
   * (`S2C_SCROLL_OFFSET`) with the offset it now holds for us.
   *
   * Separate from the dirty listener because it isn't a frame: the content
   * hasn't been decided yet, only where in the scrollback it will be read
   * from.
   */
  addScrollAnchorListener(
    sessionId: SessionId,
    listener: (offset: number) => void,
  ): () => void {
    const id = this.ptyId(sessionId);
    if (id == null) return () => {};
    const entry = { ptyId: id, listener };
    this.scrollAnchorListeners.add(entry);
    return () => {
      this.scrollAnchorListeners.delete(entry);
    };
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

  /** Report the composition in progress; empty text withdraws it. */
  sendSurfacePreedit(
    surfaceId: number,
    text: string,
    cursorUtf16: number,
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(
      buildSurfacePreeditMessage(surfaceId, text, cursorUtf16),
    );
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

  sendSurfaceAxis2(surfaceId: number, ev: SurfaceAxisEvent): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceAxis2Message(surfaceId, ev));
  }

  /** Returns whether the message went out.  Unlike the input sends, a
   *  dropped resize is not water under the bridge: the caller records it
   *  as the size the server knows about, so silently swallowing one on a
   *  disconnected transport leaves the surface stuck at the previous size
   *  until the pane happens to change size again. */
  private sendSurfaceResize(
    surfaceId: number,
    width: number,
    height: number,
    scale120: number = 0,
  ): boolean {
    if (this.transport.status !== "connected") return false;
    this.transport.send(
      buildSurfaceResizeMessage(surfaceId, width, height, scale120),
    );
    return true;
  }

  // Per-view resize constraints.  The wire message is one size per
  // (client, surface) — it carries no view id, and 0×0 means "unset".
  // Two live views of the same surface therefore share one slot, and
  // during a handoff (BSP pane ⇄ workspace foreground view) the old
  // view's teardown used to fire the unset *after* the new view had
  // already sent its size, wiping it; the survivor's own dedup then
  // kept it from ever re-offering, so the surface stayed unsized until
  // its box happened to change.  Mediate here instead, exactly like
  // {@link effectiveSurfaceTarget} does for subscribe targets: views
  // offer and withdraw their sizes, the most recent offer wins, and the
  // unset only goes out when no sized view remains.
  private surfaceViewSizes = new Map<
    number,
    {
      /** Insertion-ordered; the most recent offer is the effective size. */
      views: Map<string, { width: number; height: number; scale120: number }>;
      /** Last size actually sent on the wire, null when nothing (or the
       *  unset) is what the server currently knows. */
      lastSent: { width: number; height: number; scale120: number } | null;
    }
  >();

  /** Offer one view's size for a surface.  Returns whether the server now
   *  knows the effective size (sent, or already current) — false means the
   *  transport was down and the caller should retry. */
  offerSurfaceViewSize(
    surfaceId: number,
    viewId: string,
    width: number,
    height: number,
    scale120: number = 0,
  ): boolean {
    let entry = this.surfaceViewSizes.get(surfaceId);
    if (!entry) {
      entry = { views: new Map(), lastSent: null };
      this.surfaceViewSizes.set(surfaceId, entry);
    }
    // Re-insert so a repeat offer moves to the end: latest writer wins.
    entry.views.delete(viewId);
    entry.views.set(viewId, { width, height, scale120 });
    return this.flushSurfaceViewSize(surfaceId, entry);
  }

  /** Withdraw one view's size.  Re-sends the surviving latest offer if the
   *  withdrawn view's was the one on the wire, or the unset when it was the
   *  last sized view. */
  withdrawSurfaceViewSize(surfaceId: number, viewId: string): void {
    const entry = this.surfaceViewSizes.get(surfaceId);
    if (!entry || !entry.views.delete(viewId)) return;
    if (entry.views.size === 0) {
      if (
        entry.lastSent !== null &&
        this.sendSurfaceResize(surfaceId, 0, 0, 0)
      ) {
        entry.lastSent = null;
      }
      // Keep the entry only while it still says something the map's
      // absence wouldn't: a lastSent the reconnect path must clear.
      if (entry.lastSent === null) this.surfaceViewSizes.delete(surfaceId);
      return;
    }
    this.flushSurfaceViewSize(surfaceId, entry);
  }

  private flushSurfaceViewSize(
    surfaceId: number,
    entry: NonNullable<ReturnType<BlitConnection["surfaceViewSizes"]["get"]>>,
  ): boolean {
    let effective: { width: number; height: number; scale120: number } | null =
      null;
    for (const size of entry.views.values()) effective = size;
    if (!effective) return true;
    if (
      entry.lastSent !== null &&
      entry.lastSent.width === effective.width &&
      entry.lastSent.height === effective.height &&
      entry.lastSent.scale120 === effective.scale120
    ) {
      return true;
    }
    if (
      !this.sendSurfaceResize(
        surfaceId,
        effective.width,
        effective.height,
        effective.scale120,
      )
    ) {
      return false;
    }
    entry.lastSent = { ...effective };
    return true;
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
  //   * bandwidth / speed: per-surface overrides (set by
  //     sendSurfaceResubscribe) falling back to defaultSurfaceBandwidth
  //     and defaultSurfaceSpeed.
  // Each `BlitSurfaceCanvas` (or equivalent caller) allocates its own
  // token via allocSurfaceViewId() and owns the subscribe/unsubscribe
  // lifecycle for it.
  /** Active surface subscriptions keyed by surface id. */
  private surfaceSubs = new Map<number, SurfaceSub>();
  private surfaceViewIdCounter = 0;

  /** Allocate a token identifying one view's subscription to a surface.
   *  Mirrors {@link allocViewId} for PTYs. */
  allocSurfaceViewId(): string {
    return `s${++this.surfaceViewIdCounter}`;
  }

  /**
   * The subscribe this connection should be asking for, given every live
   * view of the surface.
   *
   * A view that wants the surface unscaled wins outright: it needs pixels
   * nobody can reconstruct from a downscale, and the thumbnail sharing the
   * stream can always shrink what it is given.  Otherwise the largest
   * request wins, for the same reason in miniature — downscaling further is
   * cheap, upscaling is lossy.
   */
  private effectiveSurfaceTarget(sub: SurfaceSub): SurfaceTarget | null {
    let width = 0;
    let height = 0;
    for (const target of sub.views.values()) {
      if (!target) return null;
      width = Math.max(width, target.width);
      height = Math.max(height, target.height);
    }
    return width > 0 && height > 0 ? { width, height } : null;
  }

  /** Grace window before a refCount=0 subscription's wire UNSUB fires.
   *  Chosen to comfortably cover typical Solid re-render ordering where
   *  the old mount's `onCleanup` fires before the new mount's
   *  `onMount`, but keeps dropped-stream latency tight if the user
   *  really did stop watching. */
  private static readonly SUB_UNSUB_GRACE_MS = 250;

  /** Cancel any pending deferred unsubscribe timers and reset
   *  `lastSent` so the next refresh fires a wire subscribe.
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
      sub.lastSent = null;
    }
    // The new server session knows no view sizes; without this the
    // canvases' resendDisplaySize would dedup against a size only the
    // old session ever heard.
    for (const entry of this.surfaceViewSizes.values()) {
      entry.lastSent = null;
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
    this.surfaceViewSizes.clear();
  }

  private maybeSendSurfaceSubscribe(sub: SurfaceSub): void {
    if (this.transport.status !== "connected") return;
    if (!this.surfaceStreamingEnabled) return;
    const bandwidth = sub.bandwidthOverride ?? this.defaultSurfaceBandwidth;
    const speed = sub.speedOverride ?? this.defaultSurfaceSpeed;
    const target = this.effectiveSurfaceTarget(sub);
    const width = target?.width ?? 0;
    const height = target?.height ?? 0;
    if (
      sub.lastSent !== null &&
      sub.lastSent.bandwidth === bandwidth &&
      sub.lastSent.speed === speed &&
      sub.lastSent.width === width &&
      sub.lastSent.height === height
    ) {
      return;
    }
    sub.lastSent = { bandwidth, speed, width, height };
    this._logger.info(
      `surface sub ${this.id}:${sub.surfaceId}${target ? ` @${width}x${height}` : ""}`,
    );
    this.transport.send(
      buildSurfaceSubscribeMessage(
        sub.surfaceId,
        0,
        bandwidth,
        speed,
        width,
        height,
      ),
    );
  }

  /**
   * Subscribe one view to a surface's frames.  A single wire subscription
   * exists per (connection, surface); additional views share it and the
   * effective request is derived across them.
   *
   * `viewId` comes from {@link allocSurfaceViewId} and identifies this view
   * for the lifetime of its mount.  `target` asks the server to encode a
   * fixed-size downscale for this client instead of sizing the surface to
   * fit — pass null to watch the surface at its mediated size.
   */
  sendSurfaceSubscribe(
    surfaceId: number,
    viewId: string,
    target: SurfaceTarget | null = null,
  ): void {
    let sub = this.surfaceSubs.get(surfaceId);
    if (!sub) {
      sub = {
        surfaceId,
        views: new Map(),
        bandwidthOverride: null,
        speedOverride: null,
        lastSent: null,
        pendingUnsub: null,
      };
      this.surfaceSubs.set(surfaceId, sub);
    } else if (sub.pendingUnsub !== null) {
      // Cancel any pending deferred UNSUB — the new mount wants the
      // live stream and the server's encoder is still valid.
      clearTimeout(sub.pendingUnsub);
      sub.pendingUnsub = null;
    }
    sub.views.set(viewId, target);
    this.maybeSendSurfaceSubscribe(sub);
  }

  /** Update the fixed encode size one view wants, re-deriving the wire
   *  request.  No-op for a view that is not subscribed. */
  setSurfaceViewTarget(
    surfaceId: number,
    viewId: string,
    target: SurfaceTarget | null,
  ): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub || !sub.views.has(viewId)) return;
    const previous = sub.views.get(viewId);
    if (
      previous?.width === target?.width &&
      previous?.height === target?.height
    ) {
      return;
    }
    sub.views.set(viewId, target);
    this.maybeSendSurfaceSubscribe(sub);
  }

  /** Resend the wire subscribe without bumping the ref-count.  Used
   *  after reconnect, where the server lost its subscription table but
   *  the client still has all its mounts active — bumping the count
   *  would leak references. */
  refreshSurfaceSubscribe(surfaceId: number): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub) return;
    sub.lastSent = null;
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
      sub.lastSent = null;
      this.maybeSendSurfaceSubscribe(sub);
    }
  }

  /** Pending probation per withdrawn bit set: the timer that will re-offer
   *  it, and how long this round's ban is (kept after the timer fires so
   *  the next demotion of the same bits doubles it). */
  private codecProbation = new Map<
    number,
    { timer: ReturnType<typeof setTimeout> | null; ms: number }
  >();

  /** First ban after a demotion, and the point past which bans stop
   *  expiring.  A demotion is three decode failures deep — enough to get
   *  the session unstuck, not enough to conclude the codec is broken — so
   *  the first one is short.  Each repeat doubles it, and a codec that
   *  fails again after every reprieve has earned the permanent one. */
  private static readonly CODEC_PROBATION_MS = 60_000;
  private static readonly CODEC_PROBATION_MAX_MS = 8 * 60_000;

  /** Arrange for demoted codec bits to be offered again after a ban. */
  private scheduleCodecProbation(bits: number): void {
    const prev = this.codecProbation.get(bits);
    if (prev?.timer !== null && prev?.timer !== undefined) {
      clearTimeout(prev.timer);
    }
    const ms = prev ? prev.ms * 2 : BlitConnection.CODEC_PROBATION_MS;
    if (ms > BlitConnection.CODEC_PROBATION_MAX_MS) {
      this.codecProbation.set(bits, { timer: null, ms });
      return;
    }
    const timer = setTimeout(() => {
      this.codecProbation.set(bits, { timer: null, ms });
      const mask = restoreCodecSupport(bits);
      if (mask === null) return;
      this._logger.info(
        `codec support 0x${bits.toString(16)} off probation ` +
          `(now 0x${mask.toString(16)})`,
      );
      // Advertised, but deliberately not re-subscribed: every resubscribe
      // costs the server an encoder rebuild and this client a keyframe on
      // every surface at once, and unlike a demotion nothing is broken
      // right now.  The next rebuild — a resize, a remount, a reconnect —
      // picks the restored codec up.
      if (this.transport.status === "connected") this.sendClientFeatures(mask);
    }, ms);
    this.codecProbation.set(bits, { timer, ms });
  }

  sendSurfaceUnsubscribe(surfaceId: number, viewId: string): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub) return;
    if (!sub.views.delete(viewId)) return;
    if (sub.views.size > 0) {
      // Somebody else is still watching, but the view that left may have
      // been the one holding the stream at full size.
      this.maybeSendSurfaceSubscribe(sub);
      return;
    }
    // Defer the wire UNSUB so a remount within the grace window
    // (typical when moving a surface between UI locations, e.g.
    // BSP ↔ side-panel preview) finds the server-side encoder still
    // alive and can resume without a full re-init + keyframe wait.
    if (sub.pendingUnsub !== null) clearTimeout(sub.pendingUnsub);
    sub.pendingUnsub = setTimeout(() => {
      const cur = this.surfaceSubs.get(surfaceId);
      if (!cur || cur.views.size > 0 || cur.pendingUnsub === null) return;
      cur.pendingUnsub = null;
      if (this.transport.status === "connected") {
        this._logger.info(`surface unsub ${this.id}:${surfaceId}`);
        this.transport.send(buildSurfaceUnsubscribeMessage(surfaceId));
      }
      this.surfaceSubs.delete(surfaceId);
    }, BlitConnection.SUB_UNSUB_GRACE_MS);
  }

  /** Set per-surface bandwidth and speed overrides and re-send the
   *  subscribe.  The server treats a second SURFACE_SUBSCRIBE at the
   *  same sid as a codec/bandwidth/speed update.  No-op when the sid is
   *  unknown. */
  sendSurfaceResubscribe(
    surfaceId: number,
    bandwidth: number,
    speed: number,
  ): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub) return;
    sub.bandwidthOverride = bandwidth;
    sub.speedOverride = speed;
    sub.lastSent = null;
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
        sub.lastSent = null;
        this.maybeSendSurfaceSubscribe(sub);
      }
    } else {
      for (const sub of this.surfaceSubs.values()) {
        this.transport.send(buildSurfaceUnsubscribeMessage(sub.surfaceId));
        sub.lastSent = null;
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
   * Take ownership of PRIMARY, the selection a middle click pastes.
   *
   * The compositor serves these bytes itself, so send them only when the
   * user actually asks to paste — see {@link buildPrimaryMessage}.
   */
  sendPrimary(mimeType: string, data: Uint8Array): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildPrimaryMessage(mimeType, data));
  }

  /**
   * Advertise client capabilities to the server: which video codecs this
   * browser decodes, so the server picks a compatible encoder, and the
   * largest frame it decodes, so the server knows whether it may composite
   * a surface above the H.264 ceiling for us.  Called automatically when
   * the connection is established and codec probing completes.
   */
  sendClientFeatures(codecSupport: number): void {
    if (this.transport.status !== "connected") return;
    const [maxW, maxH] = getMaxDecodeSize();
    this.transport.send(buildClientFeaturesMessage(codecSupport, maxW, maxH));
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

  private noteServerResponsive(emit = false): void {
    // Some older blit-server builds do not send S2C_READY, but they still send
    // S2C_LIST. Wait for that list before showing a remote as connected: HELLO,
    // pings, or surface frames prove the upstream socket is responsive, but not
    // that terminal state has arrived.
    if (
      this.transport.status !== "connected" ||
      !this.hasReceivedList ||
      this.snapshot.status === "connected"
    ) {
      return;
    }
    this.snapshot = {
      ...this.snapshot,
      status: "connected",
    };
    if (emit) this.emit();
  }

  /** Drop any half-received fragment sequence (reconnect or dispose) so it
   *  cannot bleed into the first fragmented message on the next connection. */
  private resetFragmentReassembly(): void {
    this.fragmentChunks = [];
    this.fragmentBytes = 0;
  }

  private handleMessage = (data: ArrayBuffer): void => {
    const bytes = new Uint8Array(data);
    if (bytes.length === 0) return;

    const type = bytes[0];
    // Fragment reassembly is handled before the normal dispatch so the
    // reconstituted buffer flows through the same switch as any other
    // message — callers don't need to know their message was chunked.
    if (type === S2C_FRAGMENT) {
      if (bytes.length < 2) return;
      const flags = bytes[1];
      const chunk = bytes.subarray(2);
      // A complete message can never exceed the protocol-wide decompressed
      // ceiling, so a fragment stream that grows past it is a buggy or
      // hostile peer — drop the partial rather than reassemble without
      // bound. Without this a peer that never sets FRAGMENT_FLAG_LAST grows
      // the buffer until the tab dies, and each chunk is a subarray that
      // pins the whole frame it arrived in. The Rust reader has always had
      // this guard; the browser did not.
      if (this.fragmentBytes + chunk.length > FS_MAX_DECOMPRESSED) {
        this.resetFragmentReassembly();
        return;
      }
      this.fragmentChunks.push(chunk);
      this.fragmentBytes += chunk.length;
      if (flags & FRAGMENT_FLAG_LAST) {
        const reassembled = new Uint8Array(this.fragmentBytes);
        let offset = 0;
        for (const c of this.fragmentChunks) {
          reassembled.set(c, offset);
          offset += c.length;
        }
        this.fragmentChunks = [];
        this.fragmentBytes = 0;
        this.handleMessage(reassembled.buffer);
      }
      return;
    }

    if (type !== S2C_QUIT && type !== S2C_HELLO && type !== S2C_READY) {
      this.noteServerResponsive(true);
    }

    switch (type) {
      case S2C_PING:
        // Application-level keepalive — no action needed.
        return;
      case S2C_QUIT:
        // Server is shutting down.  Immediately dismiss all sessions and
        // surfaces so the UI doesn't show stale windows while reconnecting.
        // This mirrors the S2C_HELLO reset path but happens *before* the
        // transport drops, so the UI clears instantly.
        //
        // Flip ready=false *before* wiping the surface store so consumers
        // (e.g. the BSP reconciler) observe the "not ready" state at the
        // moment surfaces drop out of the live list.  Otherwise a
        // reactive flush driven by surfaceStore.reset() can race ahead
        // and wipe surface pane assignments because the connection still
        // looks ready.
        for (const session of this.sessions) {
          if (session.state !== "closed") {
            this.markSessionClosed(session.id, false);
          }
        }
        this.hasReceivedList = false;
        this.snapshot = {
          ...this.snapshot,
          // Reset generation: fs/git/lsp are torn down below.
          generation: ++this.generation,
          status:
            this.transport.status === "connected"
              ? "authenticating"
              : this.snapshot.status,
          ready: false,
          sessions: this.publicSessions,
          focusedSessionId: null,
        };
        this.emit();
        this.surfaceStore.reset();
        this.audioPlayer.reset();
        this.resetSurfaceSubsForReconnect();
        this.resetFsSyncs(connectionError("Server is shutting down"));
        this.resetGitRepos(connectionError("Server is shutting down"));
        this.resetLspAttachments(connectionError("Server is shutting down"));
        this.resetKv(connectionError("Server is shutting down"));
        this.resetFragmentReassembly();
        this.termCwds.clear();
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
      case S2C_CREATE_FAILED: {
        if (bytes.length < 4) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const pending = this.pendingCreates.get(nonce);
        if (!pending) return;
        this.pendingCreates.delete(nonce);
        const detail = textDecoder.decode(bytes.subarray(4));
        pending.reject(
          connectionError(
            `Create failed: ${statusText(bytes[3])}${detail ? `: ${detail}` : ""}`,
          ),
        );
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
          // Wire: [0x08][pty_id:2][exit_status:4] (i32 LE). Older servers
          // may omit the status; default to EXIT_STATUS_UNKNOWN.
          const exitStatus =
            bytes.length >= 7
              ? new DataView(bytes.buffer, bytes.byteOffset + 3, 4).getInt32(
                  0,
                  true,
                )
              : EXIT_STATUS_UNKNOWN;
          this.updateSession(sessionId, { state: "exited", exitStatus });
        }
        return;
      }
      case S2C_LIST: {
        if (this.handleListMessage(bytes)) {
          this.hasReceivedList = true;
          this.noteServerResponsive(true);
        }
        return;
      }
      case S2C_READY: {
        // S2C_READY is the last message in the server's initial
        // handshake sequence (after S2C_SURFACE_CREATED and S2C_LIST).
        // Setting `ready` here instead of in S2C_LIST ensures the
        // surface store is already populated when the BSP reconciliation
        // runs, preventing surface assignments from being wiped.
        //
        // Also promote the snapshot status to "connected" once a LIST has
        // arrived — until then it is held at "authenticating" because the
        // transport being open (or HELLO arriving) does not mean terminal
        // state is available for rendering.
        if (!this.snapshot.ready || this.snapshot.status !== "connected") {
          this.snapshot = {
            ...this.snapshot,
            ready: true,
            status:
              this.transport.status === "connected" && this.hasReceivedList
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
      case S2C_USED_ROWS: {
        if (bytes.length < 5) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        const usedRows = bytes[3] | (bytes[4] << 8);
        const sessionId = this.currentSessionIdByPtyId.get(ptyId);
        if (sessionId) {
          this.updateSession(sessionId, { usedRows });
        }
        return;
      }
      case S2C_SCROLL_OFFSET: {
        if (bytes.length < 7) return;
        const ptyId = bytes[1] | (bytes[2] << 8);
        const offset =
          (bytes[3] | (bytes[4] << 8) | (bytes[5] << 16) | (bytes[6] << 24)) >>>
          0;
        for (const entry of this.scrollAnchorListeners) {
          if (entry.ptyId === ptyId) entry.listener(offset);
        }
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
        const bootGeneration =
          bytes.length >= 15
            ? new DataView(bytes.buffer, bytes.byteOffset + 7, 8).getBigUint64(
                0,
                true,
              )
            : null;
        // The server's release string was appended after the boot generation,
        // also without a protocol bump: `[len:2][utf8:N]`.
        let serverVersion: string | null = null;
        if (bytes.length >= 17) {
          const verLen = bytes[15] | (bytes[16] << 8);
          if (bytes.length >= 17 + verLen) {
            serverVersion =
              verLen === 0
                ? null
                : new TextDecoder().decode(bytes.subarray(17, 17 + verLen));
          }
        }
        if (version > PROTOCOL_VERSION) {
          this.transport.close();
          return;
        }
        this.features = features;
        this.hasReceivedList = false;
        // S2C_HELLO is the first message on every new server connection.
        // Reset all surfaces and close stale sessions — the server's
        // initial message sequence (S2C_SURFACE_CREATED, S2C_LIST,
        // S2C_READY) will rebuild both.  S2C_LIST is the point at which
        // terminals are known again, and S2C_READY marks the end of the
        // initial burst.  This also handles transparent gateway reconnects
        // where the transport never went through "disconnected".
        //
        // Flip ready=false *before* wiping the surface store: a
        // reactive flush driven by surfaceStore.reset() would otherwise
        // see the connection still "ready" with an empty surface list
        // and nuke pane surface assignments before we had a chance to
        // mark the connection as reconnecting.
        for (const session of this.sessions) {
          if (session.state !== "closed") {
            this.markSessionClosed(session.id, false);
          }
        }
        this.snapshot = {
          ...this.snapshot,
          // Bump generation: a re-establish resets fs/git/lsp below even while
          // the transport stays "connected", so handle-holding views must
          // re-open (they can't tell from status alone).
          generation: ++this.generation,
          // The upstream server is responsive, but terminals are not known
          // until S2C_LIST arrives. Keep the user-visible state at
          // "authenticating" here so a remote does not appear connected
          // while its terminal list is still missing. S2C_LIST (via
          // noteServerResponsive) or S2C_READY promotes to "connected".
          status:
            this.transport.status === "connected"
              ? "authenticating"
              : this.snapshot.status,
          ready: false,
          supportsRestart: (features & FEATURE_RESTART) !== 0,
          supportsCopyRange: (features & FEATURE_COPY_RANGE) !== 0,
          supportsCompositor: (features & FEATURE_COMPOSITOR) !== 0,
          supportsAudio: (features & FEATURE_AUDIO) !== 0,
          supportsFsSync: (features & FEATURE_FS) !== 0,
          supportsGit: (features & FEATURE_GIT) !== 0,
          supportsLsp: (features & FEATURE_LSP) !== 0,
          supportsKv: (features & FEATURE_KV) !== 0,
          bootGeneration,
          serverVersion,
        };
        this.emit();
        this.surfaceStore.reset();
        this.audioPlayer.reset();
        this.resetSurfaceSubsForReconnect();
        // Fs syncs do not survive a server session change: old sync_ids
        // are meaningless on the new session.
        this.resetFsSyncs(connectionError("Connection re-established"));
        this.resetGitRepos(connectionError("Connection re-established"));
        this.resetLspAttachments(connectionError("Connection re-established"));
        this.resetKv(connectionError("Connection re-established"));
        this.resetFragmentReassembly();
        // Pushed cwds belong to the old server session's ptys.
        this.termCwds.clear();
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
          // Logical size is optional — servers before it simply stop at 7
          // bytes, and 0 tells the store to keep what it has.
          const hasLogical = bytes.length >= 11;
          this.surfaceStore.handleSurfaceResized(
            surfaceId,
            width,
            height,
            hasLogical ? view.getUint16(7, true) : 0,
            hasLogical ? view.getUint16(9, true) : 0,
          );
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
        const totalLines =
          (bytes[5] | (bytes[6] << 8) | (bytes[7] << 16) | (bytes[8] << 24)) >>>
          0;
        const text = textDecoder.decode(bytes.subarray(13));
        const pending = this.pendingReads.get(nonce);
        if (pending) {
          this.pendingReads.delete(nonce);
          pending.resolve({ text, totalLines });
        }
        return;
      }
      case S2C_FS_SYNCED: {
        if (bytes.length < 8) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const pending = this.pendingFsSyncs.get(nonce);
        if (!pending) return;
        this.pendingFsSyncs.delete(nonce);
        if (this.pendingFsSyncsByKey.get(pending.key) === pending) {
          this.pendingFsSyncsByKey.delete(pending.key);
        }
        const syncId = bytes[3] | (bytes[4] << 8);
        const status = bytes[5];
        const detailLen = bytes[6] | (bytes[7] << 8);
        const detail = textDecoder.decode(bytes.subarray(8, 8 + detailLen));
        if (status !== FS_STATUS_OK) {
          // FsOpenError carries the wire status/detail so callers can
          // pick a fallback — e.g. a `single` open refused by an older
          // server (any status other than not-found/permission) falls
          // back to a directory sync.
          const error = new FsOpenError(status, detail);
          for (const waiter of pending.waiters) waiter.reject(error);
          return;
        }
        const share: FsSyncShare = {
          key: pending.key,
          syncId,
          root: detail,
          mirror: new FsMirror(),
          synced: false,
          consumers: new Set<FsSyncConsumer>(),
        };
        this.fsSyncs.set(syncId, share);
        this.fsSyncsByKey.set(pending.key, share);
        for (const waiter of pending.waiters) {
          const consumer: FsSyncConsumer = {
            options: waiter.options,
            notifier: new Notifier(),
            // Held until the opener's continuation has run — a snapshot
            // riding this very chunk is handled before it (`dispatchFs`).
            held: [],
            lastWritten: new Map<string, bigint>(),
          };
          share.consumers.add(consumer);
          waiter.resolve(this.makeFsSyncHandle(share, consumer));
          // A task, not a microtask: the opener sits behind an await chain
          // of unknown length (BlitWorkspace.syncFs adds hops of its own),
          // and only a task is guaranteed to run after all of it.
          setTimeout(() => this.releaseHeldFs(consumer), 0);
        }
        return;
      }
      case S2C_FS_UPDATE: {
        if (bytes.length < 8) return;
        const syncId = bytes[1] | (bytes[2] << 8);
        const share = this.fsSyncs.get(syncId);
        if (!share) return;
        const flags = bytes[7];
        // One decompress + decode serves the mirror, the per-record
        // callbacks, and the echo bookkeeping; skip collection entirely
        // when nobody needs the records.
        let wantRecords = false;
        for (const consumer of share.consumers) {
          if (consumer.options.onRecord || consumer.lastWritten.size > 0) {
            wantRecords = true;
            break;
          }
        }
        const records: FsRecord[] | undefined = wantRecords ? [] : undefined;
        const applied = share.mirror.apply(bytes, records);
        if (applied === null) {
          this._logger.warn(
            `${this.id}: malformed FS_UPDATE for sync ${syncId}`,
          );
          return;
        }
        this.transport.send(buildFsAckMessage(syncId, applied.updateId));
        if (flags & FS_UPDATE_SYNC) share.synced = true;
        if (flags & FS_UPDATE_RESET) {
          for (const consumer of share.consumers) {
            const onReset = consumer.options.onReset;
            if (onReset) this.dispatchFs(consumer, onReset);
          }
        }
        if (records) {
          for (const consumer of share.consumers) {
            const onRecord = consumer.options.onRecord;
            if (!onRecord) continue;
            // One dispatch per update, not per record: a snapshot's worth of
            // records would otherwise be a closure each while held.
            this.dispatchFs(consumer, () => {
              for (const record of records) onRecord(record);
            });
          }
        }
        if (flags & FS_UPDATE_SYNC) {
          for (const consumer of share.consumers) {
            const onSync = consumer.options.onSync;
            if (onSync) this.dispatchFs(consumer, onSync);
          }
        }
        // Staged records leave `live` untouched; only wake subscribers
        // when it actually changed (direct apply or the SYNC swap). The
        // revision must be bumped before onUpdate runs: a signal write
        // inside onUpdate re-runs consumer memos synchronously, and any
        // cache keyed on `handle.revision` must see the new revision then,
        // not the pre-update one.
        if (applied.liveChanged) {
          for (const consumer of share.consumers) {
            this.dispatchFs(consumer, () => {
              consumer.notifier.emit();
              consumer.options.onUpdate?.();
            });
          }
        }
        // Every callback above has had its shot at self-echo suppression;
        // consume matched entries so the maps cannot grow without bound.
        // Per consumer: only the writer's own map holds the hash, so every
        // other consumer saw this upsert as the external change it is.
        if (records) {
          for (const consumer of share.consumers) {
            if (consumer.lastWritten.size === 0) continue;
            for (const record of records) {
              if (
                record.kind === "upsert" &&
                consumer.lastWritten.get(record.path) === record.hash
              ) {
                consumer.lastWritten.delete(record.path);
              }
            }
          }
        }
        return;
      }
      case S2C_FS_FILE: {
        const parsed = parseFsFileMessage(bytes);
        if (!parsed) return;
        const pending = this.pendingFsFetches.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsFetches.delete(parsed.nonce);
        if (parsed.status === FS_FILE_OK) {
          pending.resolve(parsed.data);
        } else {
          pending.reject(
            connectionError(`Fetch failed: ${fsFileStatusText(parsed.status)}`),
          );
        }
        return;
      }
      case S2C_FS_SEARCH: {
        const parsed = parseFsSearchResult(bytes);
        if (!parsed) return;
        const pending = this.pendingFsSearches.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsSearches.delete(parsed.nonce);
        pending.resolve(parsed.paths);
        return;
      }
      case S2C_FS_GREP: {
        const parsed = parseFsGrepResult(bytes);
        if (!parsed) return;
        const pending = this.pendingFsGreps.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsGreps.delete(parsed.nonce);
        if (parsed.status === FS_DONE_OK) {
          pending.resolve({
            files: parsed.files,
            truncated: (parsed.flags & FS_GREP_TRUNCATED) !== 0,
          });
        } else {
          pending.reject(
            connectionError(
              parsed.detail ||
                `Content search failed: ${fsDoneStatusText(parsed.status)}`,
            ),
          );
        }
        return;
      }
      case S2C_FS_INDEX: {
        const parsed = parseFsIndexResult(bytes);
        if (!parsed) return;
        const pending = this.pendingFsIndexes.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsIndexes.delete(parsed.nonce);
        if (parsed.status === FS_DONE_OK) {
          pending.resolve({
            paths: parsed.paths,
            truncated: (parsed.flags & FS_INDEX_TRUNCATED) !== 0,
          });
        } else {
          pending.reject(
            connectionError(
              `File index failed: ${fsDoneStatusText(parsed.status)}`,
            ),
          );
        }
        return;
      }
      case S2C_TERM_CWD: {
        const parsed = parseTermCwdReply(bytes);
        if (!parsed) return;
        const pending = this.pendingCwds.get(parsed.nonce);
        if (!pending) return;
        this.pendingCwds.delete(parsed.nonce);
        pending.resolve(parsed.cwd);
        return;
      }
      case S2C_TERM_CWD_EVENT: {
        const parsed = parseTermCwdEvent(bytes);
        if (!parsed) return;
        const sessionId = this.currentSessionIdByPtyId.get(parsed.ptyId);
        if (!sessionId) return; // unknown pty: ignore
        this.termCwds.set(sessionId, parsed.cwd);
        for (const listener of this.termCwdListeners) {
          listener(sessionId, parsed.cwd);
        }
        return;
      }
      case S2C_FS_DONE: {
        const parsed = parseFsDoneMessage(bytes);
        if (!parsed) return;
        const pending = this.pendingFsWrites.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsWrites.delete(parsed.nonce);
        if (parsed.status === FS_DONE_OK) {
          // Record the hash for self-echo suppression: the writer's own
          // UPSERT echo will carry it, and its model already holds it.
          // Into the issuing consumer only — to every other consumer of
          // the shared sync this write is an external change.
          if (pending.record) {
            pending.record.consumer.lastWritten.set(
              pending.record.path,
              parsed.hash,
            );
          }
          pending.resolve({ hash: parsed.hash, mtimeNs: parsed.mtimeNs });
        } else if (parsed.status === FS_DONE_CONFLICT) {
          pending.reject(new FsConflictError(parsed.hash));
        } else if (parsed.status === FS_DONE_INVALID && pending.onInvalid) {
          // A delta write refused by a pre-delta server: re-send once as
          // a full write; the promise settles with the retry's outcome.
          pending.onInvalid();
        } else {
          pending.reject(
            connectionError(`Write failed: ${fsDoneStatusText(parsed.status)}`),
          );
        }
        return;
      }
      case S2C_FS_CLOSED: {
        if (bytes.length < 4) return;
        const syncId = bytes[1] | (bytes[2] << 8);
        const share = this.fsSyncs.get(syncId);
        if (!share) return;
        this.fsSyncs.delete(syncId);
        if (this.fsSyncsByKey.get(share.key) === share) {
          this.fsSyncsByKey.delete(share.key);
        }
        const reason = bytes[3];
        for (const consumer of share.consumers) {
          this.dispatchFs(
            consumer,
            () => {
              consumer.options.onClosed?.(reason);
              consumer.notifier.emit();
            },
            false,
          );
        }
        share.consumers.clear();
        return;
      }
      case S2C_KV_OPENED: {
        const parsed = parseKvOpenedMessage(bytes);
        if (!parsed) return;
        const pending = this.pendingKvOpens.get(parsed.nonce);
        if (!pending) return;
        this.pendingKvOpens.delete(parsed.nonce);
        if (parsed.status !== KV_STATUS_OK) {
          pending.reject(
            connectionError(`Watch failed: ${kvStatusText(parsed.status)}`),
          );
          return;
        }
        const mirror = new KvMirror();
        this.kvWatches.set(parsed.kvId, { mirror, options: pending.options });
        const kvId = parsed.kvId;
        pending.resolve({
          kvId,
          mirror,
          close: () => {
            if (this.kvWatches.delete(kvId)) {
              if (this.transport.status === "connected") {
                this.transport.send(buildKvStopMessage(kvId));
              }
            }
          },
        });
        return;
      }
      case S2C_KV_UPDATE: {
        if (bytes.length < 8) return;
        const kvId = bytes[1] | (bytes[2] << 8);
        const watch = this.kvWatches.get(kvId);
        if (!watch) return;
        const updateId = watch.mirror.applyUpdate(bytes);
        if (updateId === null) {
          this._logger.warn(`${this.id}: malformed KV_UPDATE for ${kvId}`);
          return;
        }
        // Acks are load-bearing: they advance the server's retention
        // floor, and a subscription whose queued-unacked bytes breach
        // `BLIT_KV_UNACKED_MAX` is dropped with `KV_CLOSED` reason
        // RESOURCE_LIMIT (docs/design/kv.md "Retention").
        this.transport.send(buildKvAckMessage(kvId, updateId));
        watch.options.onUpdate?.(watch.mirror);
        return;
      }
      case S2C_KV_DONE: {
        const parsed = parseKvDoneMessage(bytes);
        if (!parsed) return;
        const pending = this.pendingKvPuts.get(parsed.nonce);
        if (!pending) return;
        this.pendingKvPuts.delete(parsed.nonce);
        if (parsed.status === KV_STATUS_OK) {
          pending.resolve({ hash: parsed.hash, mtimeNs: parsed.mtimeNs });
        } else if (parsed.status === KV_STATUS_CONFLICT) {
          // The same conflict shape as fs writes: `hash` carries the
          // current value hash so the caller rebases without a round trip.
          pending.reject(new FsConflictError(parsed.hash));
        } else {
          pending.reject(
            connectionError(`Put failed: ${kvStatusText(parsed.status)}`),
          );
        }
        return;
      }
      case S2C_KV_VALUE: {
        const parsed = parseKvValueMessage(bytes);
        if (!parsed) return;
        const pending = this.pendingKvFetches.get(parsed.nonce);
        if (!pending) return;
        this.pendingKvFetches.delete(parsed.nonce);
        if (parsed.status === KV_STATUS_OK) {
          pending.resolve({ hash: parsed.hash, value: parsed.data });
        } else if (parsed.status === KV_STATUS_NOT_FOUND) {
          pending.resolve(null);
        } else {
          pending.reject(
            connectionError(`Fetch failed: ${kvStatusText(parsed.status)}`),
          );
        }
        return;
      }
      case S2C_KV_CLOSED: {
        // [0x74][kv_id:2][reason:1] — the subscription is gone server-side.
        // Removing the watch first means the handle's future close() finds
        // nothing to delete and sends no KV_STOP for the dead id; the
        // mirror is dead and recovery is the caller re-`watchKv`ing (the
        // fresh snapshot is the recovery, docs/design/kv.md "Retention").
        if (bytes.length < 4) return;
        const kvId = bytes[1] | (bytes[2] << 8);
        const watch = this.kvWatches.get(kvId);
        if (!watch) return;
        this.kvWatches.delete(kvId);
        watch.options.onClosed?.(
          connectionError(`Watch closed: ${kvClosedText(bytes[3])}`),
        );
        return;
      }
      case S2C_GIT_REPO: {
        const info = parseGitRepo(bytes);
        if (!info) return;
        const pending = this.pendingGitOpens.get(info.nonce);
        if (!pending) return;
        this.pendingGitOpens.delete(info.nonce);
        if (info.status !== GIT_STATUS_OK) {
          // GIT_REPO carries its diagnostic in `workdir` on failure.
          pending.reject(new GitStatusError("Open", info.status, info.workdir));
          return;
        }
        const mirror = new GitStateMirror();
        const notifier = new Notifier();
        this.gitRepos.set(info.repoId, {
          mirror,
          options: pending.options,
          notifier,
        });
        pending.resolve(
          this.makeGitRepoHandle(info.repoId, info, mirror, notifier),
        );
        return;
      }
      case S2C_GIT_STATE: {
        if (bytes.length < 8) return;
        const repoId = bytes[1] | (bytes[2] << 8);
        const repo = this.gitRepos.get(repoId);
        if (!repo) return;
        const stateId = repo.mirror.applyState(bytes);
        if (stateId === null) {
          this._logger.warn(
            `${this.id}: malformed GIT_STATE for repo ${repoId}`,
          );
          return;
        }
        this.transport.send(msgGitAck(repoId, stateId));
        // Revision before the callback: revision-keyed caches re-read
        // inside it must observe the bump (same invariant as fs updates).
        repo.notifier.emit();
        repo.options.onState?.(repo.mirror, stateId);
        return;
      }
      case S2C_GIT_CLOSED: {
        const closed = parseGitClosed(bytes);
        if (!closed) return;
        const repo = this.gitRepos.get(closed[0]);
        if (!repo) return;
        this.gitRepos.delete(closed[0]);
        this.closeGitLogSubs(closed[0]);
        repo.options.onClosed?.(closed[1]);
        repo.notifier.emit();
        return;
      }
      case S2C_GIT_LOG_PAGE: {
        const page = parseGitLogPage(bytes);
        if (!page) return;
        const sub = this.gitLogSubs.get(page.logId);
        if (!sub) return;
        // Acknowledge before delivering: pacing must not wait on the callback.
        if (this.transport.status === "connected") {
          this.transport.send(
            msgGitLogAck(page.logId, sub.repoId, page.updateId),
          );
        }
        sub.onUpdate(page);
        return;
      }
      case S2C_GIT_COMMITS:
      case S2C_GIT_TREE:
      case S2C_GIT_BLOB:
      case S2C_GIT_DIFF:
      case S2C_GIT_PATCH:
      case S2C_GIT_INDEX:
      case S2C_GIT_BASE:
      case S2C_GIT_DISCOVER:
      case S2C_GIT_BLAME:
      case S2C_GIT_REFLOG:
      case S2C_GIT_FETCH:
      case S2C_GIT_RESOLVE: {
        if (bytes.length < 3) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const pending = this.pendingGitRequests.get(nonce);
        if (!pending || pending.opcode !== bytes[0]) return;
        this.pendingGitRequests.delete(nonce);
        // An abandoned request has already rejected; the reply only
        // releases its nonce.
        if (!pending.abandoned) pending.resolve(bytes);
        return;
      }
      case S2C_LSP_OPENED: {
        const opened = parseLspOpened(bytes);
        if (!opened) return;
        const pending = this.pendingLspOpens.get(opened.nonce);
        if (!pending) return;
        this.pendingLspOpens.delete(opened.nonce);
        if (opened.status !== LSP_STATUS_OK) {
          pending.reject(
            connectionError(
              `Open failed: ${lspStatusText(opened.status)}${opened.detail ? `: ${opened.detail}` : ""}`,
            ),
          );
          return;
        }
        const state = new LspStateMirror();
        const diags = new LspDiagMirror();
        const notifier = new Notifier();
        this.lspAttachments.set(opened.lspId, {
          state,
          diags,
          options: pending.options,
          notifier,
        });
        pending.resolve(
          this.makeLspHandle(opened.lspId, opened.root, state, diags, notifier),
        );
        return;
      }
      case S2C_LSP_STATE: {
        if (bytes.length < 8) return;
        const lspId = bytes[1] | (bytes[2] << 8);
        const attachment = this.lspAttachments.get(lspId);
        if (!attachment) return;
        const stateId = attachment.state.applyState(bytes);
        if (stateId === null) {
          this._logger.warn(
            `${this.id}: malformed LSP_STATE for attachment ${lspId}`,
          );
          return;
        }
        // Acknowledge before delivering: pacing must not wait on the callback.
        this.transport.send(msgLspAck(lspId, LSP_STREAM_STATE, stateId));
        // Revision before the callback (same invariant as fs updates).
        attachment.notifier.emit();
        attachment.options.onState?.(attachment.state, stateId);
        return;
      }
      case S2C_LSP_DIAG: {
        if (bytes.length < 8) return;
        const lspId = bytes[1] | (bytes[2] << 8);
        const attachment = this.lspAttachments.get(lspId);
        if (!attachment) return;
        const updateId = attachment.diags.applyDiag(bytes);
        if (updateId === null) {
          this._logger.warn(
            `${this.id}: malformed LSP_DIAG for attachment ${lspId}`,
          );
          return;
        }
        // Acknowledge before delivering: pacing must not wait on the callback.
        this.transport.send(msgLspAck(lspId, LSP_STREAM_DIAG, updateId));
        // Revision before the callback (same invariant as fs updates).
        attachment.notifier.emit();
        attachment.options.onDiagnostics?.(attachment.diags, updateId);
        return;
      }
      case S2C_LSP_QUERY: {
        if (bytes.length < 3) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        const pending = this.pendingLspRequests.get(nonce);
        if (!pending) return;
        this.pendingLspRequests.delete(nonce);
        pending.resolve(bytes);
        return;
      }
      case S2C_LSP_CLOSED: {
        const closed = parseLspClosed(bytes);
        if (!closed) return;
        const attachment = this.lspAttachments.get(closed[0]);
        if (!attachment) return;
        this.lspAttachments.delete(closed[0]);
        attachment.options.onClosed?.(closed[1]);
        attachment.notifier.emit();
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
      this.hasReceivedList = false;
      this.retryCount = 0;
      this.lastError = null;
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

    // When the transport connects, the blit protocol handshake has not
    // necessarily produced any server frames yet. Report "authenticating" until
    // noteServerResponsive()/S2C_HELLO/S2C_READY confirms the upstream blit
    // server is reachable. `ready` still waits for S2C_READY.
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
      // Fs syncs and git repos do not survive a transport drop; reject
      // their pending promises promptly rather than leaving them hung.
      this.resetFsSyncs(connectionError(`Transport ${status}`));
      this.resetGitRepos(connectionError(`Transport ${status}`));
      this.resetLspAttachments(connectionError(`Transport ${status}`));
      this.resetKv(connectionError(`Transport ${status}`));
      this.resetFragmentReassembly();
      this.termCwds.clear();
      this.resolveAllPendingCloses();
      this.hasReceivedList = false;
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
      // Emit the ready=false snapshot *before* wiping the surface store
      // so reactive consumers (BSP reconciliation) see the connection
      // as "not ready" when surfaces drop out of the live list.
      // Otherwise surfaceStore.handleDisconnect() can synchronously
      // trigger a reconcile that still thinks the connection is ready
      // and nuke the pane surface assignments.
      this.emit();
      this.surfaceStore.handleDisconnect();
      this.audioPlayer.reset();
      // All server-side surface subscriptions are implicitly dropped
      // when the transport dies, but the CLIENT-SIDE ref-counts (one
      // per live mount) must be preserved: each mount is still there
      // and will call `refreshSurfaceSubscribe` when the store's
      // generation ticks forward, which is how the wire subscribe gets
      // re-sent on reconnect.  Just reset `lastSent` so the
      // refresh actually fires.
      this.resetSurfaceSubsForReconnect();
      return;
    }

    this.emit();
  };

  private parseListMessage(
    bytes: Uint8Array,
    includeCommand: boolean,
  ): ParsedList {
    if (bytes.length < 3) return { entries: [], complete: false };

    const count = bytes[1] | (bytes[2] << 8);
    const entries: ListEntry[] = [];
    let offset = 3;
    for (let index = 0; index < count; index++) {
      if (offset + 4 > bytes.length) {
        return { entries, complete: false };
      }
      const ptyId = bytes[offset] | (bytes[offset + 1] << 8);
      const tagLen = bytes[offset + 2] | (bytes[offset + 3] << 8);
      offset += 4;
      if (offset + tagLen > bytes.length) {
        return { entries, complete: false };
      }
      const tag = textDecoder.decode(bytes.subarray(offset, offset + tagLen));
      offset += tagLen;

      let command: string | null = null;
      if (includeCommand) {
        if (offset + 2 > bytes.length) {
          return { entries, complete: false };
        }
        const cmdLen = bytes[offset] | (bytes[offset + 1] << 8);
        offset += 2;
        if (offset + cmdLen > bytes.length) {
          return { entries, complete: false };
        }
        if (cmdLen > 0) {
          command = textDecoder.decode(bytes.subarray(offset, offset + cmdLen));
        }
        offset += cmdLen;
      }

      entries.push({ ptyId, tag, command });
    }

    return { entries, complete: offset === bytes.length };
  }

  private handleListMessage(bytes: Uint8Array): boolean {
    if (bytes.length < 3) return false;

    // Current servers include a command length after every tag. Older remote
    // servers did not, so a multi-entry legacy list can otherwise be
    // misparsed by treating the next PTY id as a command length and dropping
    // the remaining terminals. Prefer the current format when it parses
    // exactly; fall back to legacy when the current parse is incomplete.
    const withCommand = this.parseListMessage(bytes, true);
    const legacy = withCommand.complete
      ? withCommand
      : this.parseListMessage(bytes, false);
    const parsed =
      withCommand.complete || !legacy.complete ? withCommand : legacy;
    const entries = parsed.entries;

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

    return parsed.complete;
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
      usedRows: current?.usedRows ?? 0,
      command,
      state,
      exitStatus: current?.exitStatus ?? null,
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
    this.termCwds.delete(sessionId);
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
