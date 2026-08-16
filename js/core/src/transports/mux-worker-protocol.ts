/**
 * Wire protocol between the main thread and the transport worker.
 *
 * The worker owns the socket — WebSocket, and the WebTransport upgrade with
 * it — so that receiving a frame does not require the main thread to be
 * running. Audio is the reason: an encoded audio frame is 160 bytes every
 * 20 ms, and it used to reach the decoder only when the main thread got round
 * to dispatching it. Scrolling a window is enough to stop that happening —
 * half-megabyte surface frames arriving, being parsed, decoded and painted —
 * so audio arrived in bursts, the jitter buffer ran dry, and playback paused
 * on a local network with a server that had handed every frame over on time.
 *
 * Measured before this existed: the server reported a worst gap of 42 ms
 * between audio writes with zero drops, while the client sat at a 360 ms
 * buffer and still starved. The gap was manufactured after delivery.
 *
 * Only audio takes the short path. Everything else is forwarded to the main
 * thread, which keeps its decoding and its vsync-paced presenter exactly as
 * they were — worker-side presentation is not available on Safari, which is
 * the platform that needs this most.
 */

import type { ConnectionStatus } from "../types";

/** Options forwarded to the worker's `MuxTransport`. */
export interface MuxWorkerOptions {
  reconnect?: boolean;
  reconnectDelay?: number;
  maxReconnectDelay?: number;
  reconnectBackoff?: number;
  connectTimeoutMs?: number;
  wtConnectTimeoutMs?: number;
  channelConnectTimeoutMs?: number;
  wtReprobeMs?: number;
  wtUrl?: string;
  wtCertHash?: string;
}

export type MuxWorkerRequest =
  | {
      t: "init";
      wsUrl: string;
      passphrase: string;
      options?: MuxWorkerOptions;
    }
  /**
   * Hands over the port that reaches one channel's audio decoder directly.
   *
   * Per channel, not per transport: each connection has its own `AudioPlayer`,
   * and an audio frame belongs to exactly one of them.
   */
  | { t: "audioPort"; id: number }
  /**
   * Stop diverting audio for a channel; send it to the main thread again.
   *
   * The short path is only worth taking while there is a live decoder at the
   * far end. When that decoder dies the port stays open and swallows every
   * frame, and because the main thread then sees headers with no payload it
   * has nothing to decode either — silence that no amount of reconnecting
   * recovers. Revoking puts audio back on the ordinary route, which is slower
   * but works.
   */
  | { t: "audioPortDetach"; id: number }
  | { t: "connect" }
  | { t: "close" }
  | { t: "updateWtCertHash"; hexHash: string; wtUrl?: string }
  /**
   * Channel ids are allocated by the main thread, not the worker.
   * `createChannel` has to return a channel synchronously, and the id is
   * protocol-significant — it prefixes every frame — so it cannot wait for a
   * round trip.
   */
  | { t: "channelCreate"; id: number; destName: string }
  | { t: "channelConnect"; id: number }
  | { t: "channelReconnect"; id: number }
  | { t: "channelSend"; id: number; data: ArrayBuffer }
  | { t: "channelClose"; id: number }
  | { t: "channelSuspend"; id: number }
  | { t: "cancelChannelConnectTimer"; id: number };

export type MuxWorkerEvent =
  | { t: "status"; status: ConnectionStatus; isWebTransport: boolean }
  /**
   * A diagnostic from the worker's `MuxTransport`, relayed to the logger on
   * the main thread. The logger itself cannot cross: it is an object with
   * methods, and methods do not survive structured clone.
   *
   * Arguments are rendered to strings here rather than sent as values,
   * because anything the caller chose to log is arbitrary — a socket, an
   * error, a class instance — and one uncloneable argument would throw
   * inside the very path that reports trouble.
   */
  | { t: "debug"; level: "log" | "warn" | "error"; msg: string; args: string[] }
  | {
      t: "channelStatus";
      id: number;
      status: ConnectionStatus;
      authRejected: boolean;
      lastError: string | null;
    }
  /** `data` is transferred, so this costs no copy. */
  | { t: "channelMessage"; id: number; data: ArrayBuffer };
