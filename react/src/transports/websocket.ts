import type { BlitTransport, ConnectionStatus } from '../types';

export interface WebSocketTransportOptions {
  /** Enable automatic reconnection on disconnect. Default: true. */
  reconnect?: boolean;
  /** Initial reconnect delay in ms. Default: 500. */
  reconnectDelay?: number;
  /** Maximum reconnect delay in ms. Default: 10000. */
  maxReconnectDelay?: number;
  /** Backoff multiplier for reconnect delay. Default: 1.5. */
  reconnectBackoff?: number;
}

/**
 * WebSocket-based transport for the blit protocol.
 *
 * Handles connection, passphrase authentication, binary message framing,
 * and optional reconnection with exponential backoff.
 */
export class WebSocketTransport implements BlitTransport {
  private ws: WebSocket | null = null;
  private _status: ConnectionStatus = 'disconnected';
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;

  onmessage: ((data: ArrayBuffer) => void) | null = null;
  onstatuschange: ((status: ConnectionStatus) => void) | null = null;

  private readonly url: string;
  private readonly passphrase: string;
  private readonly reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;

  constructor(url: string, passphrase: string, options?: WebSocketTransportOptions) {
    this.url = url;
    this.passphrase = passphrase;
    this.reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.currentDelay = this.initialDelay;
    this.connect();
  }

  get status(): ConnectionStatus {
    return this._status;
  }

  send(data: Uint8Array): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data);
    }
  }

  close(): void {
    this.disposed = true;
    this.clearReconnectTimer();
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.onopen = null;
      this.ws.close();
      this.ws = null;
    }
    this.setStatus('disconnected');
  }

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    this._status = status;
    this.onstatuschange?.(status);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this.reconnect) return;
    this.clearReconnectTimer();
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.disposed) {
        this.connect();
      }
    }, this.currentDelay);
    this.currentDelay = Math.min(this.currentDelay * this.backoff, this.maxDelay);
  }

  private connect(): void {
    if (this.disposed) return;
    this.setStatus('connecting');

    const socket = new WebSocket(this.url);
    socket.binaryType = 'arraybuffer';

    if (this.ws && this.ws !== socket) {
      try {
        this.ws.onclose = null;
        this.ws.close();
      } catch {
        // Ignore close errors on stale socket.
      }
    }
    this.ws = socket;

    let authenticated = false;

    socket.onopen = () => {
      if (this.ws !== socket || this.disposed) return;
      this.setStatus('authenticating');
      socket.send(this.passphrase);
    };

    socket.onmessage = (e: MessageEvent) => {
      if (this.ws !== socket || this.disposed) return;

      if (typeof e.data === 'string') {
        if (e.data === 'ok') {
          authenticated = true;
          this.currentDelay = this.initialDelay;
          this.setStatus('connected');
        } else {
          // Authentication failed — do not reconnect.
          this.setStatus('error');
          socket.close();
        }
        return;
      }

      if (authenticated && e.data instanceof ArrayBuffer) {
        this.onmessage?.(e.data);
      }
    };

    socket.onerror = () => {
      if (this.ws !== socket || this.disposed) return;
      if (!authenticated) {
        this.setStatus('error');
      }
    };

    socket.onclose = () => {
      if (this.ws !== socket || this.disposed) return;
      this.ws = null;
      const wasConnected = authenticated;
      this.setStatus('disconnected');
      if (wasConnected) {
        this.scheduleReconnect();
      }
    };
  }
}
