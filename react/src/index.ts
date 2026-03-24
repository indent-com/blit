export { BlitTerminal } from './BlitTerminal';
export type { BlitTerminalHandle } from './BlitTerminal';

export { useBlitConnection } from './hooks/useBlitConnection';
export type { BlitConnectionCallbacks, PtyListEntry, SearchResult } from './hooks/useBlitConnection';

export { useBlitSessions } from './hooks/useBlitSessions';
export type { UseBlitSessionsOptions } from './hooks/useBlitSessions';

export { useBlitTerminal, measureCell } from './hooks/useBlitTerminal';
export type { CellMetrics, UseBlitTerminalOptions } from './hooks/useBlitTerminal';

export { WebSocketTransport } from './transports/websocket';
export type { WebSocketTransportOptions } from './transports/websocket';

export { createWebRtcDataChannelTransport } from './transports/webrtc';
export type { WebRtcDataChannelTransportOptions } from './transports/webrtc';

export type {
  BlitTransport,
  BlitTransportEventMap,
  BlitTerminalProps,
  BlitSession,
  ConnectionStatus,
  TerminalPalette,
} from './types';

export { PALETTES } from './palettes';
