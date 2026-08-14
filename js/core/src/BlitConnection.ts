import type {
  BlitConnectionSnapshot,
  BlitClientList,
  BlitSearchResult,
  BlitSession,
  BlitTransport,
  BlitTransportMessage,
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
  FEATURE_CLIENT_CONTROL,
  FEATURE_CREATE_NONCE,
  FEATURE_CREATE_STATUS,
  FEATURE_KILL_MODE,
  FEATURE_RESIZE_BATCH,
  FEATURE_SCROLL_BY,
  FEATURE_SURFACE_TOUCH,
  FEATURE_SURFACE_TEXT_INPUT,
  FEATURE_RESTART,
  statusText,
  S2C_AUDIO_FRAME,
  PROTOCOL_VERSION,
  S2C_CLIPBOARD_CONTENT,
  S2C_CLIPBOARD_LIST,
  S2C_CLIPBOARD_OWNER,
  S2C_CLOSED,
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_CREATE_FAILED,
  S2C_EXITED,
  S2C_HELLO,
  S2C_KICKED,
  S2C_CLIENT_LIST,
  S2C_KICK_RESULT,
  S2C_LIST,
  S2C_READY,
  S2C_SEARCH_RESULTS,
  S2C_SURFACE_APP_ID,
  S2C_SURFACE_ACTIVATED,
  S2C_SURFACE_TEXT_INPUT,
  S2C_SURFACE_CURSOR,
  S2C_SURFACE_REMOTE_INPUT,
  REMOTE_INPUT_POINTER,
  REMOTE_INPUT_TOUCH,
  S2C_SURFACE_CREATED,
  S2C_SURFACE_DESTROYED,
  S2C_SURFACE_ENCODER,
  S2C_SURFACE_FRAME,
  SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US,
  S2C_SURFACE_RESIZED,
  S2C_SURFACE_TITLE,
  SURFACE_TEXT_INPUT_ENABLED,
  SURFACE_TEXT_INPUT_REQUESTED,
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
  STATUS_OK,
  SURFACE_TOUCH_ENABLE,
  SURFACE_TOUCH_DISABLE,
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
  buildSurfaceDragEnterMessage,
  buildSurfaceDragMotionMessage,
  buildSurfaceDragLeaveMessage,
  buildSurfaceDragDropMessage,
  buildSurfaceDragCancelMessage,
  type SurfaceDragItem,
  buildSurfaceAxisMessage,
  buildSurfaceAxis2Message,
  type SurfaceAxisEvent,
  buildSurfaceResizeMessage,
  buildSurfaceFocusMessage,
  buildSurfaceCloseMessage,
  buildSurfaceSubscribeMessage,
  buildSurfaceUnsubscribeMessage,
  buildSurfaceAckMessage,
  buildClipboardGetMessage,
  buildClipboardListMessage,
  buildClipboardMessage,
  buildPrimaryMessage,
  buildClientFeaturesMessage,
  buildClientListMessage,
  buildClientWatchMessage,
  buildClientUnwatchMessage,
  buildKickClientMessage,
  kickReasonByteLength,
  KICK_REASON_MAX,
  buildAudioSubscribeMessage,
  buildAudioUnsubscribeMessage,
  buildSurfaceTouchMessage,
  type SurfaceTouchPoint,
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
  FS_DONE_OFFSET_MISMATCH,
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
  FS_SYNC_STAGING,
  FS_WRITE_CONTENT_DELTA,
  FS_WRITE_CONTENT_FULL,
  FS_WRITE_DURABLE,
  FS_WRITE_MKPARENTS,
  FS_WRITE_NO_CAS,
  FS_UPLOAD_DURABLE,
  FS_UPLOAD_MKPARENTS,
  FS_UPLOAD_NO_CAS,
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
  S2C_FS_UPLOAD_BEGIN,
  S2C_FS_UPLOAD_CHUNK,
  S2C_FS_UPLOAD_FINISH,
  buildFsAckMessage,
  buildFsFetchMessage,
  buildFsIndexMessage,
  buildFsSearchMessage,
  buildFsOpMessage,
  buildFsStopMessage,
  buildFsSyncMessage,
  buildFsUploadBeginMessage,
  buildFsUploadCancelMessage,
  buildFsUploadChunkMessage,
  buildFsUploadFinishMessage,
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
  parseFsUploadBeginReply,
  parseFsUploadChunkAck,
  parseFsUploadFinishReply,
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
  type FsUploadOptions,
  type FsUploadResult,
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
import {
  DesktopStore,
  FEATURE_DESKTOP,
  S2C_NOTIFICATION_UPDATE,
  S2C_TRAY_MENU,
  S2C_TRAY_UPDATE,
} from "./desktop";

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

type PendingClipboardRequest<T> = {
  promise: Promise<T>;
  resolve: (value: T) => void;
  reject: (error: Error) => void;
  timer: ReturnType<typeof setTimeout>;
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

/** Unacked upload bytes allowed on the wire at once. Kept small on purpose:
 *  C2S has no flow control, so every queued upload byte sits ahead of
 *  interactive input (keyboard/mouse) sharing this connection — a multi-MiB
 *  backlog makes the terminal hang for seconds on a slow uplink. */
const FS_UPLOAD_MAX_IN_FLIGHT = 512 * 1024;
/** Default plaintext bytes per upload chunk: small enough that the in-flight
 *  cap above holds only a couple of chunks, and still far under the 16 MiB
 *  transport frame cap after compression. */
const FS_UPLOAD_DEFAULT_CHUNK = 256 * 1024;
/** The server's compositor read itself is bounded to two seconds. */
const CLIPBOARD_REQUEST_TIMEOUT_MS = 2_500;

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

function normalizeSurfaceMaxFps(maxFps: number): number {
  return Number.isFinite(maxFps)
    ? Math.max(0, Math.min(65_535, Math.round(maxFps)))
    : 0;
}

interface SurfaceViewRequest {
  target: SurfaceTarget | null;
  /** Zero means the connection's declared display rate. */
  maxFps: number;
}

/** Per-surface subscription state.  One entry per visible surface on
 *  this connection.  `views` tracks the live mounts (e.g. BSP view plus
 *  side-panel preview) sharing the stream: the wire UNSUBSCRIBE fires
 *  only when the last one goes away.  Without that, unmounting one of
 *  two mounts tears down the stream for both. */
interface SurfaceSub {
  surfaceId: number;
  /** Live mounts, keyed by the token allocSurfaceViewId() handed out. Each
   *  request carries the fixed encode size and cadence that view wants.
   *  Held per view rather than collapsed to a count because the effective
   *  request is derived from all of them — and on unmount we have to know
   *  which one left. */
  views: Map<string, SurfaceViewRequest>;
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
    maxFps: number;
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
  readonly desktopStore = new DesktopStore();

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
  private readonly pendingClientLists = new Map<
    number,
    {
      resolve: (result: BlitClientList) => void;
      reject: (error: Error) => void;
    }
  >();
  private readonly pendingClientKicks = new Map<
    number,
    { resolve: () => void; reject: (error: Error) => void }
  >();
  private readonly clientCatalogSubscribers = new Set<{
    listener: (catalog: BlitClientList) => void;
    onError?: (error: Error) => void;
  }>();
  private clientCatalogWatchNonce: number | null = null;
  /** Nonce of the most recent `CLIENT_UNWATCH`. A successful unwatch draws no
   *  reply, so without holding the nonce back it is free for reuse while a
   *  refusal of that unwatch is still in flight — and the refusal would then
   *  settle whichever request had since taken the nonce. */
  private retiredWatchNonce: number | null = null;
  private lastClientCatalog: BlitClientList | null = null;
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
  /** Unanswered `C2S_FS_UPLOAD_BEGIN`s by nonce. */
  private readonly pendingFsUploadBegins = new Map<
    number,
    {
      resolve: (uploadId: number) => void;
      reject: (error: Error) => void;
    }
  >();
  /** Live chunked uploads by server `upload_id`; `ack` is driven by each
   *  `S2C_FS_UPLOAD_CHUNK` reply. */
  private readonly pendingFsUploads = new Map<
    number,
    {
      ack: (status: number, received: number) => void;
      reject: (error: Error) => void;
    }
  >();
  /** Unanswered `C2S_FS_UPLOAD_FINISH`es by nonce. */
  private readonly pendingFsUploadFinishes = new Map<
    number,
    {
      resolve: (result: FsUploadResult) => void;
      reject: (error: Error) => void;
      /** As `pendingFsWrites.record`: a successful commit records the hash
       *  in the issuing consumer's `lastWritten`. */
      record?: { consumer: FsSyncConsumer; path: string };
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
  private clientControlNonceCounter = 0;
  private searchCounter = 0;
  private fsNonceCounter = 0;
  private gitLogIdCounter = 0;
  private features = 0;
  /** Mounted direct-touch canvases sharing this transport. The server sees
   * one virtual touchscreen capability while this count is non-zero. */
  private surfaceTouchUsers = 0;
  private disposed = false;
  /** Per-session, per-view size registry for computing minimum resize. */
  private viewSizes = new Map<
    SessionId,
    Map<string, { rows: number; cols: number; isActive?: () => boolean }>
  >();
  private viewIdCounter = 0;
  private hasReceivedList = false;
  private retryCount = 0;
  private generation = 0;
  private lastError: string | null = null;
  /** Clipboard authority learned from the compositor.  `null` means the
   *  browser may have acquired a newer clipboard while this page was not
   *  authoritative, so the next paste must import it before pressing V. */
  private waylandClipboardOwned: boolean | null = null;
  /** Text mirrored from the current Wayland owner.  Unlike the host
   *  clipboard mirror, this remains usable when browser clipboard writes are
   *  permission-gated.  Null means it has not arrived or is not text. */
  private waylandClipboardText: string | null = null;
  private pendingClipboardList: PendingClipboardRequest<string[]> | null = null;
  private pendingClipboardGets = new Map<
    string,
    PendingClipboardRequest<Uint8Array>
  >();
  private clipboardChangeTarget: EventTarget | null = null;
  private clipboardChangeHandler: EventListener | null = null;
  private clipboardMirrorToken = 0;
  private pendingClipboardMirrors = new Map<
    number,
    ReturnType<typeof setTimeout>
  >();

  /** Default video bandwidth for new surface subscriptions (0 = server default). */
  defaultSurfaceBandwidth = 0;
  /** Default encoder speed for new surface subscriptions (0 = server default). */
  defaultSurfaceSpeed = 0;
  /** User-selected ceiling applied after each view's own cadence request.
   *  Zero leaves surface cadence tied to the display rate. */
  private surfaceMaxFpsCap = 0;
  /** Default audio bitrate in kbps for audio subscriptions (0 = server default). */
  defaultAudioBitrateKbps = 0;
  /** When false, surface subscribe messages are suppressed (ref-counts
   *  still tracked so re-enabling restores subscriptions). */
  surfaceStreamingEnabled = true;
  /** Page visibility is an effective streaming gate, separate from the
   *  user's persistent video preference above. */
  private pageVisible =
    typeof document === "undefined" || document.visibilityState !== "hidden";
  private pageVisibilityHandler: (() => void) | null = null;
  private pingTimer: ReturnType<typeof setInterval> | null = null;
  private readonly pingIntervalMs = 10_000;
  private clockPingNonce = 0;
  private pendingClockPings = new Map<number, number>();

  /**
   * Reusable accumulator for `S2C_FRAGMENT` messages. TCP preserves order
   * and the server only splits one bulk message at a time, so fragments of
   * different messages never interleave. Incoming transport views are
   * borrowed; copy them into this buffer synchronously, then reuse its
   * capacity after dispatch instead of allocating once per fragment.
   * Audio frames and other small messages bypass this buffer.
   */
  private fragmentBuffer = new Uint8Array(0);
  private fragmentBytes = 0;
  private readonly surfaceAckMessages = new Map<number, Uint8Array>();

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
    this.desktopStore.setSender((message) => {
      if (this.transport.status === "connected") this.transport.send(message);
    });
    this.surfaceStore.setAckSender((surfaceId, decoderQueueDepth) => {
      if (this.transport.status === "connected") {
        let message = this.surfaceAckMessages.get(surfaceId);
        if (!message) {
          message = buildSurfaceAckMessage(surfaceId, decoderQueueDepth);
          this.surfaceAckMessages.set(surfaceId, message);
        } else {
          message[3] = Math.max(
            0,
            Math.min(255, Math.trunc(decoderQueueDepth)),
          );
        }
        this.transport.send(message);
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
      supportsSurfaceTouch: false,
      supportsSurfaceTextInput: false,
      supportsAudio: false,
      supportsClientControl: false,
      supportsFsSync: false,
      supportsGit: false,
      supportsLsp: false,
      supportsKv: false,
      supportsDesktop: false,
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
    if (typeof document !== "undefined") {
      this.pageVisibilityHandler = () => {
        this.setPageVisible(document.visibilityState !== "hidden");
      };
      document.addEventListener("visibilitychange", this.pageVisibilityHandler);
    }
    if (typeof navigator !== "undefined") {
      const clipboard = navigator.clipboard as
        | (Clipboard & { onclipboardchange?: unknown })
        | undefined;
      // `clipboardchange` is still progressively deployed.  Checking the
      // event-handler property is the feature detection recommended by the
      // API; EventTarget alone is not enough because older Clipboard objects
      // accept arbitrary event names without ever dispatching them.
      if (clipboard && "onclipboardchange" in clipboard) {
        this.clipboardChangeTarget = clipboard;
        this.clipboardChangeHandler = (event) => {
          if (this.consumeMirroredClipboardChange(event)) return;
          this.noteBrowserClipboardMayHaveChanged();
        };
        clipboard.addEventListener(
          "clipboardchange",
          this.clipboardChangeHandler,
        );
      }
    }

    // Propagate AudioPlayer state changes (e.g. reset on reconnect) into the
    // connection's listener chain so the reactive graph re-evaluates audio
    // subscription intent.  Without this, audioPlayer.reset() sets _subscribed
    // to false but nothing in the SolidJS reactive graph notices, so the
    // Workspace audio effect never re-runs to re-subscribe.
    this.audioPlayer.onChange(() => this.emit());
    this.desktopStore.subscribe(() => this.emit());

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
    if (this.lastError !== null) {
      this.lastError = null;
      this.snapshot = { ...this.snapshot, error: null };
      this.emit();
    }
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
    this.pendingClockPings.clear();
    this.transport.removeEventListener("message", this.handleMessage);
    this.transport.removeEventListener("statuschange", this.handleStatusChange);
    if (this.pageVisibilityHandler && typeof document !== "undefined") {
      document.removeEventListener(
        "visibilitychange",
        this.pageVisibilityHandler,
      );
      this.pageVisibilityHandler = null;
    }
    if (this.clipboardChangeTarget && this.clipboardChangeHandler) {
      this.clipboardChangeTarget.removeEventListener(
        "clipboardchange",
        this.clipboardChangeHandler,
      );
      this.clipboardChangeTarget = null;
      this.clipboardChangeHandler = null;
    }
    this.clearPendingClipboardMirrors();
    this.rejectPendingClipboardRequests(connectionError("Connection disposed"));
    this.rejectPendingCreates(
      connectionError("Connection disposed before PTY creation completed"),
    );
    this.rejectPendingSearches(connectionError("Connection disposed"));
    this.rejectPendingReads(connectionError("Connection disposed"));
    this.resetClientControl(connectionError("Connection disposed"));
    this.clientCatalogSubscribers.clear();
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
    this.desktopStore.reset();
    this.desktopStore.setSender(null);
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

  /** List other connections to this server and their active subscriptions. */
  listClients(): Promise<BlitClientList> {
    const error = this.clientControlAvailabilityError("list clients");
    if (error) return Promise.reject(error);
    return new Promise<BlitClientList>((resolve, reject) => {
      const nonce = this.nextClientControlNonce();
      this.pendingClientLists.set(nonce, { resolve, reject });
      this.transport.send(buildClientListMessage(nonce));
    });
  }

  /**
   * Subscribe to the live catalog of other server connections. Multiple
   * consumers share one wire subscription. The returned function disposes
   * this consumer and unwatches the server after the final consumer leaves.
   */
  subscribeClients(
    listener: (catalog: BlitClientList) => void,
    onError?: (error: Error) => void,
  ): () => void {
    const subscriber = { listener, onError };
    this.clientCatalogSubscribers.add(subscriber);
    if (this.lastClientCatalog) listener(this.lastClientCatalog);
    this.startClientCatalogWatch();
    return () => {
      if (!this.clientCatalogSubscribers.delete(subscriber)) return;
      if (this.clientCatalogSubscribers.size === 0) {
        this.stopClientCatalogWatch();
      }
    };
  }

  /** Disconnect another connection to this server. */
  kickClient(clientId: bigint, reason = ""): Promise<void> {
    const error = this.clientControlAvailabilityError("kick a client");
    if (error) return Promise.reject(error);
    if (clientId < 0n || clientId > 0xffff_ffff_ffff_ffffn) {
      return Promise.reject(
        connectionError("Client ID is outside the u64 range"),
      );
    }
    // Refuse rather than send a silently shortened reason: the point of a
    // reason is that the kicked peer reads what you wrote.
    const reasonBytes = kickReasonByteLength(reason);
    if (reasonBytes > KICK_REASON_MAX) {
      return Promise.reject(
        connectionError(
          `Kick reason is ${reasonBytes} bytes; maximum is ${KICK_REASON_MAX}`,
        ),
      );
    }
    return new Promise<void>((resolve, reject) => {
      const nonce = this.nextClientControlNonce();
      this.pendingClientKicks.set(nonce, { resolve, reject });
      this.transport.send(buildKickClientMessage(nonce, clientId, reason));
    });
  }

  private clientControlAvailabilityError(action: string): Error | null {
    if (this.transport.status !== "connected") {
      return connectionError(
        `Cannot ${action} while transport is ${this.transport.status}`,
      );
    }
    if ((this.features & FEATURE_CLIENT_CONTROL) === 0) {
      return connectionError("Server does not support client control");
    }
    return null;
  }

  private nextClientControlNonce(): number {
    let nonce = 0;
    do {
      nonce = this.clientControlNonceCounter =
        (this.clientControlNonceCounter + 1) & 0xffff;
    } while (
      this.pendingClientLists.has(nonce) ||
      this.pendingClientKicks.has(nonce) ||
      this.clientCatalogWatchNonce === nonce ||
      this.retiredWatchNonce === nonce
    );
    return nonce;
  }

  private startClientCatalogWatch(): void {
    if (
      this.clientCatalogSubscribers.size === 0 ||
      this.clientCatalogWatchNonce !== null
    ) {
      return;
    }
    const error = this.clientControlAvailabilityError(
      "subscribe to the client catalog",
    );
    if (error) {
      for (const subscriber of this.clientCatalogSubscribers) {
        subscriber.onError?.(error);
      }
      return;
    }
    const nonce = this.nextClientControlNonce();
    this.clientCatalogWatchNonce = nonce;
    this.transport.send(buildClientWatchMessage(nonce));
  }

  private stopClientCatalogWatch(): void {
    const nonce = this.clientCatalogWatchNonce;
    this.clientCatalogWatchNonce = null;
    this.lastClientCatalog = null;
    if (nonce !== null && this.transport.status === "connected") {
      this.retiredWatchNonce = nonce;
      this.transport.send(buildClientUnwatchMessage(nonce));
    }
  }

  private resetClientControl(error: Error, notifySubscribers = true): void {
    for (const pending of this.pendingClientLists.values()) {
      pending.reject(error);
    }
    this.pendingClientLists.clear();
    for (const pending of this.pendingClientKicks.values()) {
      pending.reject(error);
    }
    this.pendingClientKicks.clear();
    const hadWatch =
      this.clientCatalogWatchNonce !== null || this.lastClientCatalog !== null;
    this.clientCatalogWatchNonce = null;
    // Nothing is in flight across a reset, so the held-back unwatch nonce is
    // free again; keeping it would retire one nonce per reconnect.
    this.retiredWatchNonce = null;
    this.lastClientCatalog = null;
    if (notifySubscribers && hadWatch) {
      for (const subscriber of this.clientCatalogSubscribers) {
        subscriber.onError?.(error);
      }
    }
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
    // A staging sync is rooted at the connection's drag staging dir, which
    // no pty cwd can name — the server refuses the pair, so say so here.
    if (options.staging && options.fromSessionId) {
      throw connectionError(
        "A staging sync cannot be resolved from a terminal's cwd",
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
    if (options.staging) flags |= FS_SYNC_STAGING;
    const srcPtyId = this.srcPtyForOpen(options.fromSessionId);
    const latencyMs = options.latencyMs ?? 0;
    const inlineMax = options.inlineMax ?? 0;
    // A staging sync's root is the connection's drag staging dir — the
    // path field is ignored and goes out empty.
    const syncPath = options.staging ? "" : path;
    // The pattern list is part of what the sync *is* — two opens that
    // exclude different things mirror different trees — so it joins the
    // coalescing key alongside the flags.
    const key = `${flags}:${latencyMs}:${inlineMax}:${srcPtyId ?? ""}:${exclude.length}:${exclude}${syncPath}`;
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
          syncPath,
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
      upload: (path, data, opts = {}) =>
        this.fsUpload(syncId, consumer, path, data, opts),
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
    // A delta applies against the exact bytes the CAS precondition
    // names (docs/design/fs-write.md content_kind 2), so it demands a
    // real nonzero-hash anchor: no `force`, no create-exclusive, no
    // unconditional write.
    if (
      options.deltaBase !== undefined &&
      (options.force || options.ifHash === undefined || options.ifHash === 0n)
    ) {
      return Promise.reject(
        connectionError(
          "deltaBase requires a nonzero ifHash precondition (without force)",
        ),
      );
    }
    // Decide the encoding before routing: delta ops go out only when
    // clearly smaller than the full content (the server's own heuristic,
    // crates/fssync/src/lib.rs), which keeps them far under the frame cap.
    let deltaOps: Uint8Array | undefined;
    if (options.deltaBase !== undefined) {
      const ops = encodeFsDelta(options.deltaBase, data);
      if (ops.length * 8 < data.length * 7) deltaOps = ops;
    }
    // Every full-content write rides the chunked-upload pump — preconditions
    // included, the upload family carries FS_WRITE's base semantics. We care
    // about interactive latency above per-write round trips: a paced stream
    // of small chunks can never queue ahead of keyboard/mouse input on this
    // connection, however big the write. Delta frames are small by
    // construction and stay on FS_WRITE.
    if (deltaOps === undefined) {
      return this.fsUpload(syncId, consumer, path, data, {
        mode: options.mode,
        createParents: options.createParents,
        durable: options.durable,
        ifHash: options.ifHash,
        create: options.create,
        force: options.force,
      }).then((r) => ({ hash: r.hashU128, mtimeNs: r.mtimeNs }));
    }
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
      if (deltaOps !== undefined) {
        // A pre-delta server answers INVALID for content_kind 2: retry
        // once as a full write with the same precondition, surfacing
        // only the retry's outcome. (CONFLICT is a real CAS failure
        // and never retries.)
        send(FS_WRITE_CONTENT_DELTA, deltaOps, () =>
          send(FS_WRITE_CONTENT_FULL, data),
        );
      }
    });
  }

  /**
   * Chunked upload (the `FS_UPLOAD_*` family): BEGIN, a pipelined run of
   * CHUNKs acked cumulatively, FINISH. `data` may be a `Blob`, read slice
   * by slice so the whole file is never in memory at once.
   */
  private fsUpload(
    syncId: number,
    consumer: FsSyncConsumer,
    path: string,
    data: Uint8Array | Blob,
    options: FsUploadOptions,
  ): Promise<FsUploadResult> {
    return new Promise<FsUploadResult>((resolve, reject) => {
      if (!this.fsSyncs.has(syncId)) {
        reject(connectionError("Sync is closed"));
        return;
      }
      const total = data instanceof Blob ? data.size : data.length;
      const chunkSize = options.chunkSize ?? FS_UPLOAD_DEFAULT_CHUNK;
      if (!Number.isInteger(chunkSize) || chunkSize <= 0) {
        reject(connectionError("chunkSize must be a positive integer"));
        return;
      }
      const signal = options.signal;
      if (signal?.aborted) {
        reject(connectionError("Upload aborted"));
        return;
      }
      let flags = 0;
      if (options.createParents) flags |= FS_UPLOAD_MKPARENTS;
      if (options.durable) flags |= FS_UPLOAD_DURABLE;
      // Precondition, mirroring fsWrite: create-exclusive (base 0), CAS
      // (base = ifHash), or — by default or under force — an unconditional
      // overwrite. Checked at BEGIN and re-verified at FINISH.
      let base = 0n;
      if (options.force) {
        flags |= FS_UPLOAD_NO_CAS;
      } else if (options.create) {
        base = 0n;
      } else if (options.ifHash !== undefined) {
        base = options.ifHash;
      } else {
        flags |= FS_UPLOAD_NO_CAS;
      }

      let settled = false;
      const fail = (error: Error): void => {
        if (settled) return;
        settled = true;
        if (uploadId >= 0) this.pendingFsUploads.delete(uploadId);
        signal?.removeEventListener("abort", onAbort);
        reject(error);
      };
      const done = (result: FsUploadResult): void => {
        if (settled) return;
        settled = true;
        if (uploadId >= 0) this.pendingFsUploads.delete(uploadId);
        signal?.removeEventListener("abort", onAbort);
        resolve(result);
      };

      // Chunk-pump state, valid once BEGIN is accepted.
      let uploadId = -1;
      let sent = 0; // next plaintext offset to send
      let inFlightBytes = 0; // unacked plaintext bytes on the wire
      const inFlightLens: number[] = []; // FIFO of unacked chunk lengths
      let generation = 0; // bumped on a rewind so stale async reads drop out
      // Drag-provider Files are lazy Blobs.  In particular, iPad screenshot
      // providers can stop making progress when asked to materialize several
      // slices concurrently.  Serialize Blob reads, while still allowing the
      // chunks already materialized and sent to remain pipelined on the wire.
      // Uint8Array reads do not touch a provider and can stay parallel.
      let blobReadTail = Promise.resolve();
      // The transport is ordered too: even a non-Blob async source must never
      // let a later slice overtake an earlier one.
      let nextToWire = 0;
      const readyChunks = new Map<number, Uint8Array>();
      let finishing = false;
      const sliceAt = async (
        offset: number,
        length: number,
      ): Promise<Uint8Array> =>
        data instanceof Blob
          ? new Uint8Array(
              await data.slice(offset, offset + length).arrayBuffer(),
            )
          : data.subarray(offset, offset + length);
      const finish = (): void => {
        if (finishing) return;
        finishing = true;
        const nonce = this.nextFsNonce(this.pendingFsUploadFinishes);
        this.pendingFsUploadFinishes.set(nonce, {
          resolve: done,
          reject: fail,
          record: { consumer, path },
        });
        this.transport.send(buildFsUploadFinishMessage(nonce, uploadId));
      };
      const flushReadyChunks = (): void => {
        while (!settled) {
          const bytes = readyChunks.get(nextToWire);
          if (!bytes) return;
          readyChunks.delete(nextToWire);
          const offset = nextToWire;
          nextToWire += bytes.length;
          this.transport.send(
            buildFsUploadChunkMessage(uploadId, offset, bytes),
          );
        }
      };
      const failSourceRead = (cause: unknown): void => {
        if (settled) return;
        if (uploadId >= 0 && this.transport.status === "connected") {
          this.transport.send(buildFsUploadCancelMessage(uploadId));
        }
        const detail = cause instanceof Error ? cause.message : String(cause);
        fail(connectionError(`Upload source read failed: ${detail}`));
      };
      const queueSlice = (
        offset: number,
        length: number,
      ): Promise<Uint8Array> => {
        if (!(data instanceof Blob)) return sliceAt(offset, length);
        const read = blobReadTail.then(() => sliceAt(offset, length));
        // Keep later reads moving after this one rejects; `read` still
        // reports the failure through failSourceRead below.
        blobReadTail = read.then(
          () => undefined,
          () => undefined,
        );
        return read;
      };
      const kick = (): void => {
        while (
          !settled &&
          !finishing &&
          inFlightBytes < FS_UPLOAD_MAX_IN_FLIGHT &&
          sent < total
        ) {
          const offset = sent;
          const length = Math.min(chunkSize, total - sent);
          const gen = generation;
          sent += length;
          inFlightBytes += length;
          inFlightLens.push(length);
          void queueSlice(offset, length)
            .then((bytes) => {
              // A rewind (OFFSET_MISMATCH) or abort while the slice was being
              // read makes this chunk stale; it must not hit the wire.
              if (gen !== generation || settled) return;
              if (bytes.length !== length) {
                throw new Error(
                  `slice at ${offset} returned ${bytes.length} of ${length} bytes`,
                );
              }
              readyChunks.set(offset, bytes);
              flushReadyChunks();
            })
            .catch((cause) => {
              if (gen !== generation || settled) return;
              failSourceRead(cause);
            });
        }
        if (!settled && sent >= total && inFlightBytes === 0) {
          // total 0 never enters the loop; every byte acked ends here too.
          finish();
        }
      };
      const ack = (status: number, received: number): void => {
        if (settled || finishing) return;
        if (status === FS_DONE_OFFSET_MISMATCH) {
          // Resend from the server's resume point; in-order transports mean
          // the acks already in flight belong to chunks past that point and
          // their duplicate mismatches converge on the same offset.
          generation++;
          sent = received;
          nextToWire = received;
          readyChunks.clear();
          // Reads queued by the superseded generation must not hold up the
          // resumed stream.  Their generation guards discard late results.
          blobReadTail = Promise.resolve();
          inFlightBytes = 0;
          inFlightLens.length = 0;
          kick();
          return;
        }
        if (status !== FS_DONE_OK) {
          fail(connectionError(`Upload failed: ${fsDoneStatusText(status)}`));
          return;
        }
        inFlightBytes -= inFlightLens.shift() ?? 0;
        options.onProgress?.(received, total);
        kick();
      };
      const onAbort = (): void => {
        if (uploadId >= 0 && this.transport.status === "connected") {
          this.transport.send(buildFsUploadCancelMessage(uploadId));
        }
        fail(connectionError("Upload aborted"));
      };
      signal?.addEventListener("abort", onAbort, { once: true });

      const nonce = this.nextFsNonce(this.pendingFsUploadBegins);
      this.pendingFsUploadBegins.set(nonce, {
        resolve: (id) => {
          if (settled) {
            // Aborted while BEGIN was in flight: the server now holds an
            // upload it must be told to drop.
            if (this.transport.status === "connected") {
              this.transport.send(buildFsUploadCancelMessage(id));
            }
            return;
          }
          uploadId = id;
          this.pendingFsUploads.set(id, {
            ack,
            reject: fail,
          });
          kick();
        },
        reject: fail,
      });
      this.transport.send(
        buildFsUploadBeginMessage({
          nonce,
          syncId,
          flags,
          base,
          mode: options.mode ?? 0,
          size: total,
          path,
        }),
      );
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
    for (const pending of this.pendingFsUploadBegins.values()) {
      pending.reject(error);
    }
    this.pendingFsUploadBegins.clear();
    for (const pending of this.pendingFsUploads.values()) {
      pending.reject(error);
    }
    this.pendingFsUploads.clear();
    for (const pending of this.pendingFsUploadFinishes.values()) {
      pending.reject(error);
    }
    this.pendingFsUploadFinishes.clear();
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
    isActive?: () => boolean,
  ): void {
    let views = this.viewSizes.get(sessionId);
    if (!views) {
      views = new Map();
      this.viewSizes.set(sessionId, views);
    }
    views.set(viewId, { rows, cols, isActive });
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

  /**
   * Forget every mounted terminal view owned by this browser client.
   *
   * This is primarily an HMR recovery boundary. A hot update can replace the
   * UI tree without running every old surface cleanup, leaving an orphaned
   * (often smaller) view in {@link viewSizes}. Since the session uses the
   * minimum of all registered views, that stale entry pins the terminal to a
   * small grid until the page is refreshed. Clear the server constraints in
   * one batch before the replacement tree registers its live views again.
   */
  resetViewSizes(): void {
    const sessionIds = [...this.viewSizes.keys()];
    if (sessionIds.length === 0) return;
    this.viewSizes.clear();
    this.clearSessionSizes(sessionIds);
  }

  private sendMinSize(sessionId: SessionId): void {
    const views = this.viewSizes.get(sessionId);
    if (!views || views.size === 0) return;
    // HMR can remove a terminal's DOM subtree without reaching the old
    // component cleanup. The replacement surface still registers normally;
    // prune disconnected predecessors at that point so an orphaned small pane
    // cannot remain the session minimum until a full page refresh.
    for (const [viewId, view] of views) {
      if (!view.isActive) continue;
      let active = false;
      try {
        active = view.isActive();
      } catch {
        // A liveness probe belongs to UI teardown code. If that code is gone,
        // the view it described is gone too.
      }
      if (!active) views.delete(viewId);
    }
    if (views.size === 0) {
      this.viewSizes.delete(sessionId);
      this.clearSessionSize(sessionId);
      return;
    }
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

  sendSurfaceInput(
    surfaceId: number,
    keycode: number,
    pressed: boolean,
    timeMs = 0,
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(
      buildSurfaceInputMessage(surfaceId, keycode, pressed, timeMs),
    );
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
    timeMs = 0,
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(
      buildSurfacePointerMessage(surfaceId, type, button, x, y, timeMs),
    );
  }

  get supportsSurfaceTouch(): boolean {
    return (this.features & FEATURE_SURFACE_TOUCH) !== 0;
  }

  get supportsSurfaceTextInput(): boolean {
    return (this.features & FEATURE_SURFACE_TEXT_INPUT) !== 0;
  }

  /** Keep the compositor's virtual touchscreen capability present while at
   * least one mounted view is configured for direct touch. */
  acquireSurfaceTouch(): void {
    this.surfaceTouchUsers++;
    if (this.surfaceTouchUsers === 1) this.syncSurfaceTouchCapability();
  }

  releaseSurfaceTouch(): void {
    if (this.surfaceTouchUsers === 0) return;
    this.surfaceTouchUsers--;
    if (
      this.surfaceTouchUsers === 0 &&
      this.transport.status === "connected" &&
      this.supportsSurfaceTouch
    ) {
      this.transport.send(buildSurfaceTouchMessage(0, SURFACE_TOUCH_DISABLE));
    }
  }

  private syncSurfaceTouchCapability(): void {
    if (
      this.surfaceTouchUsers === 0 ||
      this.transport.status !== "connected" ||
      !this.supportsSurfaceTouch
    )
      return;
    this.transport.send(buildSurfaceTouchMessage(0, SURFACE_TOUCH_ENABLE));
  }

  sendSurfaceTouch(
    surfaceId: number,
    phase: number,
    contacts: readonly SurfaceTouchPoint[] = [],
    timeMs = 0,
  ): void {
    if (
      this.transport.status !== "connected" ||
      !this.supportsSurfaceTouch ||
      this.surfaceTouchUsers === 0
    )
      return;
    this.transport.send(
      buildSurfaceTouchMessage(surfaceId, phase, contacts, timeMs),
    );
  }

  sendSurfaceDragEnter(
    surfaceId: number,
    x: number,
    y: number,
    mimes: string[],
    items?: string[],
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(
      buildSurfaceDragEnterMessage(surfaceId, x, y, mimes, items),
    );
  }

  sendSurfaceDragMotion(surfaceId: number, x: number, y: number): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceDragMotionMessage(surfaceId, x, y));
  }

  sendSurfaceDragLeave(surfaceId: number): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceDragLeaveMessage(surfaceId));
  }

  sendSurfaceDragDrop(
    surfaceId: number,
    x: number,
    y: number,
    items: SurfaceDragItem[],
  ): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceDragDropMessage(surfaceId, x, y, items));
  }

  sendSurfaceDragCancel(): void {
    if (this.transport.status !== "connected") return;
    this.transport.send(buildSurfaceDragCancelMessage());
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
  // offer and withdraw their sizes, {@link effectiveSurfaceViewSize}
  // folds them into the one the wire can carry, and the unset only goes
  // out when no sized view remains.
  private surfaceViewSizes = new Map<
    number,
    {
      /** Every live view's own offer, keyed by view id. */
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
    entry.views.set(viewId, { width, height, scale120 });
    return this.flushSurfaceViewSize(surfaceId, entry);
  }

  /** Withdraw one view's size.  Re-derives the request across the surviving
   *  views — the departing one may have been the constraint — or sends the
   *  unset when it was the last sized view. */
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

  /** Forget the size the server was told for this surface.
   *
   *  A wire UNSUBSCRIBE makes the server drop this client's view size
   *  (`C2S_SURFACE_UNSUBSCRIBE` clears `surface_view_sizes`), so what we
   *  last sent is no longer what it knows. Without this the offer that
   *  follows the next subscribe dedups against a size only the previous
   *  subscription ever carried, and the client silently drops out of the
   *  server's size mediation: it is subscribed, it has a pane, and it has
   *  no say in how big the surface is. With another viewer watching, the
   *  surface then sits at *their* size forever — the pane's box never
   *  changes again, so no new offer is ever made. Hiding the tab and
   *  coming back is enough to trigger it.
   *
   *  Mirrors what {@link resetSurfaceSubsForReconnect} does for a new
   *  server session. */
  private forgetSentSurfaceViewSize(surfaceId: number): void {
    const entry = this.surfaceViewSizes.get(surfaceId);
    if (entry) entry.lastSent = null;
  }

  /** Re-offer the effective view size after a subscribe goes on the wire.
   *  No-op unless {@link forgetSentSurfaceViewSize} (or a reconnect) cleared
   *  `lastSent`, so a steady-state resubscribe costs nothing. */
  private resendSurfaceViewSize(surfaceId: number): void {
    const entry = this.surfaceViewSizes.get(surfaceId);
    if (entry && entry.views.size > 0) {
      this.flushSurfaceViewSize(surfaceId, entry);
    }
  }

  /**
   * The one size this connection can ask for on behalf of every live view
   * of a surface.
   *
   * The wire carries one size per (client, surface), so several views have
   * to be reconciled into a single request — and the answer is the same
   * one the server computes across clients: the largest logical box that
   * fits in every view, at the highest density any of them will display.
   * Taking the most recent offer instead made the surface follow whichever
   * pane was measured last, so the other one was left with a surface too
   * big for it — the same defect the server's mediation exists to prevent,
   * reintroduced one layer up.
   *
   * The constraining view's own physical extent is returned verbatim when
   * it is already at the winning scale: the logical round trip does not
   * return what it was given (at 2× an odd extent comes back a pixel
   * *larger*, 1001 → 501 → 1002), and a surface a pixel bigger than the
   * pane that asked for it shows up as a letterbox bar. `Session::
   * mediated_size_for_surface` takes the same escape hatch for the same
   * reason.
   */
  private effectiveSurfaceViewSize(
    views: Iterable<{ width: number; height: number; scale120: number }>,
  ): { width: number; height: number; scale120: number } | null {
    // Wayland's output scale floor is 1×; an unset (0) scale means the
    // view never named one, which is the same thing.
    const eff = (s: number) => (s >= 120 ? s : 120);
    // Round half up so a 1× and a 2× view reporting the same logical box
    // land on the same logical integer.
    const logical = (px: number, s: number) =>
      Math.floor((px * 120 + eff(s) / 2) / eff(s));
    let minW: { logical: number; px: number; scale120: number } | null = null;
    let minH: { logical: number; px: number; scale120: number } | null = null;
    let scale120 = 0;
    for (const v of views) {
      if (v.width <= 0 || v.height <= 0) continue;
      const lw = logical(v.width, v.scale120);
      const lh = logical(v.height, v.scale120);
      if (!minW || lw < minW.logical) {
        minW = { logical: lw, px: v.width, scale120: v.scale120 };
      }
      if (!minH || lh < minH.logical) {
        minH = { logical: lh, px: v.height, scale120: v.scale120 };
      }
      scale120 = Math.max(scale120, v.scale120);
    }
    if (!minW || !minH) return null;
    const exact = (m: { logical: number; px: number; scale120: number }) =>
      eff(m.scale120) === eff(scale120)
        ? m.px
        : Math.max(
            1,
            Math.floor((Math.max(1, m.logical) * eff(scale120)) / 120),
          );
    return { width: exact(minW), height: exact(minH), scale120 };
  }

  private flushSurfaceViewSize(
    surfaceId: number,
    entry: NonNullable<ReturnType<BlitConnection["surfaceViewSizes"]["get"]>>,
  ): boolean {
    const effective = this.effectiveSurfaceViewSize(entry.views.values());
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
    for (const view of sub.views.values()) {
      if (!view.target) return null;
      width = Math.max(width, view.target.width);
      height = Math.max(height, view.target.height);
    }
    return width > 0 && height > 0 ? { width, height } : null;
  }

  /** Highest cadence any live view needs, constrained by the user's global
   *  ceiling. Zero (uncapped) wins between views, just as an unscaled target
   *  wins the resolution derivation above, but it does not bypass that cap. */
  private effectiveSurfaceMaxFps(sub: SurfaceSub): number {
    let maxFps = 0;
    for (const view of sub.views.values()) {
      if (view.maxFps <= 0) {
        maxFps = 0;
        break;
      }
      maxFps = Math.max(maxFps, view.maxFps);
    }
    if (this.surfaceMaxFpsCap <= 0) return maxFps;
    return maxFps <= 0
      ? this.surfaceMaxFpsCap
      : Math.min(maxFps, this.surfaceMaxFpsCap);
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
    if (!this.surfaceStreamingEnabled || !this.pageVisible) return;
    if (sub.views.size === 0) return;
    const bandwidth = sub.bandwidthOverride ?? this.defaultSurfaceBandwidth;
    const speed = sub.speedOverride ?? this.defaultSurfaceSpeed;
    const target = this.effectiveSurfaceTarget(sub);
    const width = target?.width ?? 0;
    const height = target?.height ?? 0;
    const maxFps = this.effectiveSurfaceMaxFps(sub);
    if (
      sub.lastSent !== null &&
      sub.lastSent.bandwidth === bandwidth &&
      sub.lastSent.speed === speed &&
      sub.lastSent.width === width &&
      sub.lastSent.height === height &&
      sub.lastSent.maxFps === maxFps
    ) {
      return;
    }
    sub.lastSent = { bandwidth, speed, width, height, maxFps };
    this._logger.info(
      `surface sub ${this.id}:${sub.surfaceId}${target ? ` @${width}x${height}` : ""}${maxFps ? ` ${maxFps}fps` : ""}`,
    );
    this.transport.send(
      buildSurfaceSubscribeMessage(
        sub.surfaceId,
        0,
        bandwidth,
        speed,
        width,
        height,
        maxFps,
      ),
    );
    // A subscribe that follows an unsubscribe reaches a server that no
    // longer knows this client's view size.  Re-offer it here rather than
    // at each of the paths that resubscribe, so none of them can forget.
    this.resendSurfaceViewSize(sub.surfaceId);
  }

  /**
   * Subscribe one view to a surface's frames.  A single wire subscription
   * exists per (connection, surface); additional views share it and the
   * effective request is derived across them.
   *
   * `viewId` comes from {@link allocSurfaceViewId} and identifies this view
   * for the lifetime of its mount.  `target` asks the server to encode a
   * fixed-size downscale for this client instead of sizing the surface to
   * fit — pass null to watch the surface at its mediated size. `maxFps`
   * limits this view's cadence; zero uses the display rate.
   */
  sendSurfaceSubscribe(
    surfaceId: number,
    viewId: string,
    target: SurfaceTarget | null = null,
    maxFps: number = 0,
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
    sub.views.set(viewId, {
      target,
      maxFps: normalizeSurfaceMaxFps(maxFps),
    });
    this.maybeSendSurfaceSubscribe(sub);
  }

  /** Update the fixed encode size one view wants, re-deriving the wire
   *  request.  No-op for a view that is not subscribed. */
  setSurfaceViewTarget(
    surfaceId: number,
    viewId: string,
    target: SurfaceTarget | null,
    maxFps?: number,
  ): void {
    const sub = this.surfaceSubs.get(surfaceId);
    if (!sub || !sub.views.has(viewId)) return;
    const previous = sub.views.get(viewId);
    const nextMaxFps =
      maxFps === undefined
        ? (previous?.maxFps ?? 0)
        : normalizeSurfaceMaxFps(maxFps);
    if (
      previous?.target?.width === target?.width &&
      previous?.target?.height === target?.height &&
      previous?.maxFps === nextMaxFps
    ) {
      return;
    }
    sub.views.set(viewId, { target, maxFps: nextMaxFps });
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
    if (!this.surfaceStreamingEnabled || !this.pageVisible) return;
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
        this.forgetSentSurfaceViewSize(surfaceId);
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

  /** Apply a user-selected cadence ceiling to every active surface stream.
   *  Existing per-view limits (such as the 15 fps thumbnail limit) still win
   *  when they are lower. Zero disables the global ceiling. */
  setSurfaceMaxFpsCap(maxFps: number): void {
    const next = normalizeSurfaceMaxFps(maxFps);
    if (this.surfaceMaxFpsCap === next) return;
    this.surfaceMaxFpsCap = next;
    for (const sub of this.surfaceSubs.values()) {
      this.maybeSendSurfaceSubscribe(sub);
    }
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
    if (enabled && this.pageVisible) {
      for (const sub of this.surfaceSubs.values()) {
        sub.lastSent = null;
        this.maybeSendSurfaceSubscribe(sub);
      }
    } else if (!enabled && this.pageVisible) {
      for (const sub of this.surfaceSubs.values()) {
        this.transport.send(buildSurfaceUnsubscribeMessage(sub.surfaceId));
        sub.lastSent = null;
        this.forgetSentSurfaceViewSize(sub.surfaceId);
      }
    }
  }

  /** Suspend video while the document is hidden without overwriting the
   *  user's persistent streaming preference. The live view registry stays
   *  intact, so becoming visible restores exactly the previous streams. */
  private setPageVisible(visible: boolean): void {
    if (this.pageVisible === visible) return;
    this.pageVisible = visible;
    if (
      this.transport.status !== "connected" ||
      !this.surfaceStreamingEnabled
    ) {
      return;
    }
    if (visible) {
      for (const sub of this.surfaceSubs.values()) {
        sub.lastSent = null;
        this.maybeSendSurfaceSubscribe(sub);
      }
    } else {
      for (const sub of this.surfaceSubs.values()) {
        if (sub.views.size > 0) {
          this.transport.send(buildSurfaceUnsubscribeMessage(sub.surfaceId));
          this.forgetSentSurfaceViewSize(sub.surfaceId);
        }
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

  sendClipboard(mimeType: string, data: Uint8Array): void {
    if (this.transport.status !== "connected") return;
    this.waylandClipboardOwned = false;
    this.waylandClipboardText = null;
    this.rejectPendingClipboardRequests(
      connectionError("Wayland clipboard ownership changed"),
    );
    this.transport.send(buildClipboardMessage(mimeType, data));
  }

  /** True when Ctrl/Cmd+V must preserve the compositor's current selection.
   *  A Wayland-owned selection can carry several representations and is
   *  spliced directly from its source to the destination client. */
  usesWaylandClipboard(): boolean {
    return this.waylandClipboardOwned === true;
  }

  /** Read text from the compositor's live Wayland selection.
   *
   * A Wayland copy is mirrored to `navigator.clipboard` as a convenience,
   * but browsers can reject that background write.  Terminal paste therefore
   * consumes the in-connection mirror directly and asks the compositor when
   * this client connected after the copy or the eager mirror did not arrive.
   */
  async readWaylandClipboardText(): Promise<string | null> {
    if (
      this.waylandClipboardOwned !== true ||
      this.transport.status !== "connected"
    ) {
      return null;
    }
    if (this.waylandClipboardText !== null) {
      return this.waylandClipboardText || null;
    }

    try {
      const mimes = await this.requestClipboardMimes();
      if (this.waylandClipboardOwned !== true) return null;
      const preferred = [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
      ];
      const mime =
        preferred.find((candidate) => mimes.includes(candidate)) ??
        mimes.find((candidate) =>
          candidate.toLowerCase().startsWith("text/plain"),
        );
      if (!mime) return null;
      const data = await this.requestClipboardContent(mime);
      if (this.waylandClipboardOwned !== true || data.length === 0) return null;
      const text = textDecoder.decode(data);
      this.waylandClipboardText = text;
      return text || null;
    } catch {
      return null;
    }
  }

  private requestClipboardMimes(): Promise<string[]> {
    if (this.pendingClipboardList) return this.pendingClipboardList.promise;
    if (this.transport.status !== "connected") {
      return Promise.reject(connectionError("Clipboard is disconnected"));
    }
    let resolve!: (value: string[]) => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<string[]>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    let pending!: PendingClipboardRequest<string[]>;
    const timer = setTimeout(() => {
      if (this.pendingClipboardList !== pending) return;
      this.pendingClipboardList = null;
      reject(connectionError("Clipboard MIME request timed out"));
    }, CLIPBOARD_REQUEST_TIMEOUT_MS);
    pending = { promise, resolve, reject, timer };
    this.pendingClipboardList = pending;
    this.transport.send(buildClipboardListMessage());
    return promise;
  }

  private requestClipboardContent(mime: string): Promise<Uint8Array> {
    const existing = this.pendingClipboardGets.get(mime);
    if (existing) return existing.promise;
    if (this.transport.status !== "connected") {
      return Promise.reject(connectionError("Clipboard is disconnected"));
    }
    let resolve!: (value: Uint8Array) => void;
    let reject!: (error: Error) => void;
    const promise = new Promise<Uint8Array>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    let pending!: PendingClipboardRequest<Uint8Array>;
    const timer = setTimeout(() => {
      if (this.pendingClipboardGets.get(mime) !== pending) return;
      this.pendingClipboardGets.delete(mime);
      reject(connectionError("Clipboard content request timed out"));
    }, CLIPBOARD_REQUEST_TIMEOUT_MS);
    pending = { promise, resolve, reject, timer };
    this.pendingClipboardGets.set(mime, pending);
    this.transport.send(buildClipboardGetMessage(mime));
    return promise;
  }

  private rejectPendingClipboardRequests(error: Error): void {
    const list = this.pendingClipboardList;
    if (list) {
      this.pendingClipboardList = null;
      clearTimeout(list.timer);
      list.reject(error);
    }
    for (const pending of this.pendingClipboardGets.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.pendingClipboardGets.clear();
  }

  /** The host clipboard may be newer (clipboardchange, window/tab loss, or a
   *  real DOM copy/cut).  The next paste probes the browser clipboard and
   *  publishes it to the compositor before forwarding V. */
  noteBrowserClipboardMayHaveChanged(): void {
    this.waylandClipboardOwned = null;
    this.waylandClipboardText = null;
    this.rejectPendingClipboardRequests(
      connectionError("Browser clipboard may be newer"),
    );
  }

  /** Expect the text-only clipboardchange caused by mirroring a Wayland
   *  selection into the host clipboard.  That write must not make us forget
   *  the richer, client-owned Wayland source. */
  private expectMirroredClipboardChange(): number | null {
    if (!this.clipboardChangeTarget || this.waylandClipboardOwned !== true) {
      return null;
    }
    const token = ++this.clipboardMirrorToken;
    const timer = setTimeout(() => {
      this.pendingClipboardMirrors.delete(token);
    }, 1_000);
    this.pendingClipboardMirrors.set(token, timer);
    return token;
  }

  private finishMirroredClipboardChange(token: number | null): void {
    if (token === null) return;
    const timer = this.pendingClipboardMirrors.get(token);
    if (timer === undefined) return;
    clearTimeout(timer);
    this.pendingClipboardMirrors.delete(token);
  }

  private clearPendingClipboardMirrors(): void {
    for (const timer of this.pendingClipboardMirrors.values()) {
      clearTimeout(timer);
    }
    this.pendingClipboardMirrors.clear();
  }

  private consumeMirroredClipboardChange(event: Event): boolean {
    if (this.pendingClipboardMirrors.size === 0) return false;
    const types = (event as Event & { readonly types?: readonly string[] })
      .types;
    // writeText() exposes only text/plain.  A screenshot is image/png, so it
    // must invalidate immediately even if it closely follows our own mirror.
    const textOnly =
      types !== undefined &&
      types.length !== 0 &&
      types.every(
        (type) => type === "text/plain" || type.startsWith("text/plain;"),
      );
    if (!textOnly) {
      this.clearPendingClipboardMirrors();
      return false;
    }
    const token = this.pendingClipboardMirrors.keys().next().value;
    if (token === undefined) return false;
    this.finishMirroredClipboardChange(token);
    return true;
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
    // Disconnects and malformed over-sized sequences release the allocation;
    // successful messages only reset fragmentBytes and retain capacity.
    this.fragmentBuffer = new Uint8Array(0);
    this.fragmentBytes = 0;
  }

  /** Grow the fragment accumulator geometrically. In steady state the first
   *  large frame sizes it and later frames require no reassembly allocation. */
  private ensureFragmentCapacity(required: number): void {
    if (required <= this.fragmentBuffer.length) return;
    let capacity = Math.max(4 * 1024, this.fragmentBuffer.length);
    while (capacity < required) {
      capacity = Math.min(capacity * 2, FS_MAX_DECOMPRESSED);
    }
    const grown = new Uint8Array(capacity);
    grown.set(this.fragmentBuffer.subarray(0, this.fragmentBytes));
    this.fragmentBuffer = grown;
  }

  private handleMessage = (data: BlitTransportMessage): void => {
    const bytes = data instanceof Uint8Array ? data : new Uint8Array(data);
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
      // bound. The Rust reader has always had this guard; the browser did not.
      if (this.fragmentBytes + chunk.length > FS_MAX_DECOMPRESSED) {
        this.resetFragmentReassembly();
        return;
      }
      // Transport callbacks may expose a borrowed view into a reusable BYOB
      // or decoder buffer. Copy it now into one reusable accumulator; keeping
      // the view itself would let the next read overwrite this fragment.
      const nextBytes = this.fragmentBytes + chunk.length;
      this.ensureFragmentCapacity(nextBytes);
      this.fragmentBuffer.set(chunk, this.fragmentBytes);
      this.fragmentBytes = nextBytes;
      if (flags & FRAGMENT_FLAG_LAST) {
        // handleMessage and all synchronous parsers honor the transport's
        // borrowed-view contract. Promise results that escape it take their
        // own copy at the resolve site below.
        const reassembled = this.fragmentBuffer.subarray(0, this.fragmentBytes);
        this.fragmentBytes = 0;
        this.handleMessage(reassembled);
      }
      return;
    }

    if (type !== S2C_QUIT && type !== S2C_HELLO && type !== S2C_READY) {
      this.noteServerResponsive(true);
    }

    switch (type) {
      case S2C_PING:
        if (bytes.length >= 9) {
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
          const nonce = view.getUint32(1, true);
          const serverMs = view.getUint32(5, true);
          const sentAt = this.pendingClockPings.get(nonce);
          if (sentAt !== undefined) {
            this.pendingClockPings.delete(nonce);
            this.surfaceStore.noteServerClock(
              serverMs,
              sentAt,
              performance.now(),
            );
          }
        }
        return;
      case S2C_CLIENT_LIST: {
        if (bytes.length < 3) return;
        const view = new DataView(
          bytes.buffer,
          bytes.byteOffset,
          bytes.byteLength,
        );
        const nonce = view.getUint16(1, true);
        const pending = this.pendingClientLists.get(nonce);
        const watched = this.clientCatalogWatchNonce === nonce;
        if (!pending && !watched) return;
        const malformed = (): void => {
          const error = connectionError("Malformed client catalog response");
          if (pending) {
            this.pendingClientLists.delete(nonce);
            pending.reject(error);
          }
          if (watched) {
            for (const subscriber of this.clientCatalogSubscribers) {
              subscriber.onError?.(error);
            }
          }
        };
        if (bytes.length < 15) {
          malformed();
          return;
        }
        const count = view.getUint32(11, true);
        let offset = 15;
        // Every record needs a 30-byte header, before subscriptions.
        if (count > Math.floor((bytes.length - offset) / 30)) {
          malformed();
          return;
        }
        const clients: BlitClientList["clients"][number][] = [];
        for (let i = 0; i < count; i++) {
          if (offset + 30 > bytes.length) {
            malformed();
            return;
          }
          const id = view.getBigUint64(offset, true);
          const ageSeconds = Number(view.getBigUint64(offset + 8, true));
          const outboundBytesPerSecond = Number(
            view.getBigUint64(offset + 16, true),
          );
          const terminalCount = view.getUint16(offset + 24, true);
          const surfaceCount = view.getUint16(offset + 26, true);
          const subscriptionCount = view.getUint16(offset + 28, true);
          offset += 30;
          if (
            offset +
              terminalCount * 6 +
              surfaceCount * 8 +
              subscriptionCount * 3 >
            bytes.length
          ) {
            malformed();
            return;
          }
          const terminals = [];
          for (let j = 0; j < terminalCount; j++) {
            const ptyId = view.getUint16(offset, true);
            const rows = view.getUint16(offset + 2, true);
            const cols = view.getUint16(offset + 4, true);
            terminals.push({
              ptyId,
              rows: rows === 0 ? null : rows,
              cols: cols === 0 ? null : cols,
            });
            offset += 6;
          }
          const surfaces = [];
          for (let j = 0; j < surfaceCount; j++) {
            const surfaceId = view.getUint16(offset, true);
            const width = view.getUint16(offset + 2, true);
            const height = view.getUint16(offset + 4, true);
            const scale120 = view.getUint16(offset + 6, true);
            surfaces.push({
              surfaceId,
              width: width === 0 ? null : width,
              height: height === 0 ? null : height,
              scale120: scale120 === 0 ? null : scale120,
            });
            offset += 8;
          }
          const subscriptions = [];
          for (let j = 0; j < subscriptionCount; j++) {
            subscriptions.push({
              kind: bytes[offset],
              id: view.getUint16(offset + 1, true),
            });
            offset += 3;
          }
          clients.push({
            id,
            ageSeconds,
            outboundBytesPerSecond,
            subscriptions,
            terminals,
            surfaces,
          });
        }
        if (offset !== bytes.length) {
          malformed();
          return;
        }
        const catalog = { selfId: view.getBigUint64(3, true), clients };
        if (pending) {
          this.pendingClientLists.delete(nonce);
          pending.resolve(catalog);
        }
        if (watched) {
          this.lastClientCatalog = catalog;
          for (const subscriber of this.clientCatalogSubscribers) {
            subscriber.listener(catalog);
          }
        }
        return;
      }
      case S2C_KICK_RESULT: {
        if (bytes.length < 3) return;
        const nonce = bytes[1] | (bytes[2] << 8);
        // This is the whole family's status reply, not just the kick's: the
        // server answers a malformed LIST/WATCH/UNWATCH with it too, under the
        // sender's nonce. Nonces are unique across all three pending maps, so
        // one lookup order settles whichever request it belongs to — without
        // this, a refused list request would hang until its caller gave up.
        // A refused unwatch is the one member of the family with nothing left
        // to settle — release its nonce and stop.
        if (this.retiredWatchNonce === nonce) {
          this.retiredWatchNonce = null;
          return;
        }
        const pendingKick = this.pendingClientKicks.get(nonce);
        const pendingList = this.pendingClientLists.get(nonce);
        const watched = this.clientCatalogWatchNonce === nonce;
        if (!pendingKick && !pendingList && !watched) return;
        this.pendingClientKicks.delete(nonce);
        this.pendingClientLists.delete(nonce);
        const status = bytes.length < 4 ? null : bytes[3];
        const detail =
          bytes.length < 4 ? "" : textDecoder.decode(bytes.subarray(4)).trim();
        // Only a kick has an "OK" form. An OK under a list or watch nonce is
        // the server contradicting itself, and silently keeping a watch the
        // server just refused would leave the catalog frozen with no error.
        const error =
          status === STATUS_OK
            ? connectionError("Client control replied OK to a catalog request")
            : connectionError(
                status === null
                  ? "Malformed kick result"
                  : detail || `Client control failed: ${statusText(status)}`,
              );
        if (status === STATUS_OK) {
          pendingKick?.resolve();
        } else {
          pendingKick?.reject(error);
        }
        pendingList?.reject(error);
        if (watched) {
          this.clientCatalogWatchNonce = null;
          this.lastClientCatalog = null;
          for (const subscriber of this.clientCatalogSubscribers) {
            subscriber.onError?.(error);
          }
        }
        return;
      }
      case S2C_KICKED: {
        const reason = textDecoder.decode(bytes.subarray(1)).trim();
        this.lastError = `kicked: ${reason || "kicked by another client"}`;
        // A kick suppresses automatic retry so two duplicate clients do not
        // fight forever, but it must not dispose the transport: Reconnect is
        // an explicit user choice and remains available.
        if (this.transport.suspend) this.transport.suspend();
        else this.transport.close();
        return;
      }
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
        this.resetClientControl(connectionError("Server is shutting down"));
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
      case S2C_TRAY_UPDATE: {
        if (!this.desktopStore.handleTrayUpdate(bytes)) {
          this._logger.warn(`${this.id}: malformed TRAY_UPDATE`);
        }
        return;
      }
      case S2C_TRAY_MENU: {
        if (!this.desktopStore.handleTrayMenu(bytes)) {
          this._logger.warn(`${this.id}: malformed TRAY_MENU`);
        }
        return;
      }
      case S2C_NOTIFICATION_UPDATE: {
        if (!this.desktopStore.handleNotificationUpdate(bytes)) {
          this._logger.warn(`${this.id}: malformed NOTIFICATION_UPDATE`);
        }
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
        this.syncSurfaceTouchCapability();
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
          supportsSurfaceTouch: (features & FEATURE_SURFACE_TOUCH) !== 0,
          supportsSurfaceTextInput:
            (features & FEATURE_SURFACE_TEXT_INPUT) !== 0,
          supportsAudio: (features & FEATURE_AUDIO) !== 0,
          supportsClientControl: (features & FEATURE_CLIENT_CONTROL) !== 0,
          supportsFsSync: (features & FEATURE_FS) !== 0,
          supportsGit: (features & FEATURE_GIT) !== 0,
          supportsLsp: (features & FEATURE_LSP) !== 0,
          supportsKv: (features & FEATURE_KV) !== 0,
          supportsDesktop: (features & FEATURE_DESKTOP) !== 0,
          bootGeneration,
          serverVersion,
        };
        this.emit();
        this.surfaceStore.reset();
        this.audioPlayer.reset();
        this.desktopStore.reset();
        this.resetSurfaceSubsForReconnect();
        // Fs syncs do not survive a server session change: old sync_ids
        // are meaningless on the new session.
        this.resetFsSyncs(connectionError("Connection re-established"));
        this.resetClientControl(
          connectionError("Connection re-established"),
          false,
        );
        this.startClientCatalogWatch();
        this.resetGitRepos(connectionError("Connection re-established"));
        this.resetLspAttachments(connectionError("Connection re-established"));
        this.resetKv(connectionError("Connection re-established"));
        this.resetFragmentReassembly();
        // Pushed cwds belong to the old server session's ptys.
        this.termCwds.clear();
        if (features & FEATURE_DESKTOP) {
          this.desktopStore.subscribeDesktop();
        }
        return;
      }
      case S2C_SURFACE_CREATED: {
        try {
          if (bytes.length < 11) return;
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
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
        // Base layout: [type][sid 2][timestamp 4][flags 1][w 2][h 2][data…]
        // Precise layout appends [sub_us 2] before data and marks flag bit 3.
        if (bytes.length < 12) return;
        const surfaceId = bytes[1] | (bytes[2] << 8);
        const timestamp =
          (bytes[3] | (bytes[4] << 8) | (bytes[5] << 16) | (bytes[6] << 24)) >>>
          0;
        const flags = bytes[7];
        const width = bytes[8] | (bytes[9] << 8);
        const height = bytes[10] | (bytes[11] << 8);
        const hasSubUs = (flags & SURFACE_FRAME_FLAG_TIMESTAMP_SUB_US) !== 0;
        if (hasSubUs && bytes.length < 14) return;
        const timestampSubUs = hasSubUs ? bytes[12] | (bytes[13] << 8) : 0;
        const dataOffset = hasSubUs ? 14 : 12;
        try {
          // The store sends ACKs itself, deferring them when the decode
          // queue is deep to apply backpressure on the server.
          this.surfaceStore.handleSurfaceFrame(
            surfaceId,
            timestamp,
            flags,
            width,
            height,
            bytes.subarray(dataOffset),
            timestampSubUs,
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
            this.surfaceStore.handleSurfaceCursor(surfaceId, shape, {
              kind: "named",
              name: shape,
            });
          } else if (cursorType === 1) {
            // Hidden
            this.surfaceStore.handleSurfaceCursor(surfaceId, "none", {
              kind: "hidden",
            });
          } else if (cursorType === 2) {
            // Custom image: hotx(2) + hoty(2) + w(2) + h(2) + png
            if (bytes.length < 12) return;
            const view = new DataView(
              bytes.buffer,
              bytes.byteOffset,
              bytes.byteLength,
            );
            const hotX = view.getUint16(4, true);
            const hotY = view.getUint16(6, true);
            const width = view.getUint16(8, true);
            const height = view.getUint16(10, true);
            if (width === 0 || height === 0) return;
            const pngData = bytes.subarray(12);
            const blob = new Blob([new Uint8Array(pngData)], {
              type: "image/png",
            });
            const url = URL.createObjectURL(blob);
            this.surfaceStore.handleSurfaceCursor(
              surfaceId,
              `url(${url}) ${hotX} ${hotY}, auto`,
              {
                kind: "custom",
                url,
                hotspotX: hotX,
                hotspotY: hotY,
                width,
                height,
              },
            );
          }
        } catch {}
        return;
      }
      case S2C_SURFACE_REMOTE_INPUT: {
        // Layout: [type][sid:2][kind:1][count:1][x:2,y:2]*.
        if (bytes.length < 5) return;
        const view = new DataView(
          bytes.buffer,
          bytes.byteOffset,
          bytes.byteLength,
        );
        const surfaceId = view.getUint16(1, true);
        const kindByte = bytes[3]!;
        const count = bytes[4]!;
        if (bytes.length !== 5 + count * 4) return;
        // `kind` matters even at count 0: a retire withdraws only its own kind,
        // so a lifted finger must not erase that viewer's live cursor.
        if (
          kindByte !== REMOTE_INPUT_POINTER &&
          kindByte !== REMOTE_INPUT_TOUCH
        )
          return;
        const points: { x: number; y: number }[] = [];
        for (let i = 0; i < count; i++) {
          points.push({
            x: view.getUint16(5 + i * 4, true),
            y: view.getUint16(7 + i * 4, true),
          });
        }
        this.surfaceStore.handleRemoteInput(
          surfaceId,
          kindByte === REMOTE_INPUT_TOUCH ? "touch" : "pointer",
          points,
        );
        return;
      }
      case S2C_SURFACE_ENCODER: {
        try {
          // Layout: [type][sid 2][name + 0 + codec_str]
          if (bytes.length < 3) return;
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
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
      case S2C_SURFACE_ACTIVATED: {
        try {
          if (bytes.length < 3) return;
          const surfaceId = bytes[1] | (bytes[2] << 8);
          this.surfaceStore.handleSurfaceActivated(surfaceId);
        } catch {}
        return;
      }
      case S2C_SURFACE_TEXT_INPUT: {
        if (bytes.length < 12) return;
        const view = new DataView(
          bytes.buffer,
          bytes.byteOffset,
          bytes.byteLength,
        );
        const surfaceId = view.getUint16(1, true);
        const flags = bytes[3]!;
        this.surfaceStore.handleSurfaceTextInput(surfaceId, {
          enabled: (flags & SURFACE_TEXT_INPUT_ENABLED) !== 0,
          requested: (flags & SURFACE_TEXT_INPUT_REQUESTED) !== 0,
          hint: view.getUint32(4, true),
          purpose: view.getUint32(8, true),
        });
        return;
      }
      case S2C_SURFACE_RESIZED: {
        try {
          if (bytes.length < 7) return;
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
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
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
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
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
          const mimeLen = view.getUint16(1, true);
          if (bytes.length < 3 + mimeLen + 4) return;
          const mimeType = textDecoder.decode(bytes.subarray(3, 3 + mimeLen));
          const dataLen = view.getUint32(3 + mimeLen, true);
          const dataStart = 7 + mimeLen;
          if (bytes.length < dataStart + dataLen) return;
          const data = bytes.subarray(dataStart, dataStart + dataLen);
          const pending = this.pendingClipboardGets.get(mimeType);
          if (pending) {
            this.pendingClipboardGets.delete(mimeType);
            clearTimeout(pending.timer);
            // Transport adapters may hand us a borrowed/reused receive view;
            // the awaiting paste resumes on a later microtask.
            pending.resolve(data.slice());
          }
          if (mimeType.startsWith("text/") || mimeType === "UTF8_STRING") {
            const text = textDecoder.decode(data);
            if (this.waylandClipboardOwned === true) {
              this.waylandClipboardText = text;
            }
            const mirrorToken = this.expectMirroredClipboardChange();
            try {
              navigator.clipboard
                .writeText(text)
                .catch(() => this.finishMirroredClipboardChange(mirrorToken));
            } catch {
              this.finishMirroredClipboardChange(mirrorToken);
            }
          }
        } catch {}
        return;
      }
      case S2C_CLIPBOARD_LIST: {
        try {
          if (bytes.length < 3) return;
          const view = new DataView(
            bytes.buffer,
            bytes.byteOffset,
            bytes.byteLength,
          );
          const count = view.getUint16(1, true);
          const mimes: string[] = [];
          let offset = 3;
          for (let i = 0; i < count; i++) {
            if (offset + 2 > bytes.length) return;
            const len = view.getUint16(offset, true);
            offset += 2;
            if (offset + len > bytes.length) return;
            mimes.push(
              textDecoder.decode(bytes.subarray(offset, offset + len)),
            );
            offset += len;
          }
          const pending = this.pendingClipboardList;
          if (pending) {
            this.pendingClipboardList = null;
            clearTimeout(pending.timer);
            pending.resolve(mimes);
          }
        } catch {}
        return;
      }
      case S2C_CLIPBOARD_OWNER: {
        if (bytes.length !== 2 || bytes[1] > 1) return;
        const wayland = bytes[1] !== 0;
        this.rejectPendingClipboardRequests(
          connectionError("Wayland clipboard ownership changed"),
        );
        this.waylandClipboardOwned = wayland;
        // Every true announcement corresponds to a fresh SetSelection, even
        // when the previous owner was also a Wayland client.  Do not let its
        // cached text leak into the new selection while the content follows.
        this.waylandClipboardText = null;
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
      case S2C_FS_UPLOAD_BEGIN: {
        const parsed = parseFsUploadBeginReply(bytes);
        if (!parsed) return;
        const pending = this.pendingFsUploadBegins.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsUploadBegins.delete(parsed.nonce);
        if (parsed.status === FS_DONE_OK) {
          pending.resolve(parsed.uploadId);
        } else if (parsed.status === FS_DONE_CONFLICT) {
          // Same conflict shape as fs writes: `hash` carries the current
          // on-disk hash so the caller rebases without a round trip.
          pending.reject(new FsConflictError(parsed.hash));
        } else {
          pending.reject(
            connectionError(
              `Upload failed: ${fsDoneStatusText(parsed.status)}`,
            ),
          );
        }
        return;
      }
      case S2C_FS_UPLOAD_CHUNK: {
        const parsed = parseFsUploadChunkAck(bytes);
        if (!parsed) return;
        this.pendingFsUploads
          .get(parsed.uploadId)
          ?.ack(parsed.status, parsed.received);
        return;
      }
      case S2C_FS_UPLOAD_FINISH: {
        const parsed = parseFsUploadFinishReply(bytes);
        if (!parsed) return;
        const pending = this.pendingFsUploadFinishes.get(parsed.nonce);
        if (!pending) return;
        this.pendingFsUploadFinishes.delete(parsed.nonce);
        if (parsed.status === FS_DONE_OK) {
          // Record the hash for self-echo suppression, as `fsWrite` does.
          if (pending.record) {
            pending.record.consumer.lastWritten.set(
              pending.record.path,
              parsed.hash,
            );
          }
          pending.resolve({
            hash: parsed.hashBytes,
            hashU128: parsed.hash,
            mtime: Number(parsed.mtimeNs),
            mtimeNs: parsed.mtimeNs,
          });
        } else if (parsed.status === FS_DONE_CONFLICT) {
          // The precondition held at BEGIN but the file changed during the
          // upload; same conflict shape as fs writes.
          pending.reject(new FsConflictError(parsed.hash));
        } else {
          pending.reject(
            connectionError(
              `Upload failed: ${fsDoneStatusText(parsed.status)}`,
            ),
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
        if (!pending.abandoned) pending.resolve(bytes.slice());
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
        pending.resolve(bytes.slice());
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
      this.pendingClockPings.clear();
      this.surfaceStore.clearServerClock();
      this.sendClockPing();
      // Start application-level keepalive.
      if (this.pingTimer === null && this.pingIntervalMs > 0) {
        this.pingTimer = setInterval(() => {
          this.sendClockPing();
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
      this.waylandClipboardOwned = null;
      this.waylandClipboardText = null;
      this.rejectPendingClipboardRequests(
        connectionError(`Transport ${status}`),
      );
      if (this.pingTimer !== null) {
        clearInterval(this.pingTimer);
        this.pingTimer = null;
      }
      this.pendingClockPings.clear();
      this.surfaceStore.clearServerClock();
      this.rejectPendingCreates(
        connectionError(`Transport ${status} before PTY creation completed`),
      );
      this.rejectPendingSearches(connectionError(`Transport ${status}`));
      this.rejectPendingReads(connectionError(`Transport ${status}`));
      this.resetClientControl(connectionError(`Transport ${status}`));
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
      this.desktopStore.reset();
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

  private sendClockPing(): void {
    if (this.transport.status !== "connected") return;
    this.clockPingNonce = (this.clockPingNonce + 1) >>> 0;
    const message = new Uint8Array(5);
    message[0] = C2S_PING;
    new DataView(message.buffer).setUint32(1, this.clockPingNonce, true);
    this.pendingClockPings.set(this.clockPingNonce, performance.now());
    while (this.pendingClockPings.size > 4) {
      const oldest = this.pendingClockPings.keys().next().value;
      if (oldest === undefined) break;
      this.pendingClockPings.delete(oldest);
    }
    this.transport.send(message);
  }

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
