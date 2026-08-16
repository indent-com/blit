/**
 * Multiplexed transport (WebSocket with optional WebTransport upgrade).
 *
 * A single connection to the gateway carries traffic for all destinations.
 * Each destination gets a lightweight "channel" that implements
 * {@link BlitTransport} so it can be handed directly to a
 * {@link BlitConnection}.
 *
 * When a `wtUrl` is provided and the browser supports WebTransport, the
 * transport will try QUIC first and fall back to WebSocket on failure.
 *
 * Wire format (after authentication):
 *
 *   Data frame:    [channel_id:2 LE][blit_payload:N]
 *   Control frame: [0xFFFF][opcode:1][...]
 *
 * Over WebSocket each frame is a single binary message.
 * Over WebTransport frames are length-prefixed on a bidirectional stream:
 *   [frame_len:4 LE][mux_frame]
 *
 * Control opcodes:
 *   C2S  OPEN  0x01  [ch:2][name_len:2][name:N]
 *   C2S  CLOSE 0x02  [ch:2]
 *   S2C  OPENED 0x81 [ch:2]
 *   S2C  CLOSED 0x82 [ch:2]
 *   S2C  ERROR  0x83 [ch:2][msg_len:2][msg:N]
 */

import {
  noopDebug,
  S2C_SURFACE_FRAME,
  type BlitDebug,
  type BlitTransport,
  type BlitTransportMessage,
  type BlitTransportOptions,
  type ConnectionStatus,
} from "../types";
import { LengthPrefixedFrameDecoder } from "./length-prefixed";

// -- Protocol constants -----------------------------------------------------

const MUX_CONTROL = 0xffff;
const MUX_C2S_OPEN = 0x01;
const MUX_C2S_CLOSE = 0x02;
const MUX_S2C_OPENED = 0x81;
const MUX_S2C_CLOSED = 0x82;
const MUX_S2C_ERROR = 0x83;
const MAX_MUX_FRAME_LENGTH = 16 * 1024 * 1024;
// Bound the amount of transport work one fulfilled read can dump into the
// main thread. At 6–8 MB/s a 256 KiB read spans 30–40 ms and Chromium may
// deliver several complete video frames synchronously; 32 KiB keeps the
// dispatch slice around one frame without returning to per-packet churn.
const WT_BYOB_BUFFER_SIZE = 32 * 1024;

const textDecoder = new TextDecoder();

// -- MuxTransport -----------------------------------------------------------

export interface MuxTransportOptions extends BlitTransportOptions {
  /** WebTransport URL (e.g. `https://host:3264/mux`).  When set and the
   *  browser supports WebTransport, QUIC is tried first. */
  wtUrl?: string;
  /** SHA-256 cert hash (hex) for self-signed WebTransport certs. */
  wtCertHash?: string;
  /** Timeout for the optional WebTransport attempt before falling back to WebSocket. Default: 3000 ms. */
  wtConnectTimeoutMs?: number;
  /** Timeout waiting for a virtual channel OPEN acknowledgement before retrying. Default: 10000 ms. */
  channelConnectTimeoutMs?: number;
  /** How long to stay on WebSocket after a WebTransport attempt fails before
   *  probing QUIC again. Default: 300000 ms (5 min). */
  wtReprobeMs?: number;
  /** Optional debug logger for connection diagnostics. */
  debug?: BlitDebug;
}

/**
 * Manages a single multiplexed connection and exposes per-destination
 * channels that each implement {@link BlitTransport}.
 */
export class MuxTransport {
  private ws: WebSocket | null = null;
  // WebTransport state
  private wt: WebTransport | null = null;
  private wtWriter: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private wtReadAbort: AbortController | null = null;
  private bufferRecycler: Worker | null = null;

  private _status: ConnectionStatus = "disconnected";
  private _authRejected = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private currentDelay: number;
  private disposed = false;
  /** True while an async WT connect attempt is in progress. */
  private wtConnecting = false;

  private readonly wsUrl: string;
  private readonly passphrase: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;
  private wtUrl: string | undefined;
  private wtCertHash: Uint8Array | undefined;
  private readonly wtConnectTimeoutMs: number;
  private readonly channelConnectTimeoutMs: number;
  private readonly wtReprobeMs: number;
  /** Set after a WT failure to keep us on WebSocket. Cleared by
   *  `wtReprobeTimer` so a transient QUIC problem — a moment of UDP loss, a
   *  network that blocks it, a gateway still binding its endpoint — costs one
   *  cooldown rather than WebTransport for the life of the page. blit sessions
   *  run for days; a permanent flag turns a one-second event into a
   *  permanent downgrade. */
  private wtFailed = false;
  private wtReprobeTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly dbg: BlitDebug;
  /** Exact-size reusable WS frames for high-rate small messages (notably
   * surface ACKs). WebSocket.send snapshots BufferSource data synchronously. */
  private readonly wsSmallSendFrames = new Map<number, Uint8Array>();

  /** All channels keyed by channel ID. */
  private readonly channels = new Map<number, MuxChannel>();
  /** Next channel ID to assign. */
  private nextChannelId = 0;
  /** Channels that were open/opening when the connection dropped — need re-open on reconnect. */
  private readonly pendingReopen = new Set<MuxChannel>();
  /** Per-channel reconnect timers for channels that received S2C_CLOSED/ERROR. */
  private readonly channelReconnectTimers = new Map<
    number,
    ReturnType<typeof setTimeout>
  >();
  /** Per-channel timers while waiting for S2C_OPENED. */
  private readonly channelConnectTimers = new Map<
    number,
    ReturnType<typeof setTimeout>
  >();

  constructor(
    wsUrl: string,
    passphrase: string,
    options?: MuxTransportOptions,
  ) {
    this.wsUrl = wsUrl;
    this.passphrase = passphrase;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.currentDelay = this.initialDelay;
    // WebTransport is an optimization; if UDP/QUIC is blocked we should fall
    // back to WebSocket quickly rather than making first load sit on the
    // browser's long connection timeout. `connectTimeoutMs` remains an alias
    // for callers that use the generic BlitTransportOptions field.
    this.wtConnectTimeoutMs =
      options?.wtConnectTimeoutMs ?? options?.connectTimeoutMs ?? 3_000;
    // Opening a mux channel may involve SSH/WebRTC/proxy setup on the gateway.
    // If the first attempt wedges, retry automatically — this is the same
    // recovery path as pressing the Reconnect button, just without user action.
    this.channelConnectTimeoutMs =
      options?.channelConnectTimeoutMs ?? options?.connectTimeoutMs ?? 10_000;
    this.wtReprobeMs = options?.wtReprobeMs ?? 300_000;
    this.dbg = options?.debug ?? noopDebug;
    if (options?.wtUrl) {
      this.wtUrl = options.wtUrl;
    }
    if (options?.wtCertHash) {
      this.wtCertHash = hexToBytes(options.wtCertHash);
    }
  }

  /**
   * Adopt a rotated WebTransport certificate hash.
   *
   * The gateway regenerates its self-signed cert every 13 days and publishes
   * the new hash over the config WebSocket. Without this the hash captured at
   * construction goes stale, every later WT attempt fails cert validation, and
   * the session is stuck on WebSocket until the page is reloaded.
   *
   * Deliberately does not reconnect: tearing down a healthy connection to
   * switch protocols would interrupt live terminals for no gain. The new hash
   * is used by the next connection attempt, whenever that happens.
   */
  updateWtCertHash(hexHash: string, wtUrl?: string): void {
    if (this.disposed) return;
    const next = hexToBytes(hexHash);
    const urlChanged = !!wtUrl && wtUrl !== this.wtUrl;
    if (wtUrl) this.wtUrl = wtUrl;
    if (this.wtCertHash && bytesEqual(this.wtCertHash, next)) {
      // Config may publish the same certificate at a different authority.
      // A failure against the old endpoint says nothing about the new one.
      if (urlChanged) this.clearWtFailure();
      return;
    }
    this.dbg.log("adopting rotated WebTransport cert hash");
    this.wtCertHash = next;
    // A failure against the *old* hash says nothing about the new one.
    this.clearWtFailure();
  }

  /** Allow WebTransport to be probed again. */
  private clearWtFailure(): void {
    this.wtFailed = false;
    if (this.wtReprobeTimer !== null) {
      clearTimeout(this.wtReprobeTimer);
      this.wtReprobeTimer = null;
    }
  }

  /** Stay on WebSocket, but only until the cooldown expires. */
  private markWtFailed(): void {
    this.wtFailed = true;
    if (this.wtReprobeTimer !== null || this.disposed) return;
    this.wtReprobeTimer = setTimeout(() => {
      this.wtReprobeTimer = null;
      this.wtFailed = false;
      this.dbg.log("WebTransport cooldown expired, will probe QUIC again");
    }, this.wtReprobeMs);
  }

  /** Current transport-level status. */
  get status(): ConnectionStatus {
    return this._status;
  }

  /** True when connected over WebTransport (QUIC) rather than WebSocket. */
  get isWebTransport(): boolean {
    return this.wt !== null && this._status === "connected";
  }

  // -- Lifecycle ------------------------------------------------------------

  connect(): void {
    if (this.disposed) return;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
      this.currentDelay = this.initialDelay;
    }
    if (
      this._status === "connecting" ||
      this._status === "authenticating" ||
      this._status === "connected"
    )
      return;

    this.setStatus("connecting");

    // Try WebTransport first if available.
    if (this.shouldTryWt()) {
      this.dbg.log("attempting WebTransport to %s", this.wtUrl);
      this.connectWt();
      return;
    }

    this.dbg.log(
      "skipping WT (failed=%s, url=%s, api=%s), using WebSocket to %s",
      this.wtFailed,
      !!this.wtUrl,
      typeof WebTransport !== "undefined",
      this.wsUrl,
    );
    this.connectWs();
  }

  close(): void {
    this.disposed = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.wtReprobeTimer !== null) {
      clearTimeout(this.wtReprobeTimer);
      this.wtReprobeTimer = null;
    }
    for (const timer of this.channelReconnectTimers.values()) {
      clearTimeout(timer);
    }
    this.channelReconnectTimers.clear();
    for (const timer of this.channelConnectTimers.values()) {
      clearTimeout(timer);
    }
    this.channelConnectTimers.clear();
    for (const ch of this.channels.values()) {
      ch._setStatus("closed");
    }
    this.channels.clear();
    this.pendingReopen.clear();
    this.cleanupWs();
    this.cleanupWt();
    this.bufferRecycler?.terminate();
    this.bufferRecycler = null;
    this.setStatus("closed");
  }

  // -- Channel management ---------------------------------------------------

  /**
   * Create a channel for the given destination name.  The channel is not
   * opened until its `connect()` method is called (which happens
   * automatically when a {@link BlitConnection} is created with
   * `autoConnect: true`).
   */
  createChannel(destName: string, channelId?: number): MuxChannel {
    // The id may be supplied by the caller. When this transport runs inside a
    // worker the main thread has already handed a channel back to its own
    // caller — `createChannel` is synchronous and the id prefixes every frame,
    // so it cannot wait for a round trip — and both sides must agree on it.
    const id = channelId ?? this.nextChannelId++;
    if (channelId !== undefined && channelId >= this.nextChannelId) {
      this.nextChannelId = channelId + 1;
    }
    const ch = new MuxChannel(this, id, destName, this.initialDelay);
    this.channels.set(id, ch);
    return ch;
  }

  /**
   * Remove a channel.  Sends CLOSE if the underlying connection is open.
   * Called internally by {@link MuxChannel.close}.
   */
  _removeChannel(ch: MuxChannel): void {
    this.channels.delete(ch.channelId);
    this.pendingReopen.delete(ch);
    this._cancelChannelReconnect(ch.channelId);
    this.cancelChannelConnectTimer(ch.channelId);
    if (this._status === "connected") {
      this._sendClose(ch.channelId);
    }
  }

  /** @internal Pause one channel without removing it, so it can be reopened manually. */
  _suspendChannel(ch: MuxChannel): void {
    this.pendingReopen.delete(ch);
    this._cancelChannelReconnect(ch.channelId);
    this.cancelChannelConnectTimer(ch.channelId);
    if (
      this._status === "connected" &&
      (ch._internalStatus === "connected" ||
        ch._internalStatus === "connecting")
    ) {
      this._sendClose(ch.channelId);
    }
  }

  /** @internal Cancel any pending per-channel reconnect timer. */
  _cancelChannelReconnect(channelId: number): void {
    const timer = this.channelReconnectTimers.get(channelId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.channelReconnectTimers.delete(channelId);
    }
  }

  /** Send a raw mux frame.  Over WS this is a single binary message;
   *  over WT it is length-prefixed on the bidirectional stream. */
  _sendRaw(data: Uint8Array): void {
    if (this.wtWriter) {
      // Length-prefixed: [len:4 LE][data]
      const frame = new Uint8Array(4 + data.length);
      frame[0] = data.length & 0xff;
      frame[1] = (data.length >> 8) & 0xff;
      frame[2] = (data.length >> 16) & 0xff;
      frame[3] = (data.length >> 24) & 0xff;
      frame.set(data, 4);
      this.wtWriter.write(frame).catch(() => {});
      return;
    }
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(data as Uint8Array<ArrayBuffer>);
    }
  }

  /** Send one channel payload without first building an intermediate mux
   * frame. WT gets one combined allocation instead of two; WS reuses exact
   * small buffers after their first send. */
  _sendChannel(channelId: number, data: Uint8Array): void {
    const muxLength = 2 + data.length;
    if (this.wtWriter) {
      const frame = new Uint8Array(4 + muxLength);
      frame[0] = muxLength & 0xff;
      frame[1] = (muxLength >> 8) & 0xff;
      frame[2] = (muxLength >> 16) & 0xff;
      frame[3] = (muxLength >> 24) & 0xff;
      frame[4] = channelId & 0xff;
      frame[5] = (channelId >> 8) & 0xff;
      frame.set(data, 6);
      this.wtWriter.write(frame).catch(() => {});
      return;
    }
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    let frame: Uint8Array;
    if (muxLength <= 64) {
      const key = channelId * 65 + muxLength;
      const cached = this.wsSmallSendFrames.get(key);
      if (cached) frame = cached;
      else {
        frame = new Uint8Array(muxLength);
        this.wsSmallSendFrames.set(key, frame);
      }
    } else {
      frame = new Uint8Array(muxLength);
    }
    frame[0] = channelId & 0xff;
    frame[1] = (channelId >> 8) & 0xff;
    frame.set(data, 2);
    this.ws.send(frame as Uint8Array<ArrayBuffer>);
  }

  /** Send an OPEN control message for a channel. */
  _sendOpen(ch: MuxChannel): void {
    if (ch._suspended) return;
    if (this._status !== "connected") {
      this.pendingReopen.add(ch);
      this.connect();
      return;
    }
    const nameBytes = new TextEncoder().encode(ch.destName);
    const buf = new Uint8Array(2 + 1 + 2 + 2 + nameBytes.length);
    const view = new DataView(buf.buffer);
    view.setUint16(0, MUX_CONTROL, true);
    buf[2] = MUX_C2S_OPEN;
    view.setUint16(3, ch.channelId, true);
    view.setUint16(5, nameBytes.length, true);
    buf.set(nameBytes, 7);
    this._sendRaw(buf);
    this.armChannelConnectTimer(ch);
  }

  // -- Internal: WebSocket --------------------------------------------------

  private connectWs(): void {
    this.dbg.log("opening WebSocket to %s", this.wsUrl);
    const socket = new WebSocket(this.wsUrl);
    socket.binaryType = "arraybuffer";

    if (this.ws && this.ws !== socket) {
      try {
        this.ws.onclose = null;
        this.ws.close();
      } catch {
        /* ignore */
      }
    }
    this.ws = socket;

    let authenticated = false;

    socket.onopen = () => {
      if (this.ws !== socket || this.disposed) return;
      this.setStatus("authenticating");
      socket.send(this.passphrase);
    };

    socket.onmessage = (e: MessageEvent) => {
      if (this.ws !== socket || this.disposed) return;

      if (typeof e.data === "string") {
        if (e.data === "mux") {
          this.dbg.log("WebSocket authenticated");
          authenticated = true;
          this.clearAuthRejection();
          this.setStatus("connected");
          this.currentDelay = this.initialDelay;
          this.reopenChannels();
        } else if (e.data === "busy") {
          // The server's auth throttle refused the handshake before looking at
          // the passphrase. Nothing is wrong with the credential, so this is an
          // ordinary disconnect: back off and retry.
          this.dbg.warn("WebSocket auth throttled, will retry");
          socket.close();
        } else if (e.data === "auth") {
          this.dbg.warn("WebSocket auth rejected");
          this._authRejected = true;
          for (const ch of this.channels.values()) {
            ch._setAuthRejected();
          }
          this.setStatus("error");
          socket.close();
        } else {
          this.setStatus("error");
          socket.close();
        }
        return;
      }

      if (authenticated && e.data instanceof ArrayBuffer) {
        this.handleMuxFrame(new Uint8Array(e.data));
        // All mux consumers honor the synchronous borrowed-view contract.
        // Transfer the now-dead backing store so its reclamation is charged
        // to the recycler worker instead of a video-presenting main-thread GC.
        this.recycleBuffer(e.data);
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
      this.ws = null;
      this.handleDisconnect();
    };
  }

  private cleanupWs(): void {
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.onerror = null;
      this.ws.onmessage = null;
      this.ws.onopen = null;
      this.ws.close();
      this.ws = null;
    }
  }

  // -- Internal: WebTransport -----------------------------------------------

  private shouldTryWt(): boolean {
    return (
      !this.wtFailed && !!this.wtUrl && typeof WebTransport !== "undefined"
    );
  }

  private connectWt(): void {
    if (this.wtConnecting) return;
    this.wtConnecting = true;
    this.connectWtAsync()
      .catch(() => {})
      .finally(() => {
        this.wtConnecting = false;
      });
  }

  private async connectWtAsync(): Promise<void> {
    if (this.disposed || !this.wtUrl) return;

    try {
      const opts: WebTransportOptions = {};
      if (this.wtCertHash) {
        opts.serverCertificateHashes = [
          {
            algorithm: "sha-256",
            value: this.wtCertHash.buffer as ArrayBuffer,
          },
        ];
      }

      const wt = new WebTransport(this.wtUrl, opts);
      this.wt = wt;
      await Promise.race([
        wt.ready,
        new Promise((_, reject) =>
          setTimeout(
            () => reject(new Error("WT connect timeout")),
            this.wtConnectTimeoutMs,
          ),
        ),
      ]);

      if (this.disposed) {
        wt.close();
        return;
      }

      // Open a bidirectional stream for the mux protocol.
      const stream = await wt.createBidirectionalStream();
      const writer = stream.writable.getWriter();
      const reader = stream.readable.getReader();

      // Authenticate: [pass_len:2 LE][passphrase] → [1/0]
      this.setStatus("authenticating");
      const passBytes = new TextEncoder().encode(this.passphrase);
      const authMsg = new Uint8Array(2 + passBytes.length);
      authMsg[0] = passBytes.length & 0xff;
      authMsg[1] = (passBytes.length >> 8) & 0xff;
      authMsg.set(passBytes, 2);
      await writer.write(authMsg);

      // Read 1-byte auth response.
      const { data: authResp, remainder } = await readExactBuffered(reader, 1);
      if (!authResp) {
        // EOF is a broken handshake, not a credential verdict. Let the normal
        // WT failure path fall back to WebSocket instead of permanently
        // parking every channel in authRejected.
        throw new Error("WebTransport closed during authentication");
      }
      if (authResp[0] !== 1) {
        this.dbg.warn("WebTransport auth rejected (resp=%s)", authResp[0]);
        this._authRejected = true;
        for (const ch of this.channels.values()) {
          ch._setAuthRejected();
        }
        this.setStatus("error");
        wt.close();
        this.wt = null;
        return;
      }

      if (this.disposed) {
        wt.close();
        this.wt = null;
        return;
      }

      this.dbg.log("WebTransport connected and authenticated");
      this.wtWriter = writer;
      // QUIC works here — retire any cooldown so a future failure starts a
      // fresh one rather than inheriting a stale timer.
      this.clearWtFailure();
      this.clearAuthRejection();
      this.currentDelay = this.initialDelay;
      this.setStatus("connected");
      this.reopenChannels();

      // Start read loop in background.
      const abort = new AbortController();
      this.wtReadAbort = abort;
      reader.releaseLock();
      let dataReader:
        | ReadableStreamDefaultReader<Uint8Array>
        | ReadableStreamBYOBReader;
      let byob = false;
      try {
        dataReader = stream.readable.getReader({ mode: "byob" });
        byob = true;
      } catch {
        // Older WebTransport implementations may not expose a byte stream.
        dataReader = stream.readable.getReader();
      }
      this.wtReadLoop(dataReader, wt, abort.signal, remainder, byob);

      // Handle connection close.
      wt.closed
        .then(() => {
          if (this.wt !== wt || this.disposed) return;
          this.cleanupWt();
          this.handleDisconnect();
        })
        .catch(() => {
          if (this.wt !== wt || this.disposed) return;
          this.cleanupWt();
          this.handleDisconnect();
        });
    } catch (err) {
      // WT failed — fall back to WS for the cooldown, then probe QUIC again.
      this.dbg.warn(
        "WebTransport failed, falling back to WebSocket: %s",
        err instanceof Error ? err.message : String(err),
      );
      this.markWtFailed();
      this.cleanupWt();
      if (this.disposed) return;
      if (this._authRejected) {
        this.setStatus("error");
        return;
      }
      // Fall back to WS immediately (don't schedule reconnect — we haven't
      // been connected yet).
      this.connectWs();
    }
  }

  private wtReadLoop(
    reader: ReadableStreamDefaultReader<Uint8Array> | ReadableStreamBYOBReader,
    wt: WebTransport,
    signal: AbortSignal,
    initialBuffer: Uint8Array,
    byob: boolean,
  ): void {
    // Run the async read loop in the background. A bidirectional stream can
    // end without the enclosing WebTransport session closing, so recover from
    // stream EOF/error here rather than relying only on `wt.closed`.
    (async () => {
      const decoder = new LengthPrefixedFrameDecoder(
        MAX_MUX_FRAME_LENGTH,
        (frame) => this.handleMuxFrame(frame),
      );
      let byobBuffer = new ArrayBuffer(WT_BYOB_BUFFER_SIZE);
      try {
        if (initialBuffer.length > 0 && !decoder.push(initialBuffer)) {
          throw new Error("invalid WebTransport frame");
        }
        while (!signal.aborted) {
          const { value, done } = byob
            ? await (reader as ReadableStreamBYOBReader).read(
                new Uint8Array(byobBuffer),
              )
            : await (reader as ReadableStreamDefaultReader<Uint8Array>).read();
          if (signal.aborted || this.wt !== wt) return;
          if (done) throw new Error("WebTransport receive stream closed");
          if (!value || value.length === 0) continue;
          if (!decoder.push(value)) {
            throw new Error("invalid WebTransport frame");
          }
          if (byob) {
            // BYOB transfers the supplied ArrayBuffer and returns ownership in
            // `value`. Reuse it after synchronous frame dispatch; this avoids
            // allocating a receive buffer for every network chunk.
            byobBuffer =
              value.buffer instanceof ArrayBuffer &&
              value.buffer.byteLength >= WT_BYOB_BUFFER_SIZE
                ? value.buffer
                : new ArrayBuffer(WT_BYOB_BUFFER_SIZE);
          }
        }
      } catch (err) {
        if (signal.aborted || this.disposed || this.wt !== wt) return;
        this.dbg.warn(
          "WebTransport receive stream failed: %s",
          err instanceof Error ? err.message : String(err),
        );
        this.markWtFailed();
        this.cleanupWt();
        this.handleDisconnect();
      }
    })();
  }

  private cleanupWt(): void {
    this.wtWriter = null;
    if (this.wtReadAbort) {
      this.wtReadAbort.abort();
      this.wtReadAbort = null;
    }
    if (this.wt) {
      try {
        this.wt.close();
      } catch {}
      this.wt = null;
    }
  }

  private recycleBuffer(buffer: ArrayBuffer): void {
    if (typeof Worker === "undefined" || buffer.byteLength === 0) return;
    try {
      if (!this.bufferRecycler) {
        // `.js`, not `.ts`: tsc copies this literal into dist verbatim, and the
        // published package resolves from dist. Vite maps it back to the `.ts`
        // source when core is consumed from src inside this repo.
        this.bufferRecycler = new Worker(
          new URL("./buffer-recycler-worker.js", import.meta.url),
          { type: "module", name: "blit-buffer-recycler" },
        );
      }
      this.bufferRecycler.postMessage(buffer, [buffer]);
    } catch {
      // Recycling is an optimization. A CSP or browser without module workers
      // keeps the ordinary GC path without affecting transport correctness.
    }
  }

  // -- Internal: shared -----------------------------------------------------

  private setStatus(status: ConnectionStatus): void {
    if (this._status === status) return;
    const prev = this._status;
    this._status = status;
    this.dbg.log("mux status %s → %s", prev, status);
  }

  private handleDisconnect(): void {
    // Cancel per-channel reconnect timers — the transport-level reconnect
    // will re-open all channels via reopenChannels().
    for (const timer of this.channelReconnectTimers.values()) {
      clearTimeout(timer);
    }
    this.channelReconnectTimers.clear();
    for (const timer of this.channelConnectTimers.values()) {
      clearTimeout(timer);
    }
    this.channelConnectTimers.clear();
    if (this._authRejected) {
      this.setStatus("disconnected");
      return;
    }
    for (const ch of this.channels.values()) {
      if (ch._internalStatus !== "closed" && !ch._suspended) {
        this.pendingReopen.add(ch);
      }
      ch._setStatus("disconnected");
    }
    this.setStatus("disconnected");
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.disposed || !this._reconnect) return;
    if (this.reconnectTimer !== null) return;
    this.dbg.log("scheduling reconnect in %dms", this.currentDelay);
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

  /**
   * Schedule a re-open attempt for a single channel after it received
   * S2C_CLOSED or S2C_ERROR.  Uses per-channel exponential backoff.
   */
  private scheduleChannelReconnect(ch: MuxChannel): void {
    if (this.disposed || !this._reconnect) return;
    if (ch._internalStatus === "closed" || ch._suspended) return;
    if (this.channelReconnectTimers.has(ch.channelId)) return;
    const delay = ch._reconnectDelay;
    ch._reconnectDelay = Math.min(delay * this.backoff, this.maxDelay);
    this.channelReconnectTimers.set(
      ch.channelId,
      setTimeout(() => {
        this.channelReconnectTimers.delete(ch.channelId);
        if (this.disposed || ch._suspended || !this.channels.has(ch.channelId))
          return;
        if (
          ch._internalStatus === "closed" ||
          ch._internalStatus === "connected" ||
          ch._internalStatus === "connecting"
        )
          return;
        if (this._status === "connected") {
          ch._setStatus("connecting");
          this._sendOpen(ch);
        } else {
          // Not connected — queue for when it reconnects.
          this.pendingReopen.add(ch);
        }
      }, delay),
    );
  }

  private armChannelConnectTimer(ch: MuxChannel): void {
    if (this.channelConnectTimeoutMs <= 0) return;
    this.cancelChannelConnectTimer(ch.channelId);
    this.channelConnectTimers.set(
      ch.channelId,
      setTimeout(() => {
        this.channelConnectTimers.delete(ch.channelId);
        if (this.disposed || ch._suspended || !this.channels.has(ch.channelId))
          return;
        if (ch._internalStatus !== "connecting") return;

        ch._lastError = "connect timeout";
        this.dbg.warn(
          "channel %d (%s) open timed out after %dms; retrying",
          ch.channelId,
          ch.destName,
          this.channelConnectTimeoutMs,
        );

        // Ask the gateway to cancel any in-flight OPEN. Gateways that support
        // pending-open cancellation will abort the slow attempt; older/local
        // handlers ignore it until their connect attempt returns, which is no
        // worse than the previous stuck state.
        if (this._status === "connected") {
          this._sendClose(ch.channelId);
        } else {
          this.pendingReopen.add(ch);
        }

        ch._setStatus("disconnected");
        this.scheduleChannelReconnect(ch);
      }, this.channelConnectTimeoutMs),
    );
  }

  cancelChannelConnectTimer(channelId: number): void {
    const timer = this.channelConnectTimers.get(channelId);
    if (timer !== undefined) {
      clearTimeout(timer);
      this.channelConnectTimers.delete(channelId);
    }
  }

  /** @internal */
  _sendClose(channelId: number): void {
    const buf = new Uint8Array(5);
    const view = new DataView(buf.buffer);
    view.setUint16(0, MUX_CONTROL, true);
    buf[2] = MUX_C2S_CLOSE;
    view.setUint16(3, channelId, true);
    this._sendRaw(buf);
  }

  /**
   * A successful (re-)authentication retires any earlier rejection. Channels
   * parked in `error` by a rejection were never queued for re-open — handleDisconnect
   * returns before touching `pendingReopen` when `_authRejected` is set — so
   * requeue them here or the transport reconnects with every channel wedged.
   */
  private clearAuthRejection(): void {
    if (!this._authRejected) return;
    this._authRejected = false;
    for (const ch of this.channels.values()) {
      if (ch._internalStatus === "closed" || ch._suspended) continue;
      ch._clearAuthRejected();
      this.pendingReopen.add(ch);
    }
  }

  private reopenChannels(): void {
    for (const ch of this.pendingReopen) {
      if (ch._suspended) continue;
      ch._setStatus("connecting");
      this._sendOpen(ch);
    }
    this.pendingReopen.clear();
  }

  private handleMuxFrame(bytes: Uint8Array): void {
    if (bytes.byteLength < 2) return;
    const chId = bytes[0] | (bytes[1] << 8);

    if (chId === MUX_CONTROL) {
      this.handleControl(bytes);
    } else {
      const ch = this.channels.get(chId);
      if (ch) {
        // A view strips the prefix without copying the encoded frame.
        ch._deliverMessage(bytes.subarray(2));
      }
    }
  }

  private handleControl(bytes: Uint8Array): void {
    if (bytes.length < 5) return;
    const opcode = bytes[2];
    const chId = bytes[3] | (bytes[4] << 8);
    const ch = this.channels.get(chId);

    switch (opcode) {
      case MUX_S2C_OPENED:
        if (ch && !ch._suspended) {
          this.cancelChannelConnectTimer(ch.channelId);
          ch._lastError = null;
          ch._reconnectDelay = this.initialDelay;
          ch._setStatus("connected");
        }
        break;

      case MUX_S2C_CLOSED:
        if (ch && !ch._suspended && ch._internalStatus !== "connecting") {
          this.cancelChannelConnectTimer(ch.channelId);
          ch._setStatus("disconnected");
          this.scheduleChannelReconnect(ch);
        }
        break;

      case MUX_S2C_ERROR: {
        if (bytes.length < 7) break;
        const msgLen = bytes[5] | (bytes[6] << 8);
        const msg =
          bytes.length >= 7 + msgLen
            ? textDecoder.decode(bytes.subarray(7, 7 + msgLen))
            : "unknown error";
        if (ch && !ch._suspended) {
          this.cancelChannelConnectTimer(ch.channelId);
          ch._lastError = msg;
          ch._setStatus("error");
          this.scheduleChannelReconnect(ch);
        }
        break;
      }
    }
  }
}

// -- MuxChannel -------------------------------------------------------------

/**
 * A single virtual channel on a {@link MuxTransport}.
 * Implements {@link BlitTransport} so it can be used directly by
 * {@link BlitConnection}.
 */
export class MuxChannel implements BlitTransport {
  /** @internal */ _internalStatus: ConnectionStatus = "disconnected";
  /** @internal */ _lastError: string | null = null;
  /** @internal Per-channel backoff delay for reconnect scheduling. */
  /** @internal */ _reconnectDelay: number;
  /** @internal */ _suspended = false;

  private readonly mux: MuxTransport;
  readonly channelId: number;
  readonly destName: string;
  private _authRejected = false;
  private messageListeners = new Set<(data: BlitTransportMessage) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();

  constructor(
    mux: MuxTransport,
    channelId: number,
    destName: string,
    initialDelay: number,
  ) {
    this.mux = mux;
    this.channelId = channelId;
    this.destName = destName;
    this._reconnectDelay = initialDelay;
  }

  get status(): ConnectionStatus {
    return this._internalStatus;
  }

  get authRejected(): boolean {
    return this._authRejected;
  }

  get lastError(): string | null {
    return this._lastError;
  }

  connect(): void {
    if (
      this._internalStatus === "connecting" ||
      this._internalStatus === "connected" ||
      this._internalStatus === "closed"
    )
      return;
    this._suspended = false;
    this.mux._cancelChannelReconnect(this.channelId);
    this._setStatus("connecting");
    this.mux._sendOpen(this);
  }

  reconnect(): void {
    if (this._internalStatus === "closed") return;
    this._suspended = false;
    this.mux._cancelChannelReconnect(this.channelId);
    this.mux.cancelChannelConnectTimer(this.channelId);
    // Ask the server to tear down the existing channel.
    if (
      this._internalStatus === "connected" ||
      this._internalStatus === "connecting"
    ) {
      this.mux._sendClose(this.channelId);
    }
    this._setStatus("disconnected");
    // Immediately reopen.
    this._setStatus("connecting");
    this.mux._sendOpen(this);
  }

  send(data: Uint8Array): void {
    if (this._internalStatus !== "connected") return;
    this.mux._sendChannel(this.channelId, data);
  }

  close(): void {
    if (this._internalStatus === "closed") return;
    this.mux._removeChannel(this);
    this._setStatus("closed");
  }

  suspend(): void {
    if (this._internalStatus === "closed") return;
    this._suspended = true;
    this.mux._suspendChannel(this);
    this._setStatus("disconnected");
  }

  addEventListener(
    type: "message",
    listener: (data: BlitTransportMessage) => void,
  ): void;
  addEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  addEventListener(type: string, listener: (...args: never[]) => void): void {
    if (type === "message") {
      this.messageListeners.add(
        listener as (data: BlitTransportMessage) => void,
      );
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener(
    type: "message",
    listener: (data: BlitTransportMessage) => void,
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
      this.messageListeners.delete(
        listener as (data: BlitTransportMessage) => void,
      );
    } else if (type === "statuschange") {
      this.statusListeners.delete(
        listener as (status: ConnectionStatus) => void,
      );
    }
  }

  // -- Internal (called by MuxTransport) ------------------------------------

  /** @internal */
  _setStatus(status: ConnectionStatus): void {
    if (this._internalStatus === status) return;
    if (this._internalStatus === "closed") return; // terminal
    this._internalStatus = status;
    for (const l of this.statusListeners) l(status);
  }

  /** @internal */
  _setAuthRejected(): void {
    this._authRejected = true;
    this._lastError = "Authentication failed";
    this._setStatus("error");
  }

  /** @internal */
  _clearAuthRejected(): void {
    this._authRejected = false;
    this._lastError = null;
  }

  /** @internal */
  _deliverMessage(data: BlitTransportMessage): void {
    // A listener may suspend the channel after a terminal protocol message
    // such as S2C_KICKED.  The gateway can already have queued the upstream
    // EOF's synthetic S2C_QUIT behind it; delivering that would make
    // BlitConnection reconnect immediately and undo the suspension.
    if (this._suspended) return;
    for (const l of this.messageListeners) l(data);
  }
}

// -- Helpers ----------------------------------------------------------------

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[^0-9a-fA-F]/g, "");
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

/** Read exactly `n` bytes and retain anything coalesced after them. */
async function readExactBuffered(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  n: number,
): Promise<{ data: Uint8Array | null; remainder: Uint8Array }> {
  const buf = new Uint8Array(n);
  let offset = 0;
  let remainder: Uint8Array = new Uint8Array(0);
  while (offset < n) {
    const { value, done } = await reader.read();
    if (done || !value) {
      return { data: null, remainder: new Uint8Array(0) };
    }
    const take = Math.min(value.length, n - offset);
    buf.set(value.subarray(0, take), offset);
    offset += take;
    remainder = value.subarray(take);
  }
  return { data: buf, remainder };
}
