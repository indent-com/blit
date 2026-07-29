export { BlitWorkspace, consoleLogger, nullLogger } from "./BlitWorkspace";
export type { BlitLogger } from "./BlitWorkspace";

export {
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
} from "./BlitConnection";

export type { BlitWasmModule } from "./TerminalStore";
export { AudioPlayer } from "./AudioPlayer";
export { SurfaceStore } from "./SurfaceStore";
export type {
  SurfaceFrameCallback,
  SurfaceEventCallback,
  SurfaceFrameSample,
} from "./SurfaceStore";

export { measureCell, cssFontFamily } from "./measure";
export type { CellMetrics } from "./measure";

export { WebSocketTransport } from "./transports/websocket";
export { WebTransportTransport } from "./transports/webtransport";
export { createShareTransport } from "./transports/webrtc-share";
export { MuxTransport, MuxChannel } from "./transports/mux";

/** TCP and UDP relay (docs/design/net.md). */
export * from "./net";
/** HTTP/1.1 over a relayed stream, for the preview service worker. */
export * from "./http1";
/** Preview targets and the /x/ bootstrap prefix. */
export * from "./preview";

export { DEFAULT_FONT, DEFAULT_FONT_SIZE, DEFAULT_TEXT_GAMMA } from "./types";

export {
  EXIT_STATUS_UNKNOWN,
  exitCodeFromStatus,
  formatExitStatus,
} from "./exit-status";

export {
  FEATURE_FS,
  FS_GREP_CASE_SENSITIVE,
  FS_GREP_REGEX,
  FS_GREP_TRUNCATED,
  FS_GREP_FILE_IGNORED,
  FS_SYNC_RECURSIVE,
  FS_SYNC_CONTENT,
  FS_SYNC_CROSS_FILESYSTEM,
  FS_STATUS_OK,
  FS_STATUS_NOT_FOUND,
  FS_STATUS_PERMISSION_DENIED,
  FS_STATUS_RESOURCE_LIMIT,
  FS_STATUS_OTHER,
  FS_CLOSED_CLIENT_REQUEST,
  FS_CLOSED_ROOT_GONE,
  FS_CLOSED_PERMISSION_LOST,
  FS_CLOSED_BACKEND_FAILED,
  FS_CLOSED_RESOURCE_LIMIT,
  FS_CLOSED_CONNECTION_LOST,
  FS_ENTRY_TYPE_MASK,
  FS_ENTRY_FILE,
  FS_ENTRY_DIR,
  FS_ENTRY_SYMLINK,
  FS_ENTRY_OTHER,
  FS_ENTRY_UNREADABLE,
  FS_ENTRY_NO_CONTENT,
  FS_ENTRY_UNSTABLE,
  FsMirror,
  applyFsDelta,
  fsCompress,
  FsConflictError,
} from "./fs";
export type {
  FsNode,
  FsRecord,
  FsContent,
  FsApplyResult,
  FsFileIndex,
  FsGrepResult,
  FsGrepFile,
  FsGrepOptions,
  FsSyncOptions,
  FsSyncHandle,
} from "./fs";
export * from "./git";
export * from "./lsp";
export * from "./kv";
export { Notifier } from "./reactive";
export type { ReactiveStore } from "./reactive";

export type {
  BlitConnectionSnapshot,
  BlitDebug,
  BlitSearchResult,
  BlitSurface,
  BlitWorkspaceSnapshot,
  BlitTransport,
  BlitSession,
  ConnectionId,
  ConnectionStatus,
  CopyRangeResult,
  SessionId,
  TerminalPalette,
  TransportConfig,
} from "./types";

export {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_UP,
  SURFACE_POINTER_MOVE,
} from "./protocol";

export { PALETTES } from "./palettes";

export { MOUSE_DOWN, MOUSE_UP, MOUSE_MOVE } from "./protocol";
export { keyToBytes, ctrlCharToByte, encoder } from "./keyboard";

export type { GlRenderer, RendererBackend } from "./gl-renderer";
export { createWebGpuRenderer } from "./webgpu-renderer";

export { BlitTerminalSurface } from "./BlitTerminalSurface";
export type {
  BlitTerminalSurfaceOptions,
  BlitTerminalSurfaceHandle,
} from "./BlitTerminalSurface";

export {
  BlitSurfaceCanvas,
  detectCodecSupport,
  getCodecSupport,
} from "./BlitSurfaceCanvas";
export type { BlitSurfaceCanvasOptions } from "./BlitSurfaceCanvas";

export { parseDSL, serializeDSL, leafCount } from "./bsp/dsl";
export type { BSPNode, BSPSplit, BSPChild, BSPLeaf } from "./bsp/dsl";

export {
  PRESETS,
  enumeratePanes,
  assignSessionsToPanes,
  buildCandidateOrder,
  reconcileAssignments,
  adjustWeights,
  layoutFromDSL,
} from "./bsp/layout";
export type { BSPLayout, BSPPane, BSPAssignments } from "./bsp/layout";
