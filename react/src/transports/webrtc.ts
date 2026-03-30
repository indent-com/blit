import type { BlitTransport, BlitTransportOptions, ConnectionStatus } from "../types";
import { C2S_DISPLAY_RATE } from "../types";

export interface WebRtcDataChannelTransportOptions extends BlitTransportOptions {
  /** Data channel label. Default: "blit". */
  label?: string;
  /** Display rate to advertise to the server. Default: 120. */
  displayRateFps?: number;
}

export function createWebRtcDataChannelTransport(
  pc: RTCPeerConnection,
  opts?: WebRtcDataChannelTransportOptions,
): BlitTransport & { waitForSync(): Promise<void> } {
  const label = opts?.label ?? "blit";
  const displayRateFps = opts?.displayRateFps ?? 120;
  const connectTimeoutMs = opts?.connectTimeoutMs ?? 10000;
  const shouldReconnect = opts?.reconnect ?? true;
  const initialDelay = opts?.reconnectDelay ?? 500;
  const maxDelay = opts?.maxReconnectDelay ?? 10000;
  const backoff = opts?.reconnectBackoff ?? 1.5;

  let _status: ConnectionStatus = "connecting";
  let _lastError: string | null = null;
  let channel: RTCDataChannel | null = null;
  let disposed = false;
  let syncResolve: (() => void) | null = null;
  let syncReject: ((err: Error) => void) | null = null;
  let readBuf = new Uint8Array(0);
  let connectTimeout: ReturnType<typeof setTimeout> | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let currentDelay = initialDelay;
  let started = false;
  let earlyMessages: ArrayBuffer[] = [];

  const messageListeners = new Set<(data: ArrayBuffer) => void>();
  const statusListeners = new Set<(status: ConnectionStatus) => void>();

  function dispatch(data: ArrayBuffer) {
    if (!started) {
      earlyMessages.push(data);
    } else {
      for (const l of messageListeners) l(data);
    }
  }

  function clearConnectTimeout() {
    if (connectTimeout !== null) {
      clearTimeout(connectTimeout);
      connectTimeout = null;
    }
  }

  function clearReconnectTimer() {
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
  }

  function isPeerConnectionAlive(): boolean {
    const s = pc.connectionState;
    return s !== "failed" && s !== "closed";
  }

  function scheduleReconnect() {
    if (disposed || !shouldReconnect || !isPeerConnectionAlive()) return;
    clearReconnectTimer();
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      if (!disposed && isPeerConnectionAlive()) {
        openChannel();
      }
    }, currentDelay);
    currentDelay = Math.min(currentDelay * backoff, maxDelay);
  }

  function wireChannel(ch: RTCDataChannel) {
    channel = ch;
    channel.binaryType = "arraybuffer";
    readBuf = new Uint8Array(0);

    clearConnectTimeout();
    connectTimeout = setTimeout(() => {
      connectTimeout = null;
      if (_status === "connecting") {
        _lastError = "connect timeout";
        setStatus("error");
        scheduleReconnect();
      }
    }, connectTimeoutMs);

    ch.onopen = () => {
      if (disposed || channel !== ch) return;
      clearConnectTimeout();
      currentDelay = initialDelay;
      _lastError = null;
      setStatus("connected");
      const msg = new Uint8Array(3);
      msg[0] = C2S_DISPLAY_RATE;
      msg[1] = displayRateFps & 0xff;
      msg[2] = (displayRateFps >> 8) & 0xff;
      transport.send(msg);
    };

    ch.onmessage = (e: MessageEvent) => {
      if (disposed || channel !== ch) return;
      const incoming = new Uint8Array(e.data as ArrayBuffer);
      const combined = new Uint8Array(readBuf.length + incoming.length);
      combined.set(readBuf);
      combined.set(incoming, readBuf.length);
      readBuf = combined;

      while (readBuf.length >= 4) {
        const len =
          readBuf[0] |
          (readBuf[1] << 8) |
          (readBuf[2] << 16) |
          (readBuf[3] << 24);
        if (readBuf.length < 4 + len) break;
        const payload = readBuf.slice(4, 4 + len);
        readBuf = readBuf.slice(4 + len);
        dispatch(payload.buffer as ArrayBuffer);
      }
    };

    ch.onerror = () => {
      if (disposed || channel !== ch) return;
      clearConnectTimeout();
      _lastError = "Data channel error";
      setStatus("error");
      scheduleReconnect();
    };

    ch.onclose = () => {
      if (disposed || channel !== ch) return;
      clearConnectTimeout();
      setStatus("disconnected");
      scheduleReconnect();
    };
  }

  function openChannel() {
    if (disposed) return;
    setStatus("connecting");
    wireChannel(pc.createDataChannel(label, { ordered: true }));
  }

  const transport: BlitTransport & { waitForSync(): Promise<void> } = {
    connect() {
      if (started) return;
      started = true;
      for (const msg of earlyMessages) {
        for (const l of messageListeners) l(msg);
      }
      earlyMessages = [];
    },

    get status() {
      return _status;
    },

    get authRejected() {
      return false;
    },
    get lastError() {
      return _lastError;
    },

    addEventListener(type: string, listener: (data: never) => void): void {
      if (type === "message") {
        messageListeners.add(
          listener as unknown as (data: ArrayBuffer) => void,
        );
      } else if (type === "statuschange") {
        statusListeners.add(
          listener as unknown as (status: ConnectionStatus) => void,
        );
      }
    },

    removeEventListener(type: string, listener: (data: never) => void): void {
      if (type === "message") {
        messageListeners.delete(
          listener as unknown as (data: ArrayBuffer) => void,
        );
      } else if (type === "statuschange") {
        statusListeners.delete(
          listener as unknown as (status: ConnectionStatus) => void,
        );
      }
    },

    send(data: Uint8Array) {
      if (!channel || channel.readyState !== "open") return;
      const frame = new Uint8Array(4 + data.length);
      const len = data.length;
      frame[0] = len & 0xff;
      frame[1] = (len >> 8) & 0xff;
      frame[2] = (len >> 16) & 0xff;
      frame[3] = (len >> 24) & 0xff;
      frame.set(data, 4);
      channel.send(frame);
    },

    close() {
      disposed = true;
      clearConnectTimeout();
      clearReconnectTimer();
      if (channel) {
        try {
          channel.close();
        } catch {
          // Ignore.
        }
        channel = null;
      }
      pc.removeEventListener("connectionstatechange", onConnectionStateChange);
      setStatus("disconnected");
    },

    waitForSync() {
      if (_status === "connected") return Promise.resolve();
      if (_status === "error" || _status === "disconnected") {
        return Promise.reject(new Error(`transport ${_status}`));
      }
      return new Promise<void>((resolve, reject) => {
        syncResolve = resolve;
        syncReject = reject;
      });
    },
  };

  function setStatus(s: ConnectionStatus) {
    if (_status === s) return;
    _status = s;
    for (const l of statusListeners) l(s);
    if (s === "connected") {
      syncResolve?.();
      syncResolve = null;
      syncReject = null;
    } else if (s === "error" || s === "disconnected") {
      syncReject?.(new Error(`transport ${s}`));
      syncResolve = null;
      syncReject = null;
    }
  }

  function onConnectionStateChange() {
    if (disposed) return;
    if (pc.connectionState === "failed" || pc.connectionState === "closed") {
      clearReconnectTimer();
      setStatus("disconnected");
    }
  }
  pc.addEventListener("connectionstatechange", onConnectionStateChange);

  wireChannel(pc.createDataChannel(label, { ordered: true }));

  return transport;
}
