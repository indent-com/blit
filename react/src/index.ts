export { BlitTerminal } from "./BlitTerminal";
export type { BlitTerminalHandle } from "./BlitTerminal";

export { createBlitWorkspace, BlitWorkspace } from "./BlitWorkspace";
export type {
  AddBlitConnectionOptions,
  CreateBlitWorkspaceOptions,
  CreateWorkspaceSessionOptions,
} from "./BlitWorkspace";
export {
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
} from "./BlitConnection";

export { useBlitConnection } from "./hooks/useBlitConnection";
export { useBlitSessions } from "./hooks/useBlitSessions";
export { useBlitWorkspace, useBlitWorkspaceState } from "./hooks/useBlitWorkspace";
export { useBlitFocusedSession } from "./hooks/useBlitSession";

export { measureCell, CSS_GENERIC } from "./hooks/useBlitTerminal";
export type { CellMetrics } from "./hooks/useBlitTerminal";

export { WebSocketTransport } from "./transports/websocket";
export type { WebSocketTransportOptions } from "./transports/websocket";

export { WebTransportTransport } from "./transports/webtransport";
export type { WebTransportTransportOptions } from "./transports/webtransport";

export { createWebRtcDataChannelTransport } from "./transports/webrtc";
export type { WebRtcDataChannelTransportOptions } from "./transports/webrtc";

export {
  DEFAULT_FONT,
  DEFAULT_FONT_SIZE,
} from "./types";
export type {
  BlitConnectionSnapshot,
  BlitSearchResult,
  BlitWorkspaceSnapshot,
  BlitTransport,
  BlitSession,
  ConnectionId,
  ConnectionStatus,
  SessionId,
  TerminalPalette,
} from "./types";

export { PALETTES } from "./palettes";

export type { BlitWasmModule } from "./TerminalStore";

export {
  BlitWorkspaceProvider,
  useBlitContext,
} from "./BlitContext";
export type { BlitContextValue, BlitProviderProps } from "./BlitContext";
