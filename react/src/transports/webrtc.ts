import type { BlitTransport, ConnectionStatus } from '../types';
import { C2S_DISPLAY_RATE } from '../types';

export interface WebRtcDataChannelTransportOptions {
  /** Data channel label. Default: "blit". */
  label?: string;
  /** Display rate to advertise to the server. Default: 120. */
  displayRateFps?: number;
  /** Timeout in ms to wait for the data channel to open. Default: 10000. */
  connectTimeoutMs?: number;
}

/**
 * Creates a BlitTransport backed by a WebRTC data channel.
 *
 * The transport manages the data channel lifecycle on the given
 * RTCPeerConnection and handles the 4-byte length-prefixed frame envelope
 * used by the blit protocol over non-WebSocket transports.
 */
export function createWebRtcDataChannelTransport(
  pc: RTCPeerConnection,
  opts?: WebRtcDataChannelTransportOptions,
): BlitTransport & { waitForSync(): Promise<void> } {
  const label = opts?.label ?? 'blit';
  const displayRateFps = opts?.displayRateFps ?? 120;
  const connectTimeoutMs = opts?.connectTimeoutMs ?? 10000;

  let _status: ConnectionStatus = 'connecting';
  let channel: RTCDataChannel | null = null;
  let disposed = false;
  let syncResolve: (() => void) | null = null;
  let syncReject: ((err: Error) => void) | null = null;
  let readBuf = new Uint8Array(0);

  const transport: BlitTransport & { waitForSync(): Promise<void> } = {
    get status() {
      return _status;
    },
    onmessage: null,
    onstatuschange: null,

    send(data: Uint8Array) {
      if (!channel || channel.readyState !== 'open') return;
      // Wrap in 4-byte length-prefixed frame.
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
      if (channel) {
        try {
          channel.close();
        } catch {
          // Ignore.
        }
        channel = null;
      }
      setStatus('disconnected');
    },

    waitForSync() {
      if (_status === 'connected') return Promise.resolve();
      if (_status === 'error' || _status === 'disconnected') {
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
    transport.onstatuschange?.(s);
    if (s === 'connected') {
      syncResolve?.();
      syncResolve = null;
      syncReject = null;
    } else if (s === 'error' || s === 'disconnected') {
      syncReject?.(new Error(`transport ${s}`));
      syncResolve = null;
      syncReject = null;
    }
  }

  // --- Data channel setup ---

  channel = pc.createDataChannel(label, { ordered: true });
  channel.binaryType = 'arraybuffer';

  const timeout = setTimeout(() => {
    if (_status === 'connecting') {
      setStatus('error');
    }
  }, connectTimeoutMs);

  channel.onopen = () => {
    if (disposed) return;
    clearTimeout(timeout);
    setStatus('connected');
    // Advertise display rate.
    const msg = new Uint8Array(3);
    msg[0] = C2S_DISPLAY_RATE;
    msg[1] = displayRateFps & 0xff;
    msg[2] = (displayRateFps >> 8) & 0xff;
    transport.send(msg);
  };

  channel.onmessage = (e: MessageEvent) => {
    if (disposed) return;
    const incoming = new Uint8Array(e.data as ArrayBuffer);
    // Append to read buffer for frame reassembly.
    const combined = new Uint8Array(readBuf.length + incoming.length);
    combined.set(readBuf);
    combined.set(incoming, readBuf.length);
    readBuf = combined;

    // Drain complete frames.
    while (readBuf.length >= 4) {
      const len =
        readBuf[0] |
        (readBuf[1] << 8) |
        (readBuf[2] << 16) |
        (readBuf[3] << 24);
      if (readBuf.length < 4 + len) break;
      const payload = readBuf.slice(4, 4 + len);
      readBuf = readBuf.subarray(4 + len);
      transport.onmessage?.(payload.buffer);
    }
  };

  channel.onerror = () => {
    if (disposed) return;
    clearTimeout(timeout);
    setStatus('error');
  };

  channel.onclose = () => {
    if (disposed) return;
    clearTimeout(timeout);
    setStatus('disconnected');
  };

  pc.addEventListener('connectionstatechange', () => {
    if (disposed) return;
    if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
      setStatus('disconnected');
    }
  });

  return transport;
}
