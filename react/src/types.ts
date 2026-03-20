import type React from 'react';

/** Connection lifecycle states. */
export type ConnectionStatus =
  | 'connecting'
  | 'authenticating'
  | 'connected'
  | 'disconnected'
  | 'error';

/**
 * Transport abstraction for blit server communication.
 * Implementations handle the underlying protocol (WebSocket, WebTransport, etc.)
 * while consumers only deal with binary messages and status changes.
 */
export interface BlitTransport {
  /** Send binary data to the server. */
  send(data: Uint8Array): void;
  /** Close the transport connection. */
  close(): void;
  /** Current connection status. */
  readonly status: ConnectionStatus;
  /** Called when a binary message is received from the server. */
  onmessage: ((data: ArrayBuffer) => void) | null;
  /** Called when the connection status changes. */
  onstatuschange: ((status: ConnectionStatus) => void) | null;
}

/** Options for the BlitTerminal component. */
export interface BlitTerminalProps {
  /** Transport instance for server communication. */
  transport: BlitTransport;
  /** PTY ID to display. If null, the component waits for a PTY to be created. */
  ptyId: number | null;
  /** CSS font family for the terminal. */
  fontFamily?: string;
  /** Font size in CSS pixels used for cell measurement. */
  fontSize?: number;
  /** Called when the terminal title changes. */
  onTitleChange?: (title: string) => void;
  /** Called when a PTY is created by the server. */
  onPtyCreated?: (ptyId: number) => void;
  /** Called when a PTY is closed by the server. */
  onPtyClosed?: (ptyId: number) => void;
  /** Called when the PTY list is received. */
  onPtyList?: (ptyIds: number[]) => void;
  /** Additional CSS class name for the container. */
  className?: string;
  /** Additional inline styles for the container. */
  style?: React.CSSProperties;
}

/** Wire protocol constants: client-to-server message types. */
export const C2S_INPUT = 0x00;
export const C2S_RESIZE = 0x01;
export const C2S_SCROLL = 0x02;
export const C2S_ACK = 0x03;
export const C2S_DISPLAY_RATE = 0x04;
export const C2S_CLIENT_METRICS = 0x05;
export const C2S_CREATE = 0x10;
export const C2S_FOCUS = 0x11;
export const C2S_CLOSE = 0x12;
export const C2S_SUBSCRIBE = 0x13;
export const C2S_UNSUBSCRIBE = 0x14;

/** Wire protocol constants: server-to-client message types. */
export const S2C_UPDATE = 0x00;
export const S2C_CREATED = 0x01;
export const S2C_CLOSED = 0x02;
export const S2C_LIST = 0x03;
export const S2C_TITLE = 0x04;
export const S2C_SEARCH_RESULTS = 0x05;
