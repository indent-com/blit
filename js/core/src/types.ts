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

export interface BlitDebug {
  log(msg: string, ...args: unknown[]): void;
  warn(msg: string, ...args: unknown[]): void;
  error(msg: string, ...args: unknown[]): void;
}

/** Silent {@link BlitDebug} that discards everything. */
export const noopDebug: BlitDebug = { log() {}, warn() {}, error() {} };

/** Connection lifecycle states. */
export type ConnectionStatus =
  | "connecting"
  | "authenticating"
  | "connected"
  | "disconnected"
  | "closed"
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
  /** Tear down the current connection and reconnect from scratch. */
  reconnect?(): void;
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
  /** Highest visible terminal row reached since the last terminal reset. */
  usedRows: number;
  command: string | null;
  state: "creating" | "active" | "exited" | "closed";
  /**
   * Raw exit status from the server once the process has exited (the
   * `exit_status` field of `S2C_EXITED`), or `null` while running.
   *
   * `>= 0` is the normal exit code, `< 0` is the negated terminating
   * signal, and {@link EXIT_STATUS_UNKNOWN} means "not yet collected".
   * Use `exitCodeFromStatus` to map it to a conventional shell exit code.
   */
  exitStatus: number | null;
};

export interface BlitConnectionSnapshot {
  id: ConnectionId;
  status: ConnectionStatus;
  ready: boolean;
  supportsRestart: boolean;
  supportsCopyRange: boolean;
  supportsCompositor: boolean;
  supportsAudio: boolean;
  supportsFsSync: boolean;
  /** Server advertises `FEATURE_GIT` (git introspection, docs/git.md). */
  supportsGit: boolean;
  /** Server advertises `FEATURE_LSP` (language intelligence, docs/design/lsp.md). */
  supportsLsp: boolean;
  /** Server advertises the KV store family (docs/design/kv.md). */
  supportsKv: boolean;
  retryCount: number;
  /** Opaque 64-bit identifier for the current server process, or `null` for
   *  servers predating the extended HELLO. */
  bootGeneration: bigint | null;
  /** The remote blit server's release, e.g. `"0.40.1"` — `null` for servers
   *  predating the field in HELLO. */
  serverVersion: string | null;
  /** Bumped on every connection reset (transport drop AND server
   *  re-establish), so views holding fs/git handles can re-open them — those
   *  don't survive a reset even when the transport stays up. */
  generation: number;
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

export interface CopyRangeResult {
  /** Copied text.  Soft-wrapped rows are joined without a separator. */
  text: string;
  /**
   * Rows the PTY held when the copy ran (scrollback plus screen), so a caller
   * that asked for a bounded window can tell whether rows were left above it.
   */
  totalLines: number;
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

export type TransportConfig =
  | {
      type: "websocket";
      url: string;
      passphrase: string;
      options?: BlitTransportOptions;
    }
  | {
      type: "webtransport";
      url: string;
      passphrase: string;
      options?: BlitTransportOptions & { certHash?: string };
    }
  | { type: "share"; hubUrl: string; passphrase: string; debug?: BlitDebug }
  | { type: "custom"; transport: BlitTransport };

export const DEFAULT_FONT = "ui-monospace, monospace";
export const DEFAULT_FONT_SIZE = 13;

/**
 * Coverage gamma for glyph antialiasing (1 = untouched, higher = thinner
 * light-on-dark text).
 *
 * Glyph coverage is blended into an sRGB-encoded framebuffer, which overstates
 * partial coverage and makes light-on-dark stems read bolder than the font
 * intends. Apple platforms are where that lands hardest — the system's own
 * text rendering is the reference users compare against, and it thins stems
 * the same way — so they get a correction by default and everyone else opts
 * in. Same reasoning, and roughly the same value, as kitty's
 * `text_gamma_adjustment`.
 */
export const DEFAULT_TEXT_GAMMA = isApplePlatform() ? 1.4 : 1;

function isApplePlatform(): boolean {
  if (typeof navigator === "undefined") return false;
  return /Mac|iPhone|iPad|iPod/.test(navigator.platform);
}

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
export const C2S_KILL = 0x1a;
export const C2S_COPY_RANGE = 0x1b;
export const C2S_TERM_CWD = 0x1c;
export const CREATE2_HAS_SRC_PTY = 1 << 0;
export const CREATE2_HAS_COMMAND = 1 << 1;
export const CREATE2_HAS_CWD = 1 << 2;

/** Wire protocol constants: server-to-client message types. */
export const S2C_UPDATE = 0x00;
export const S2C_CREATED = 0x01;
export const S2C_CLOSED = 0x02;
export const S2C_LIST = 0x03;
export const S2C_TITLE = 0x04;
export const S2C_TERM_CWD = 0x0e;
/** Unsolicited push when a pty's OSC 7-reported cwd changes
 *  (docs/protocol.md `TERM_CWD_EVENT`): [pty_id:2][cwd:N], no length
 *  prefix — the S2C_TITLE convention. */
export const S2C_TERM_CWD_EVENT = 0x0f;
export const S2C_SEARCH_RESULTS = 0x05;
export const S2C_CREATED_N = 0x06;
export const S2C_HELLO = 0x07;
export const S2C_EXITED = 0x08;
export const S2C_READY = 0x09;
export const S2C_TEXT = 0x0a;
export const S2C_PING = 0x0b;
export const S2C_QUIT = 0x0c;
export const S2C_USED_ROWS = 0x0d;
export const C2S_PING = 0x08;
export const C2S_QUIT = 0x0f;

export const C2S_SURFACE_INPUT = 0x20;
export const C2S_SURFACE_POINTER = 0x21;
export const C2S_SURFACE_POINTER_AXIS = 0x22;
/**
 * Scroll with both axes, a device source and discrete detents:
 * [0x32][surface_id:2][flags:1][dx_x100:4][dy_x100:4][v120_x:2][v120_y:2]
 *
 * Deltas are in the composited frame's pixel space, like
 * {@link C2S_SURFACE_POINTER}; the server converts to surface-logical
 * pixels. `v120` counts wheel detents in 120ths.
 */
export const C2S_SURFACE_POINTER_AXIS2 = 0x32;

/** `wl_pointer.axis_source` values, carried in the AXIS2 flags byte. */
export const AXIS_SOURCE_WHEEL = 0;
export const AXIS_SOURCE_FINGER = 1;
export const AXIS_SOURCE_CONTINUOUS = 2;
/** Set when the source bits mean anything. */
export const AXIS_FLAG_SOURCE_KNOWN = 1 << 2;
/** Set when this event ends a scroll sequence. */
export const AXIS_FLAG_STOP = 1 << 3;

export const C2S_SURFACE_RESIZE = 0x23;
export const C2S_SURFACE_FOCUS = 0x24;
export const C2S_CLIPBOARD_SET = 0x25;
export const C2S_SURFACE_SUBSCRIBE = 0x28;
export const C2S_SURFACE_UNSUBSCRIBE = 0x29;
export const C2S_SURFACE_ACK = 0x2a;
export const C2S_SURFACE_CLOSE = 0x2b;
export const C2S_CLIENT_FEATURES = 0x2d;
/** Composed text input for a Wayland surface (UTF-8): [0x2F][surface_id:2][text:N] */
export const C2S_SURFACE_TEXT = 0x2f;
export const S2C_SURFACE_CREATED = 0x20;
export const S2C_SURFACE_DESTROYED = 0x21;
export const S2C_SURFACE_FRAME = 0x22;
export const S2C_SURFACE_TITLE = 0x23;
export const S2C_SURFACE_RESIZED = 0x24;
export const S2C_CLIPBOARD_CONTENT = 0x25;
export const S2C_SURFACE_APP_ID = 0x28;
export const S2C_SURFACE_CURSOR = 0x29;
/** Encoder backend info for a surface: [0x2A][surface_id:2][name:N] */
export const S2C_SURFACE_ENCODER = 0x2a;
/**
 * Fragment of a larger S2C message: [0x2B][flags:1][chunk:N].
 * Bulk messages above the server's chunk threshold are split into
 * fragments so audio frames can interleave on the shared TCP stream.
 * Receiver concatenates fragment chunks (in order; no reordering on
 * the same stream) until a fragment with FRAGMENT_FLAG_LAST set, then
 * dispatches the reassembled buffer as the original message.
 */
export const S2C_FRAGMENT = 0x2b;
export const FRAGMENT_FLAG_LAST = 1 << 0;
export const SURFACE_FRAME_FLAG_KEYFRAME = 1 << 0;
export const SURFACE_FRAME_CODEC_MASK = 0b110;
export const SURFACE_FRAME_CODEC_H264 = 0 << 1;
export const SURFACE_FRAME_CODEC_AV1 = 1 << 1;
export const SURFACE_FRAME_CODEC_PNG = 2 << 1;

/** Bitmask for client-supported codecs in C2S_SURFACE_RESIZE / C2S_SURFACE_SUBSCRIBE. 0 = accept anything. */
export const CODEC_SUPPORT_H264 = 1 << 0;
export const CODEC_SUPPORT_AV1 = 1 << 1;
export const CODEC_SUPPORT_H264_444 = 1 << 2;
export const CODEC_SUPPORT_AV1_444 = 1 << 3;

/** Bandwidth values for C2S_SURFACE_SUBSCRIBE. 0 = server default.
 *  10–255 = custom AV1 quantizer (wire value IS the quantizer). */
export const SURFACE_BANDWIDTH_DEFAULT = 0;
export const SURFACE_BANDWIDTH_LOW = 1;
export const SURFACE_BANDWIDTH_MEDIUM = 2;
export const SURFACE_BANDWIDTH_HIGH = 3;
export const SURFACE_BANDWIDTH_ULTRA = 4;

/** Encoder speed values for C2S_SURFACE_SUBSCRIBE. 0 = server default.
 *  10–255 = custom (10 = slowest/best compression, 255 = fastest). */
export const SURFACE_SPEED_DEFAULT = 0;
export const SURFACE_SPEED_SLOW = 1;
export const SURFACE_SPEED_MEDIUM = 2;
export const SURFACE_SPEED_FAST = 3;
export const SURFACE_SPEED_REALTIME = 4;

export const PROTOCOL_VERSION = 1;
export const FEATURE_CREATE_NONCE = 1 << 0;
export const FEATURE_RESTART = 1 << 1;
export const FEATURE_RESIZE_BATCH = 1 << 2;
export const FEATURE_COPY_RANGE = 1 << 3;
export const FEATURE_COMPOSITOR = 1 << 4;
export const FEATURE_AUDIO = 1 << 5;

// -- Audio forwarding --
export const C2S_AUDIO_SUBSCRIBE = 0x30;
export const C2S_AUDIO_UNSUBSCRIBE = 0x31;
export const S2C_AUDIO_FRAME = 0x30;
export const AUDIO_FRAME_CODEC_MASK = 0b110;
export const AUDIO_FRAME_CODEC_OPUS = 0 << 1;

export type BlitSurface = {
  connectionId: ConnectionId;
  surfaceId: u16;
  parentId: u16;
  title: string;
  appId: string;
  width: number;
  height: number;
};

type u16 = number;
