/**
 * The transport, running off the main thread.
 *
 * This owns the real `MuxTransport` — the socket, the WebTransport upgrade,
 * reconnection, all of it — and reports to the main thread by message. See
 * `mux-worker-protocol.ts` for why: a frame must be able to arrive while the
 * main thread is busy, because the main thread is busy exactly when a window
 * is being scrolled, and that is when audio was stopping.
 *
 * `MuxTransport` needs no changes to live here. It touches no DOM: WebSocket,
 * WebTransport and timers are all available in a worker, which is what made
 * this seam worth choosing over moving decode or presentation (neither of
 * which can leave the main thread on Safari).
 */

import { MuxTransport, type MuxChannel } from "./mux";
import type { MuxWorkerEvent, MuxWorkerRequest } from "./mux-worker-protocol";
import type { BlitTransportMessage, ConnectionStatus } from "../types";

let transport: MuxTransport | null = null;
const channels = new Map<number, MuxChannel>();

/** Ports straight to each channel's audio decoder, when supplied. */
const audioPorts = new Map<number, MessagePort>();

/** `self` in a worker, typed without pulling the whole WebWorker lib in. */
const worker = self as unknown as {
  postMessage(message: MuxWorkerEvent, transfer: Transferable[]): void;
  onmessage: ((event: MessageEvent<MuxWorkerRequest>) => void) | null;
};

function post(event: MuxWorkerEvent, transfer?: Transferable[]): void {
  worker.postMessage(event, transfer ?? []);
}

/** Render a diagnostic to strings and send it to the main thread's logger. */
function relay(
  level: "log" | "warn" | "error",
  msg: string,
  args: unknown[],
): void {
  post({
    t: "debug",
    level,
    msg,
    args: args.map((arg) => {
      try {
        return typeof arg === "string"
          ? arg
          : (JSON.stringify(arg) ?? String(arg));
      } catch {
        // Cyclic, or a host object with a throwing getter.
        return String(arg);
      }
    }),
  });
}

/**
 * A transferable copy of a frame.
 *
 * A view over a larger buffer cannot be transferred, and transferring the
 * whole backing store of a coalesced read would take unrelated frames with
 * it. Only an exactly-sized buffer is handed over as-is; anything else is
 * copied, which is the same copy the main thread used to make anyway.
 */
function detach(data: BlitTransportMessage): ArrayBuffer {
  if (data instanceof ArrayBuffer) return data;
  if (data.byteOffset === 0 && data.byteLength === data.buffer.byteLength) {
    return data.buffer as ArrayBuffer;
  }
  return data.slice().buffer as ArrayBuffer;
}

/**
 * Wire layout of an audio frame: tag, 32-bit millisecond timestamp, flags,
 * then the encoded payload. Parsed here so the payload can go straight to the
 * decoder without the main thread reading a byte of it.
 */
const S2C_AUDIO_FRAME = 0x30;
const AUDIO_HEADER_BYTES = 6;

function watch(id: number, channel: MuxChannel): void {
  channel.addEventListener("message", (data: BlitTransportMessage) => {
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data;
    const port = audioPorts.get(id);
    if (
      port &&
      bytes.length >= AUDIO_HEADER_BYTES &&
      bytes[0] === S2C_AUDIO_FRAME
    ) {
      const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.length);
      const payload = bytes.slice(AUDIO_HEADER_BYTES);
      // Straight to the decoder, in the shape it already accepts.
      port.postMessage(
        {
          type: "opus",
          timestamp: view.getUint32(1, true),
          data: payload,
        },
        [payload.buffer],
      );
      // The header still goes to the main thread. `handleAudioFrame` owns the
      // AudioContext — creating it, resuming it, rebuilding it after the
      // browser closes it — and none of that can happen in a worker. It is
      // sent without the payload because the payload has already gone the
      // short way, and it may arrive late without harm: the lifecycle work is
      // not on the critical path, which is the entire point.
      const header = bytes.slice(0, AUDIO_HEADER_BYTES).buffer;
      post({ t: "channelMessage", id, data: header }, [header]);
      return;
    }
    const buffer = detach(data);
    post({ t: "channelMessage", id, data: buffer }, [buffer]);
  });
  channel.addEventListener("statuschange", (status: ConnectionStatus) => {
    post({
      t: "channelStatus",
      id,
      status,
      authRejected: channel.authRejected,
      lastError: channel.lastError,
    });
  });
}

worker.onmessage = (event: MessageEvent<MuxWorkerRequest>) => {
  const message = event.data;
  if (!message) return;

  switch (message.t) {
    case "init": {
      if (transport) return;
      transport = new MuxTransport(message.wsUrl, message.passphrase, {
        ...message.options,
        // Diagnostics are relayed rather than dropped: the WebTransport
        // upgrade, the reconnect backoff and the auth outcome all report
        // through here, and losing them is exactly the wrong trade for the
        // path that is hardest to observe.
        debug: {
          log: (msg, ...args) => relay("log", msg, args),
          warn: (msg, ...args) => relay("warn", msg, args),
          error: (msg, ...args) => relay("error", msg, args),
        },
        // Reconnection stays in here with the socket. Reporting it upward and
        // driving it from the main thread would put the recovery path back on
        // the thread whose stalls this exists to survive.
      });
      return;
    }
    case "audioPort": {
      const port = event.ports[0];
      if (!port) return;
      // A replacement means the decoder was rebuilt; the old port leads
      // nowhere and must not keep the frames it is handed.
      audioPorts.get(message.id)?.close();
      audioPorts.set(message.id, port);
      return;
    }
    case "audioPortDetach": {
      audioPorts.get(message.id)?.close();
      audioPorts.delete(message.id);
      return;
    }
    case "connect":
      transport?.connect();
      return;
    case "close":
      transport?.close();
      return;
    case "updateWtCertHash":
      transport?.updateWtCertHash(message.hexHash, message.wtUrl);
      return;
    case "channelCreate": {
      if (!transport || channels.has(message.id)) return;
      const channel = transport.createChannel(message.destName, message.id);
      channels.set(message.id, channel);
      watch(message.id, channel);
      return;
    }
    case "channelConnect":
      channels.get(message.id)?.connect();
      return;
    case "channelReconnect":
      channels.get(message.id)?.reconnect();
      return;
    case "channelSend":
      channels.get(message.id)?.send(new Uint8Array(message.data));
      return;
    case "channelClose": {
      channels.get(message.id)?.close();
      channels.delete(message.id);
      audioPorts.get(message.id)?.close();
      audioPorts.delete(message.id);
      return;
    }
    case "channelSuspend":
      channels.get(message.id)?.suspend();
      return;
    case "cancelChannelConnectTimer":
      transport?.cancelChannelConnectTimer(message.id);
      return;
  }
};
