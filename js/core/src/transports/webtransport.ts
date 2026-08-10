import type {
  BlitTransport,
  BlitTransportData,
  BlitTransportOptions,
  ConnectionStatus,
} from "../types";
import { LengthPrefixedFrameDecoder } from "./length-prefixed";

const MAX_FRAME_LENGTH = 16 * 1024 * 1024;
// Keep each synchronous decoder dispatch slice short at high video rates.
// Split frames are copied into the decoder's reusable accumulator, so this
// does not reintroduce one allocation per transport chunk.
const WT_BYOB_BUFFER_SIZE = 32 * 1024;

export interface WebTransportTransportOptions extends BlitTransportOptions {
  /**
   * SHA-256 hash of the server's TLS certificate (hex string).
   * Required for self-signed certs (e.g. auto-generated gateway certs).
   */
  serverCertificateHash?: string;
}

/**
 * BlitTransport implementation over WebTransport (QUIC/HTTP3).
 *
 * The protocol is length-prefixed binary frames on a single bidirectional
 * QUIC stream, with a 2-byte-length + passphrase auth handshake.
 */
export class WebTransportTransport implements BlitTransport {
  private wt: WebTransport | null = null;
  private writer: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private _status: ConnectionStatus = "disconnected";
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private connectPromise: Promise<void> | null = null;
  private currentDelay: number;
  private disposed = false;
  private messageListeners = new Set<(data: BlitTransportData) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  /** True when the server explicitly rejected the passphrase. */
  authRejected = false;
  /** Last error message, if any. */
  lastError: string | null = null;

  private readonly url: string;
  private readonly passphrase: string;
  private readonly _reconnect: boolean;
  private readonly initialDelay: number;
  private readonly maxDelay: number;
  private readonly backoff: number;
  private readonly connectTimeoutMs: number;
  private readonly certHash?: Uint8Array;

  constructor(
    url: string,
    passphrase: string,
    options?: WebTransportTransportOptions,
  ) {
    this.url = url;
    this.passphrase = passphrase;
    this._reconnect = options?.reconnect ?? true;
    this.initialDelay = options?.reconnectDelay ?? 500;
    this.maxDelay = options?.maxReconnectDelay ?? 10000;
    this.backoff = options?.reconnectBackoff ?? 1.5;
    this.connectTimeoutMs = options?.connectTimeoutMs ?? 10_000;
    this.currentDelay = this.initialDelay;
    if (options?.serverCertificateHash) {
      this.certHash = hexToBytes(options.serverCertificateHash);
    }
  }

  get status(): ConnectionStatus {
    return this._status;
  }

  send(data: Uint8Array): void {
    if (!this.writer) return;
    // Length-prefixed: 4-byte LE length + payload
    const frame = new Uint8Array(4 + data.length);
    frame[0] = data.length & 0xff;
    frame[1] = (data.length >> 8) & 0xff;
    frame[2] = (data.length >> 16) & 0xff;
    frame[3] = (data.length >> 24) & 0xff;
    frame.set(data, 4);
    this.writer.write(frame).catch(() => {});
  }

  close(): void {
    this.disposed = true;
    this.clearReconnectTimer();
    this.cleanup();
    this.setStatus("closed");
  }

  addEventListener(
    type: "message",
    listener: (data: BlitTransportData) => void,
  ): void;
  addEventListener(
    type: "statuschange",
    listener: (status: ConnectionStatus) => void,
  ): void;
  addEventListener(type: string, listener: (...args: never[]) => void): void {
    if (type === "message") {
      this.messageListeners.add(listener as (data: BlitTransportData) => void);
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener(
    type: "message",
    listener: (data: BlitTransportData) => void,
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
        listener as (data: BlitTransportData) => void,
      );
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
      if (!this.disposed) this.connect();
    }, this.currentDelay);
    this.currentDelay = Math.min(
      this.currentDelay * this.backoff,
      this.maxDelay,
    );
  }

  private cleanup(): void {
    this.writer = null;
    if (this.wt) {
      try {
        this.wt.close();
      } catch {}
      this.wt = null;
    }
  }

  connect(): void {
    if (this.connectPromise) return;
    const connectPromise = this.connectInternal().finally(() => {
      if (this.connectPromise === connectPromise) {
        this.connectPromise = null;
      }
    });
    this.connectPromise = connectPromise;
  }

  private async connectInternal(): Promise<void> {
    if (this.disposed) return;
    if (this.reconnectTimer !== null) {
      this.clearReconnectTimer();
      this.currentDelay = this.initialDelay;
    }
    if (
      this._status === "connecting" ||
      this._status === "authenticating" ||
      this._status === "connected"
    ) {
      return;
    }
    this.setStatus("connecting");

    try {
      const opts: WebTransportOptions = {};
      if (this.certHash) {
        opts.serverCertificateHashes = [
          { algorithm: "sha-256", value: this.certHash.buffer as ArrayBuffer },
        ];
      }

      const wt = new WebTransport(this.url, opts);
      this.wt = wt;
      await Promise.race([
        wt.ready,
        new Promise((_, reject) =>
          setTimeout(
            () => reject(new Error("connect timeout")),
            this.connectTimeoutMs,
          ),
        ),
      ]);

      if (this.disposed || this.wt !== wt) {
        wt.close();
        return;
      }

      // Open a bidirectional stream for the blit protocol
      const stream = await wt.createBidirectionalStream();
      const writer = stream.writable.getWriter();
      const reader = stream.readable.getReader();

      // --- Auth: send 2-byte LE length + passphrase ---
      this.setStatus("authenticating");
      const passBytes = new TextEncoder().encode(this.passphrase);
      const authMsg = new Uint8Array(2 + passBytes.length);
      authMsg[0] = passBytes.length & 0xff;
      authMsg[1] = (passBytes.length >> 8) & 0xff;
      authMsg.set(passBytes, 2);
      await writer.write(authMsg);

      // Read 1-byte auth response: 1 = ok, 0 = rejected
      const { data: authResp, remainder } = await readExactBuffered(reader, 1);
      if (!authResp) {
        // EOF before any verdict — the stream broke mid-handshake. Treating it
        // as a rejection would wedge the transport for a credential the server
        // never examined, so retry instead. The `wt.closed` handler below is
        // not installed yet, so schedule the retry here.
        this.cleanup();
        this.lastError = "Connection closed during authentication";
        this.setStatus("disconnected");
        this.scheduleReconnect();
        return;
      }
      if (authResp[0] !== 1) {
        this.authRejected = true;
        this.lastError = "Authentication failed";
        this.setStatus("error");
        wt.close();
        return;
      }

      if (this.disposed || this.wt !== wt) {
        wt.close();
        return;
      }

      this.writer = writer;
      this.authRejected = false;
      this.lastError = null;
      this.currentDelay = this.initialDelay;
      this.setStatus("connected");

      // Read loop: use a reusable caller-owned receive buffer when this
      // WebTransport implementation exposes its stream as a byte stream.
      reader.releaseLock();
      let dataReader:
        | ReadableStreamDefaultReader<Uint8Array>
        | ReadableStreamBYOBReader;
      let byob = false;
      try {
        dataReader = stream.readable.getReader({ mode: "byob" });
        byob = true;
      } catch {
        dataReader = stream.readable.getReader();
      }
      void this.readLoop(dataReader, wt, remainder, byob);

      // Handle connection close
      wt.closed
        .then((info: WebTransportCloseInfo) => {
          if (this.wt !== wt || this.disposed) return;
          this.cleanup();
          this.lastError = info.reason || null;
          this.setStatus(this.authRejected ? "error" : "disconnected");
          if (!this.authRejected) {
            this.scheduleReconnect();
          }
        })
        .catch((err: unknown) => {
          if (this.wt !== wt || this.disposed) return;
          this.cleanup();
          if (err instanceof Error && err.message) {
            this.lastError = err.message;
          }
          this.setStatus(this.authRejected ? "error" : "disconnected");
          if (!this.authRejected) {
            this.scheduleReconnect();
          }
        });
    } catch (err) {
      if (this.disposed) return;
      this.cleanup();
      if (!this.authRejected) {
        this.lastError =
          err instanceof Error ? err.message : "Connection failed";
      }
      this.setStatus("error");
      if (!this.authRejected) {
        this.scheduleReconnect();
      }
    }
  }

  private async readLoop(
    reader: ReadableStreamDefaultReader<Uint8Array> | ReadableStreamBYOBReader,
    wt: WebTransport,
    initialBuffer: Uint8Array,
    byob: boolean,
  ): Promise<void> {
    const decoder = new LengthPrefixedFrameDecoder(
      MAX_FRAME_LENGTH,
      (payload) => {
        for (const l of this.messageListeners) l(payload);
      },
    );
    let byobBuffer = new ArrayBuffer(WT_BYOB_BUFFER_SIZE);

    try {
      if (initialBuffer.length > 0 && !decoder.push(initialBuffer)) {
        throw new Error("invalid WebTransport frame");
      }
      while (true) {
        const { value, done } = byob
          ? await (reader as ReadableStreamBYOBReader).read(
              new Uint8Array(byobBuffer),
            )
          : await (reader as ReadableStreamDefaultReader<Uint8Array>).read();
        if (this.disposed || this.wt !== wt) return;
        if (done) throw new Error("WebTransport receive stream closed");
        if (!value || value.length === 0) continue;
        if (!decoder.push(value)) {
          throw new Error("invalid WebTransport frame");
        }
        if (byob) {
          byobBuffer =
            value.buffer instanceof ArrayBuffer &&
            value.buffer.byteLength >= WT_BYOB_BUFFER_SIZE
              ? value.buffer
              : new ArrayBuffer(WT_BYOB_BUFFER_SIZE);
        }
      }
    } catch (err) {
      if (this.disposed || this.wt !== wt) return;
      this.cleanup();
      this.lastError =
        err instanceof Error ? err.message : "WebTransport receive failed";
      this.setStatus("disconnected");
      this.scheduleReconnect();
    }
  }
}

/** Read exactly `n` bytes from a ReadableStreamDefaultReader. */
async function readExactBuffered(
  reader: ReadableStreamDefaultReader<Uint8Array>,
  n: number,
  initialBuffer = new Uint8Array(0),
): Promise<{ data: Uint8Array | null; remainder: Uint8Array }> {
  const buf = new Uint8Array(n);
  let offset = 0;
  let buffer = initialBuffer;
  while (offset < n) {
    if (buffer.length === 0) {
      const { value, done } = await reader.read();
      if (done || !value) {
        return { data: null, remainder: new Uint8Array(0) };
      }
      buffer = new Uint8Array(value);
    }
    const take = Math.min(buffer.length, n - offset);
    buf.set(buffer.subarray(0, take), offset);
    offset += take;
    buffer = buffer.subarray(take);
  }
  return { data: buf, remainder: buffer };
}

function hexToBytes(hex: string): Uint8Array {
  const clean = hex.replace(/[^0-9a-fA-F]/g, "");
  const bytes = new Uint8Array(clean.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}
