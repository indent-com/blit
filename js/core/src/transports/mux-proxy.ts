/**
 * Main-thread face of the transport worker.
 *
 * `WorkerMuxTransport` stands in for `MuxTransport` and `WorkerMuxChannel` for
 * `MuxChannel`, so callers see the shape they already use. What changes is
 * where the socket lives: in the worker, where a frame can arrive while this
 * thread is busy decoding and painting a half-megabyte surface frame.
 *
 * Channel ids are allocated here rather than in the worker. `createChannel`
 * has to hand a channel back synchronously, and the id prefixes every frame on
 * the wire, so it cannot wait for a round trip; the worker is told which id to
 * use and agrees by construction.
 */

import type {
  MuxWorkerEvent,
  MuxWorkerOptions,
  MuxWorkerRequest,
} from "./mux-worker-protocol";
import type {
  BlitDebug,
  BlitTransport,
  BlitTransportMessage,
  ConnectionStatus,
} from "../types";

/**
 * The subset of the options that can cross to a worker.
 *
 * Everything here is a string, number or boolean by construction. Callers
 * pass richer objects — `debug` is a logger with methods — and a method does
 * not survive structured clone: `postMessage` throws `DataCloneError`, the
 * constructor fails, and the caller silently gets the direct transport
 * instead. Picking the fields by name rather than deleting the known-bad ones
 * keeps a future option from reintroducing that by accident.
 */
function wireOptions(options?: MuxWorkerOptions): MuxWorkerOptions | undefined {
  if (!options) return undefined;
  const {
    reconnect,
    reconnectDelay,
    maxReconnectDelay,
    reconnectBackoff,
    connectTimeoutMs,
    wtConnectTimeoutMs,
    channelConnectTimeoutMs,
    wtReprobeMs,
    wtUrl,
    wtCertHash,
  } = options;
  return {
    reconnect,
    reconnectDelay,
    maxReconnectDelay,
    reconnectBackoff,
    connectTimeoutMs,
    wtConnectTimeoutMs,
    channelConnectTimeoutMs,
    wtReprobeMs,
    wtUrl,
    wtCertHash,
  };
}

type MessageListener = (data: BlitTransportMessage) => void;
type StatusListener = (status: ConnectionStatus) => void;

/** A channel whose socket lives in the worker. */
class WorkerMuxChannel implements BlitTransport {
  readonly channelId: number;
  readonly destName: string;

  private _status: ConnectionStatus = "disconnected";
  private _authRejected = false;
  private _lastError: string | null = null;
  private readonly messageListeners = new Set<MessageListener>();
  private readonly statusListeners = new Set<StatusListener>();

  constructor(
    private readonly owner: WorkerMuxTransport,
    channelId: number,
    destName: string,
  ) {
    this.channelId = channelId;
    this.destName = destName;
  }

  get status(): ConnectionStatus {
    return this._status;
  }

  get authRejected(): boolean {
    return this._authRejected;
  }

  get lastError(): string | null {
    return this._lastError;
  }

  connect(): void {
    this.owner._send({ t: "channelConnect", id: this.channelId });
  }

  reconnect(): void {
    this.owner._send({ t: "channelReconnect", id: this.channelId });
  }

  send(data: Uint8Array): void {
    // Copied, then transferred: the caller owns its buffer and frequently
    // reuses it, and a transfer would leave it detached under them.
    const copy = data.slice();
    this.owner._send(
      {
        t: "channelSend",
        id: this.channelId,
        data: copy.buffer as ArrayBuffer,
      },
      [copy.buffer as ArrayBuffer],
    );
  }

  close(): void {
    this.owner._send({ t: "channelClose", id: this.channelId });
    this.owner._forget(this.channelId);
  }

  suspend(): void {
    this.owner._send({ t: "channelSuspend", id: this.channelId });
  }

  /**
   * Route this channel's audio frames straight to a decoder.
   *
   * Exposed on the channel so a consumer can opt in without knowing anything
   * about the worker behind it — `BlitConnection` owns the `AudioPlayer` and
   * feature-detects this, and a transport that has no worker simply does not
   * offer it.
   */
  attachAudioPort(port: MessagePort): void {
    this.owner.attachAudioPort(this.channelId, port);
  }

  /** Send this channel's audio back through the main thread. */
  detachAudioPort(): void {
    this.owner.detachAudioPort(this.channelId);
  }

  addEventListener(type: "message", listener: MessageListener): void;
  addEventListener(type: "statuschange", listener: StatusListener): void;
  addEventListener(type: string, listener: (...args: never[]) => void): void {
    if (type === "message") {
      this.messageListeners.add(listener as unknown as MessageListener);
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as unknown as StatusListener);
    }
  }

  removeEventListener(type: "message", listener: MessageListener): void;
  removeEventListener(type: "statuschange", listener: StatusListener): void;
  removeEventListener(
    type: string,
    listener: (...args: never[]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.delete(listener as unknown as MessageListener);
    } else if (type === "statuschange") {
      this.statusListeners.delete(listener as unknown as StatusListener);
    }
  }

  /** @internal */
  _deliver(data: BlitTransportMessage): void {
    for (const listener of this.messageListeners) listener(data);
  }

  /** @internal */
  _setStatus(
    status: ConnectionStatus,
    authRejected: boolean,
    lastError: string | null,
  ): void {
    this._status = status;
    this._authRejected = authRejected;
    this._lastError = lastError;
    for (const listener of this.statusListeners) listener(status);
  }
}

/**
 * Drop-in for `MuxTransport` that runs the real one in a worker.
 *
 * The surface is deliberately only what callers use — `connect`,
 * `createChannel`, `updateWtCertHash`, `close` — rather than a mirror of
 * every method, so there is no pretence of proxying behaviour that is not
 * exercised.
 */
export class WorkerMuxTransport {
  private readonly worker: Worker;
  private readonly debug?: BlitDebug;
  private readonly channels = new Map<number, WorkerMuxChannel>();
  private nextChannelId = 0;
  private closed = false;

  constructor(
    wsUrl: string,
    passphrase: string,
    options?: MuxWorkerOptions & { debug?: BlitDebug },
    /** Test seam: a worker environment is not available under jsdom, and the
     *  routing this class exists for is worth testing without one. */
    spawn?: () => Worker,
  ) {
    this.worker = spawn
      ? spawn()
      : new Worker(new URL("./mux-worker.js", import.meta.url), {
          type: "module",
          name: "blit-transport",
        });
    this.worker.onmessage = (event: MessageEvent<MuxWorkerEvent>) => {
      this.receive(event.data);
    };
    // A worker that fails to load does so asynchronously, long after the
    // constructor returned, and every channel would then sit at
    // "connecting" forever with nothing to explain it. Report it as the
    // failure it is so the UI and the reconnect logic can see it.
    this.worker.onerror = () => {
      for (const channel of this.channels.values()) {
        channel._setStatus("disconnected", false, "transport worker failed");
      }
    };
    this.debug = options?.debug;
    this._send({
      t: "init",
      wsUrl,
      passphrase,
      options: wireOptions(options),
    });
  }

  /**
   * Hand the worker a port that reaches one channel's audio decoder directly.
   *
   * Without this an audio frame still waits for this thread to dispatch it,
   * which is the whole failure being fixed: the socket having moved does not
   * help audio if the last hop is still a main-thread callback. After this the
   * main thread sees only the six-byte header, which is all the AudioContext
   * lifecycle needs and cheap enough to arrive late.
   */
  attachAudioPort(channelId: number, port: MessagePort): void {
    if (this.closed) return;
    this.worker.postMessage({ t: "audioPort", id: channelId }, [port]);
  }

  /**
   * Undo `attachAudioPort`: the decoder behind the port is gone, so the
   * worker must stop feeding it and let audio take the ordinary route.
   */
  detachAudioPort(channelId: number): void {
    this._send({ t: "audioPortDetach", id: channelId });
  }

  connect(): void {
    this._send({ t: "connect" });
  }

  createChannel(destName: string): WorkerMuxChannel {
    const id = this.nextChannelId++;
    const channel = new WorkerMuxChannel(this, id, destName);
    this.channels.set(id, channel);
    this._send({ t: "channelCreate", id, destName });
    return channel;
  }

  updateWtCertHash(hexHash: string, wtUrl?: string): void {
    this._send({ t: "updateWtCertHash", hexHash, wtUrl });
  }

  close(): void {
    if (this.closed) return;
    // Tell the worker before latching `closed`, which gates `_send`. Setting
    // it first swallows the very message that closes the socket, and the
    // worker is terminated a line later so nothing else would ever send it.
    this._send({ t: "close" });
    this.closed = true;
    this.worker.terminate();
    this.channels.clear();
  }

  /** @internal */
  _send(request: MuxWorkerRequest, transfer?: Transferable[]): void {
    if (this.closed) return;
    this.worker.postMessage(request, transfer ?? []);
  }

  /** @internal */
  _forget(channelId: number): void {
    this.channels.delete(channelId);
  }

  private receive(event: MuxWorkerEvent): void {
    switch (event.t) {
      case "channelMessage": {
        // A Uint8Array view, matching what MuxChannel delivered: callers parse
        // with byte offsets and a bare ArrayBuffer would change their meaning.
        this.channels.get(event.id)?._deliver(new Uint8Array(event.data));
        return;
      }
      case "channelStatus": {
        this.channels
          .get(event.id)
          ?._setStatus(event.status, event.authRejected, event.lastError);
        return;
      }
      case "debug": {
        this.debug?.[event.level](event.msg, ...event.args);
        return;
      }
      case "status":
        return;
    }
  }
}

export type { WorkerMuxChannel };
