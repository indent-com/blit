export { BlitWorkspace } from "./BlitWorkspace";

export {
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
} from "./BlitConnection";

export type { BlitWasmModule } from "./TerminalStore";
export { SurfaceStore } from "./SurfaceStore";
export type {
  SurfaceFrameCallback,
  SurfaceEventCallback,
} from "./SurfaceStore";

export { measureCell, cssFontFamily } from "./measure";
export type { CellMetrics } from "./measure";

export { WebSocketTransport } from "./transports/websocket";
export { WebTransportTransport } from "./transports/webtransport";
export { createShareTransport } from "./transports/webrtc-share";

export { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
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
export { keyToBytes, encoder } from "./keyboard";

export type { GlRenderer } from "./gl-renderer";

export { BlitTerminalSurface } from "./BlitTerminalSurface";
export type {
  BlitTerminalSurfaceOptions,
  BlitTerminalSurfaceHandle,
} from "./BlitTerminalSurface";

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
