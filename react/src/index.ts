export { BlitTerminal } from "./BlitTerminal";
export type { BlitTerminalHandle } from "./BlitTerminal";

export { useBlitConnection } from "./hooks/useBlitConnection";
export type {
  BlitConnectionCallbacks,
  PtyListEntry,
  SearchResult,
} from "./hooks/useBlitConnection";
export {
  SEARCH_SOURCE_TITLE,
  SEARCH_SOURCE_VISIBLE,
  SEARCH_SOURCE_SCROLLBACK,
  SEARCH_MATCH_TITLE,
  SEARCH_MATCH_VISIBLE,
  SEARCH_MATCH_SCROLLBACK,
} from "./hooks/useBlitConnection";

export { useBlitSessions } from "./hooks/useBlitSessions";
export type {
  UseBlitSessionsOptions,
  UseBlitSessionsReturn,
  UseBlitSessionsFn,
} from "./hooks/useBlitSessions";

export { useBlitTerminal, measureCell } from "./hooks/useBlitTerminal";
export type {
  CellMetrics,
  UseBlitTerminalOptions,
} from "./hooks/useBlitTerminal";

export { WebSocketTransport } from "./transports/websocket";
export type { WebSocketTransportOptions } from "./transports/websocket";

export { createWebRtcDataChannelTransport } from "./transports/webrtc";
export type { WebRtcDataChannelTransportOptions } from "./transports/webrtc";

export {
  DEFAULT_FONT,
  DEFAULT_FONT_SIZE,
} from "./types";
export type {
  BlitTransport,
  BlitTransportEventMap,
  BlitTerminalProps,
  BlitSession,
  ConnectionStatus,
  TerminalPalette,
} from "./types";

export { PALETTES } from "./palettes";

export { TerminalStore } from "./TerminalStore";
export type { TerminalDirtyListener } from "./TerminalStore";
