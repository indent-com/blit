import type React from "react";

/** A terminal color palette. */
export interface TerminalPalette {
  id: string;
  name: string;
  /** true = dark background, false = light background. */
  dark: boolean;
  /** Default foreground color as [r, g, b] (0–255). */
  fg: [number, number, number];
  /** Default background color as [r, g, b] (0–255). */
  bg: [number, number, number];
  /** ANSI 16-color entries, indexed 0–15. */
  ansi: Array<[number, number, number]>;
}

/** Connection lifecycle states. */
export type ConnectionStatus =
  | "connecting"
  | "authenticating"
  | "connected"
  | "disconnected"
  | "error";

export type ConnectionId = string;
export type SessionId = string;

/**
 * Transport abstraction for blit server communication.
 * Implementations handle the underlying protocol (WebSocket, WebTransport, etc.)
 * while consumers only deal with binary messages and status changes.
 */
export type BlitTransportEventMap = {
  message: ArrayBuffer;
  statuschange: ConnectionStatus;
};

export interface BlitTransportOptions {
  /** Enable automatic reconnection on disconnect. Default: true. */
  reconnect?: boolean;
  /** Initial reconnect delay in ms. Default: 500. */
  reconnectDelay?: number;
  /** Maximum reconnect delay in ms. Default: 10000. */
  maxReconnectDelay?: number;
  /** Backoff multiplier for reconnect delay. Default: 1.5. */
  reconnectBackoff?: number;
  /** Timeout in ms to wait for the connection to be established. Default: none for WebSocket, 10000 for others. */
  connectTimeoutMs?: number;
}

export interface BlitTransport {
  /** Start connecting. Safe to call repeatedly. Call after registering listeners. */
  connect(): void;
  /** Send binary data to the server. */
  send(data: Uint8Array): void;
  /** Close the transport connection. */
  close(): void;
  /** Current connection status. */
  readonly status: ConnectionStatus;
  /** True when the server explicitly rejected authentication. */
  readonly authRejected: boolean;
  /** Last error message, if any. Cleared on successful connection. */
  readonly lastError: string | null;
  /** Register a listener for transport events. */
  addEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  addEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  /** Remove a previously registered listener. */
  removeEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  removeEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
}

/** A tracked terminal session. */
export type BlitSession = {
  id: SessionId;
  connectionId: ConnectionId;
  ptyId: number;
  tag: string;
  title: string | null;
  state: "creating" | "active" | "exited" | "closed";
};

export interface BlitConnectionSnapshot {
  id: ConnectionId;
  status: ConnectionStatus;
  ready: boolean;
  supportsRestart: boolean;
  retryCount: number;
  /** Non-null when the last connection attempt failed with an explicit error message. */
  error: string | null;
  sessions: readonly BlitSession[];
  focusedSessionId: SessionId | null;
}

export interface BlitWorkspaceSnapshot {
  connections: readonly BlitConnectionSnapshot[];
  sessions: readonly BlitSession[];
  focusedSessionId: SessionId | null;
  ready: boolean;
}

export interface BlitSearchResult {
  sessionId: SessionId;
  connectionId: ConnectionId;
  score: number;
  primarySource: number;
  matchedSources: number;
  scrollOffset: number | null;
  context: string;
}

/** Options for the BlitTerminal component. */
export interface BlitTerminalProps {
  /** Session ID to display. If null, the component renders an empty surface. */
  sessionId: SessionId | null;
  /** CSS font family for the terminal. */
  fontFamily?: string;
  /** Font size in CSS pixels used for cell measurement. */
  fontSize?: number;
  /** Additional CSS class name for the container. */
  className?: string;
  /** Additional inline styles for the container. */
  style?: React.CSSProperties;
  /** Color palette applied to the terminal. */
  palette?: TerminalPalette;
  /** When true, the terminal renders but never sends resize, input, or scroll commands. */
  readOnly?: boolean;
  /** When false, the cursor is hidden. Default: true. */
  showCursor?: boolean;
  /** Called after each render frame. Receives the render duration in ms. */
  onRender?: (renderMs: number) => void;
  /** Scrollbar indicator color (CSS color string). Default: "rgba(255,255,255,0.3)" */
  scrollbarColor?: string;
  /** Scrollbar indicator width in pixels. Default: 4 */
  scrollbarWidth?: number;
  /** Font advance-width / units-per-em ratio from font tables for native-accurate cell width. */
  advanceRatio?: number;
}

export const DEFAULT_FONT = "ui-monospace, monospace";
export const DEFAULT_FONT_SIZE = 13;

/** Wire protocol constants: client-to-server message types. */
export const C2S_INPUT = 0x00;
/** Desired viewport size(s): repeated [pty_id:2][rows:2][cols:2] entries. `0x0` clears one. */
export const C2S_RESIZE = 0x01;
export const C2S_SCROLL = 0x02;
export const C2S_ACK = 0x03;
export const C2S_DISPLAY_RATE = 0x04;
export const C2S_CLIENT_METRICS = 0x05;
export const C2S_MOUSE = 0x06;
export const C2S_RESTART = 0x07;
export const C2S_CREATE = 0x10;
export const C2S_FOCUS = 0x11;
export const C2S_CLOSE = 0x12;
export const C2S_SUBSCRIBE = 0x13;
export const C2S_UNSUBSCRIBE = 0x14;
export const C2S_SEARCH = 0x15;
export const C2S_CREATE_AT = 0x16;
export const C2S_CREATE_N = 0x17;
export const C2S_CREATE2 = 0x18;
export const CREATE2_HAS_SRC_PTY = 1 << 0;
export const CREATE2_HAS_COMMAND = 1 << 1;

/** Wire protocol constants: server-to-client message types. */
export const S2C_UPDATE = 0x00;
export const S2C_CREATED = 0x01;
export const S2C_CLOSED = 0x02;
export const S2C_LIST = 0x03;
export const S2C_TITLE = 0x04;
export const S2C_SEARCH_RESULTS = 0x05;
export const S2C_CREATED_N = 0x06;
export const S2C_HELLO = 0x07;
export const S2C_EXITED = 0x08;

export const PROTOCOL_VERSION = 1;
export const FEATURE_CREATE_NONCE = 1 << 0;
export const FEATURE_RESTART = 1 << 1;
export const FEATURE_RESIZE_BATCH = 1 << 2;
