import type React from 'react';

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
export type BlitTransportEventMap = {
  message: ArrayBuffer;
  statuschange: ConnectionStatus;
};

export interface BlitTransport {
  /** Send binary data to the server. */
  send(data: Uint8Array): void;
  /** Close the transport connection. */
  close(): void;
  /** Current connection status. */
  readonly status: ConnectionStatus;
  /** Register a listener for transport events. */
  addEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void;
  /** Remove a previously registered listener. */
  removeEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void;
}

/** A tracked PTY session. */
export type BlitSession = {
  ptyId: number;
  tag: string;
  title: string | null;
  state: 'active' | 'closed';
};

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
  /** Additional CSS class name for the container. */
  className?: string;
  /** Additional inline styles for the container. */
  style?: React.CSSProperties;
  /** Color palette applied to the terminal. */
  palette?: TerminalPalette;
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
export const C2S_CREATE_N = 0x17;

/** Wire protocol constants: server-to-client message types. */
export const S2C_UPDATE = 0x00;
export const S2C_CREATED = 0x01;
export const S2C_CLOSED = 0x02;
export const S2C_LIST = 0x03;
export const S2C_TITLE = 0x04;
export const S2C_SEARCH_RESULTS = 0x05;
export const S2C_CREATED_N = 0x06;
export const S2C_HELLO = 0x07;

export const PROTOCOL_VERSION = 1;
export const FEATURE_CREATE_NONCE = 1 << 0;
