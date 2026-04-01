import type {
  BlitTransport,
  BlitTransportOptions,
  ConnectionStatus,
} from "../types";

export type WebSocketTransportOptions = BlitTransportOptions;

export class WebSocketTransport implements BlitTransport {
  private ws: WebSocket | null = null;
  private _status: ConnectionStatus = "disconnected";
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private connectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;
  private messageListeners = new Set<(data: ArrayBuffer) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  /** True when the gateway explicitly rejected the passphrase. */
  authRejected = false;
  /** Last error message from the gateway, if any. */
  lastError: string | null = null;

  private readonly url: string;
  private readonly passphrase: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;
  private readonly connectTimeoutMs: number | null;

  constructor(
    url: string,
    passphrase: string,
    options?: WebSocketTransportOptions,
  ) {
    this.url = url;
    this.passphrase = passphrase;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.connectTimeoutMs = options?.connectTimeoutMs ?? null;
    this.currentDelay = this.initialDelay;
  }

  get status(): ConnectionStatus {
    return this._status;
  }

  send(data: Uint8Array): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data as Uint8Array<ArrayBuffer>);
    }
  }

  private clearConnectTimer(): void {
    if (this.connectTimer !== null) {
      clearTimeout(this.connectTimer);
      this.connectTimer = null;
    }
  }

  close(): void {
    this.disposed = true;
    this.clearReconnectTimer();
    this.clearConnectTimer();
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.onopen = null;
      this.ws.close();
      this.ws = null;
    }
    this.setStatus("disconnected");
  }

  addEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  addEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  addEventListener(type: string, listener: (...args: never[]) => void): void {
    if (type === "message") {
      this.messageListeners.add(listener as (data: ArrayBuffer) => void);
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener(
    type: "message",
    listener: (data: ArrayBuffer) => void,
  ): void;
  removeEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  removeEventListener(
    type: string,
    listener: (...args: never[]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.delete(listener as (data: ArrayBuffer) => void);
    } else if (type === "statuschange") {
      this.statusListeners.delete(
        listener as (status: ConnectionStatus) => void,
      );
    }
  }

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    this._status = status;
    for (const l of this.statusListeners) l(status);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this._reconnect) return;
    this.clearReconnectTimer();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) {
        this.connect();
      }
    }, this.currentDelay);
    this.currentDelay = Math.min(
      this.currentDelay * this.backoff,
      this.maxDelay,
    );
  }

  connect(): void {
    if (this.disposed) return;
    // Cancel any pending auto-reconnect; manual connect takes priority.
    if (this.reconnectTimer !== null) {
      this.clearReconnectTimer();
      this.currentDelay = this.initialDelay;
    }
    if (
      this._status === "connecting" ||
      this._status === "authenticating" ||
      this._status === "connected"
    )
      return;
    this.setStatus("connecting");

    const socket = new WebSocket(this.url);
    socket.binaryType = "arraybuffer";

    if (this.ws && this.ws !== socket) {
      try {
        this.ws.onclose = null;
        this.ws.close();
      } catch {
        // Ignore close errors on stale socket.
      }
    }
    this.ws = socket;

    this.clearConnectTimer();
    if (this.connectTimeoutMs !== null) {
      this.connectTimer = setTimeout(() => {
        this.connectTimer = null;
        if (this.ws !== socket || this.disposed) return;
        if (
          this._status === "connecting" ||
          this._status === "authenticating"
        ) {
          this.ws = null;
          this.lastError = "connect timeout";
          this.setStatus("error");
          socket.onclose = null;
          socket.close();
          this.scheduleReconnect();
        }
      }, this.connectTimeoutMs);
    }

    let authenticated = false;

    socket.onopen = () => {
      if (this.ws !== socket || this.disposed) return;
      this.setStatus("authenticating");
      socket.send(this.passphrase);
    };

    socket.onmessage = (e: MessageEvent) => {
      if (this.ws !== socket || this.disposed) return;

      if (typeof e.data === "string") {
        if (e.data === "ok") {
          authenticated = true;
          this.clearConnectTimer();
          this.authRejected = false;
          this.lastError = null;
          this.currentDelay = this.initialDelay;
          this.setStatus("connected");
        } else if (e.data === "auth") {
          this.authRejected = true;
          this.lastError = "Authentication failed";
          this.setStatus("error");
          socket.close();
        } else {
          // Gateway sent an error (e.g. "error:cannot connect to blit-server")
          this.authRejected = false;
          this.lastError = e.data.startsWith("error:")
            ? e.data.slice(6)
            : e.data;
          this.setStatus("error");
          socket.close();
        }
        return;
      }

      if (authenticated && e.data instanceof ArrayBuffer) {
        for (const l of this.messageListeners) l(e.data);
      }
    };

    socket.onerror = () => {
      if (this.ws !== socket || this.disposed) return;
      if (!authenticated) {
        this.setStatus("error");
      }
    };

    socket.onclose = () => {
      if (this.ws !== socket || this.disposed) return;
      this.clearConnectTimer();
      this.ws = null;
      if (authenticated) {
        this.setStatus("disconnected");
      } else {
        this.setStatus(this.authRejected ? "error" : "disconnected");
      }
      // Always retry unless auth was explicitly rejected or reconnect is off.
      if (!this.authRejected) {
        this.scheduleReconnect();
      }
    };
  }
}
