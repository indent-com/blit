import type { BlitSurface, ConnectionId } from "./types";
import {
  SURFACE_FRAME_FLAG_KEYFRAME,
  SURFACE_FRAME_CODEC_MASK,
  SURFACE_FRAME_CODEC_H264,
  SURFACE_FRAME_CODEC_AV1,
} from "./types";

/**
 * Frame-ready callback.  Listeners receive only the surface ID; they should
 * call {@link SurfaceStore.getCanvas} to obtain the shared backing canvas
 * that already contains the latest rendered frame.
 */
export type SurfaceFrameCallback = (surfaceId: number) => void;

export type SurfaceEventCallback = (
  surfaces: ReadonlyMap<number, BlitSurface>,
) => void;

/** Timestamped record of an incoming surface video frame. */
export interface SurfaceFrameSample {
  /** `performance.now()` when the frame arrived. */
  t: number;
  /** Encoded frame payload size in bytes. */
  bytes: number;
  /** Whether this was a keyframe. */
  key: boolean;
}

type SurfaceCodec = "h264" | "av1";

interface DecoderEntry {
  decoder: VideoDecoder;
  codec: SurfaceCodec;
  pendingKeyframe: boolean;
  /** True once a keyframe request has been sent for the current
   *  `pendingKeyframe` episode.  Reset when a keyframe successfully
   *  decodes.  Prevents every errored delta frame from firing a fresh
   *  keyframe request (which over the wire is a full SURFACE_SUBSCRIBE
   *  — each one resets server-side pacing/burst state). */
  keyframeRequested: boolean;
  /** Last H.264 codec string (e.g. "avc1.42001e"), used to avoid
   *  reconfiguring on every keyframe.  We compare the codec string
   *  (profile/compat/level) rather than raw SPS bytes because some
   *  encoders rotate sps_id on each IDR, which changes the AVCC
   *  description without affecting decode parameters.  Unnecessary
   *  reconfigures orphan in-flight VideoFrame objects (GC warning)
   *  and can stall the decode pipeline. */
  lastCodecString: string | null;
  /** Last AVCC description passed to configure(). */
  lastDescription: ArrayBuffer | null;
  /** Dimensions of the frame that triggered the most recent configure().
   *  A resolution-only resize keeps the same profile/level (and thus the
   *  same codec string), so the cs comparison above can't detect it — but
   *  the SPS embedded in the description carries the new resolution and
   *  the decoder needs to pick it up, otherwise it errors on the first
   *  post-resize keyframe with "Decoding error" and closes. */
  lastConfiguredWidth: number;
  lastConfiguredHeight: number;
}

interface CanvasEntry {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
}

/** Per-surface presenter state.  Queues decoded frames so presentation
 *  happens at vsync boundaries (one `requestAnimationFrame` per surface)
 *  rather than at arbitrary decoder-output moments. */
interface SurfacePresenter {
  /** Decoded VideoFrames waiting to be presented.  On each rAF tick only
   *  the newest is drawn; older frames are closed. */
  queue: VideoFrame[];
  /** Pending `requestAnimationFrame` handle, or null. */
  rafId: number | null;
  /** True after the first frame has been presented.  The first frame
   *  paints synchronously to minimise time-to-first-pixel. */
  initialized: boolean;
}

function codecFromFlags(flags: number): SurfaceCodec {
  const bits = flags & SURFACE_FRAME_CODEC_MASK;
  if (bits === SURFACE_FRAME_CODEC_AV1) return "av1";
  return "h264";
}

/** Gracefully shut down a decoder, ensuring every in-flight VideoFrame
 *  reaches the output callback (which calls frame.close()) before the
 *  decoder is destroyed.
 *
 *  Chromium's reset()/close() drops internally-queued VideoFrame objects
 *  without calling .close(), triggering the "VideoFrame was garbage
 *  collected without being closed" console warning and potentially
 *  stalling the frame buffer pool.  flush() drains the queue through
 *  the normal output path first.
 *
 *  The flush is fire-and-forget — callers continue immediately.  The
 *  output callback still closes every frame via its finally block even
 *  after the decoder entry has been removed from the map. */
function safeClose(decoder: VideoDecoder): void {
  try {
    if (decoder.state === "configured") {
      const close = () => {
        try {
          if (decoder.state !== "closed") decoder.close();
        } catch {
          /* already closed */
        }
      };
      decoder.flush().then(close, close);
    } else if (decoder.state !== "closed") {
      decoder.close();
    }
  } catch {
    // Already closed or in an invalid state.
  }
}

/**
 * Derive the H.264 WebCodecs codec string from the SPS NAL unit so it
 * matches the actual profile/level the encoder produced.
 */
function h264CodecStringFromSps(sps: Uint8Array): string | null {
  if (sps.length < 4) return null;
  const profile = sps[1];
  const compat = sps[2];
  const level = sps[3];
  const hex = (b: number) => b.toString(16).padStart(2, "0");
  return `avc1.${hex(profile)}${hex(compat)}${hex(level)}`;
}

// ---------------------------------------------------------------------------
// Annex B → length-prefixed NAL conversion
//
// The server sends Annex B bitstreams (start-code delimited NAL units).
// WebCodecs defaults to length-prefixed containers (AVCC for H.264).
// The `avc.format` annexb hint is not universally supported (macOS
// VideoToolbox rejects with -12909, Windows Media Foundation doesn't
// support the option at all), so we convert Annex B →
// 4-byte-length-prefixed on every frame.
// ---------------------------------------------------------------------------

/** Split Annex B byte stream into individual NAL units (without start codes). */
function splitNALs(data: Uint8Array): Uint8Array[] {
  const nals: Uint8Array[] = [];
  const len = data.length;
  let i = 0;

  // Advance past the first start code.
  while (i < len - 3) {
    if (data[i] === 0 && data[i + 1] === 0) {
      if (data[i + 2] === 1) {
        i += 3;
        break;
      }
      if (data[i + 2] === 0 && i + 3 < len && data[i + 3] === 1) {
        i += 4;
        break;
      }
    }
    i++;
  }

  let nalStart = i;
  while (i < len) {
    if (
      i + 2 < len &&
      data[i] === 0 &&
      data[i + 1] === 0 &&
      (data[i + 2] === 1 ||
        (data[i + 2] === 0 && i + 3 < len && data[i + 3] === 1))
    ) {
      if (i > nalStart) nals.push(data.subarray(nalStart, i));
      i += data[i + 2] === 1 ? 3 : 4;
      nalStart = i;
    } else {
      i++;
    }
  }
  if (nalStart < len) nals.push(data.subarray(nalStart, len));
  return nals;
}

/** Replace Annex B start codes with 4-byte big-endian length prefixes. */
function toLengthPrefixed(nals: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const n of nals) total += 4 + n.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const n of nals) {
    const l = n.length;
    out[off] = (l >>> 24) & 0xff;
    out[off + 1] = (l >>> 16) & 0xff;
    out[off + 2] = (l >>> 8) & 0xff;
    out[off + 3] = l & 0xff;
    out.set(n, off + 4);
    off += 4 + l;
  }
  return out;
}

/** H.264 NAL unit type (5 low bits of the first byte). */
function h264NalType(nal: Uint8Array): number {
  return nal[0] & 0x1f;
}

/**
 * Build an AVCDecoderConfigurationRecord (ISO 14496-15 §5.3.3.1)
 * from raw SPS and PPS NAL units (without start codes).
 */
function buildAvccDescription(sps: Uint8Array, pps: Uint8Array): ArrayBuffer {
  // Parse profile/level from SPS NAL (bytes 1-3 after the NAL type byte).
  const profileIdc = sps[1];
  const profileCompat = sps[2];
  const levelIdc = sps[3];

  const size = 6 + 1 + 2 + sps.length + 1 + 2 + pps.length;
  const buf = new ArrayBuffer(size);
  const v = new DataView(buf);
  const u = new Uint8Array(buf);
  let o = 0;

  v.setUint8(o++, 1); // configurationVersion
  v.setUint8(o++, profileIdc); // AVCProfileIndication
  v.setUint8(o++, profileCompat); // profile_compatibility
  v.setUint8(o++, levelIdc); // AVCLevelIndication
  v.setUint8(o++, 0xff); // 6 reserved bits (111111) + lengthSizeMinusOne=3
  v.setUint8(o++, 0xe1); // 3 reserved bits (111) + numOfSequenceParameterSets=1
  v.setUint16(o, sps.length); // sequenceParameterSetLength
  o += 2;
  u.set(sps, o); // sequenceParameterSetNALUnit
  o += sps.length;
  v.setUint8(o++, 1); // numOfPictureParameterSets
  v.setUint16(o, pps.length); // pictureParameterSetLength
  o += 2;
  u.set(pps, o); // pictureParameterSetNALUnit

  return buf;
}

export class SurfaceStore {
  private surfaces = new Map<number, BlitSurface>();
  private connectionId: ConnectionId = "";
  private decoders = new Map<number, DecoderEntry>();
  private canvases = new Map<number, CanvasEntry>();
  private frameListeners = new Set<SurfaceFrameCallback>();
  private cursorShapes = new Map<number, string>();
  private encoderNames = new Map<number, string>();
  private codecStrings = new Map<number, string>();
  private cursorListeners = new Set<
    (surfaceId: number, shape: string) => void
  >();
  private eventListeners = new Set<SurfaceEventCallback>();
  private _diag = { received: 0, decoded: 0, output: 0, dropped: 0, errors: 0 };
  private _diagTimer: ReturnType<typeof setInterval> | null = null;

  // Per-surface diagnostics exposed to the debug panel.
  private _surfaceFrameSamples = new Map<number, SurfaceFrameSample[]>();
  /** Timestamps of decoded output frames (for computing output fps). */
  private _surfaceOutputSamples = new Map<number, number[]>();
  /** Cumulative per-surface drop/error counters. */
  private _surfaceDrops = new Map<number, number>();
  private _surfaceErrors = new Map<number, number>();

  private static readonly FRAME_SAMPLE_MAX = 500;
  private static readonly OUTPUT_SAMPLE_MAX = 500;

  /** Per-surface presenter: queues decoded frames and paints the freshest
   *  one at the next vsync via rAF.  Older frames in the queue are closed
   *  without drawing — equivalent to the original "drawImage on each decode
   *  + coalesce via rAF" flow, consolidated into one layer. */
  private presenters = new Map<number, SurfacePresenter>();

  /**
   * Callback to send a surface ACK to the server.  Injected by the
   * connection layer so the store can defer ACKs when the decode queue
   * is deep (backpressure).
   */
  private _ackSender: ((surfaceId: number) => void) | null = null;

  /**
   * Callback to request a keyframe from the server (re-subscribe).
   * Called when the decoder enters an error state and needs a clean
   * reference point to recover.
   */
  private _keyframeSender: ((surfaceId: number) => void) | null = null;

  /** Install the ACK sender callback (called once by BlitConnection). */
  setAckSender(fn: (surfaceId: number) => void): void {
    this._ackSender = fn;
  }

  /** Install the keyframe-request callback (called once by BlitConnection). */
  setKeyframeSender(fn: (surfaceId: number) => void): void {
    this._keyframeSender = fn;
  }

  private sendAck(surfaceId: number): void {
    this._ackSender?.(surfaceId);
  }

  /** Send an ACK unconditionally — used by the connection layer's catch
   *  path when handleSurfaceFrame throws before it can ACK itself. */
  sendAckFallback(surfaceId: number): void {
    this._ackSender?.(surfaceId);
  }

  /**
   * Monotonically increasing counter bumped on every disconnect.  Consumers
   * (e.g. {@link BlitSurfaceCanvas}) compare their last-seen generation to
   * detect reconnects and re-subscribe for video frames.
   */
  private _generation = 0;
  get generation(): number {
    return this._generation;
  }

  /**
   * Whether the browser can decode surface video frames (WebCodecs + secure
   * context).  Checked eagerly at construction time so callers can skip
   * surface subscriptions that would only drive the server encoder for
   * nothing (and risk crashing it).
   */
  readonly canDecodeVideo: boolean;

  /**
   * Non-null when surface video decoding is unavailable (e.g. insecure
   * context or missing WebCodecs).  UI components should display this
   * message instead of a blank canvas.
   */
  videoUnavailableReason: string | null = null;

  constructor() {
    const hasWebCodecs =
      typeof VideoDecoder !== "undefined" &&
      typeof EncodedVideoChunk !== "undefined";
    const isSecure = typeof window === "undefined" || window.isSecureContext;
    this.canDecodeVideo = hasWebCodecs && isSecure;
    if (!this.canDecodeVideo) {
      const insecure = typeof window !== "undefined" && !window.isSecureContext;
      this.videoUnavailableReason = insecure
        ? "Secure context required (HTTPS or localhost)"
        : "WebCodecs API not available in this browser";
    }
    this._diagTimer = setInterval(() => {
      const d = this._diag;
      if (d.received > 0) {
        console.log(
          `[blit-video] recv=${d.received} decoded=${d.decoded} output=${d.output} dropped=${d.dropped} errors=${d.errors} listeners=${this.frameListeners.size}`,
        );
        d.received = d.decoded = d.output = d.dropped = d.errors = 0;
      }
    }, 5000);
  }

  onFrame(listener: SurfaceFrameCallback): () => void {
    this.frameListeners.add(listener);
    return () => this.frameListeners.delete(listener);
  }

  onChange(listener: SurfaceEventCallback): () => void {
    this.eventListeners.add(listener);
    return () => this.eventListeners.delete(listener);
  }

  getSurfaces(): ReadonlyMap<number, BlitSurface> {
    return this.surfaces;
  }

  /** Debug info about all known surfaces (encoder, codec, size, decode stats). */
  getDebugStats(): {
    surfaceId: number;
    codec: string;
    encoder: string;
    width: number;
    height: number;
    /** Ring buffer of recent incoming frame samples (for timeline graph). */
    frameSamples: SurfaceFrameSample[];
    /** Ring buffer of decoded-output timestamps (for fps computation). */
    outputSamples: readonly number[];
    /** Cumulative dropped frame count. */
    dropped: number;
    /** Cumulative decode error count. */
    errors: number;
    /** Current WebCodecs decode queue depth. */
    queueDepth: number;
  }[] {
    const result: ReturnType<SurfaceStore["getDebugStats"]> = [];
    for (const [id, surface] of this.surfaces) {
      // Skip subsurfaces — they are composited into their parent and
      // don't have their own encoder or codec.
      if (surface.parentId !== 0) continue;
      const entry = this.decoders.get(id);
      let queueDepth = 0;
      try {
        queueDepth =
          entry && entry.decoder.state === "configured"
            ? entry.decoder.decodeQueueSize
            : 0;
      } catch {
        // decoder may be closed
      }
      result.push({
        surfaceId: id,
        codec: entry?.codec ?? "",
        encoder: this.encoderNames.get(id) ?? "",
        width: surface.width,
        height: surface.height,
        frameSamples: this._surfaceFrameSamples.get(id) ?? [],
        outputSamples: this._surfaceOutputSamples.get(id) ?? [],
        dropped: this._surfaceDrops.get(id) ?? 0,
        errors: this._surfaceErrors.get(id) ?? 0,
        queueDepth,
      });
    }
    return result;
  }

  getSurface(surfaceId: number): BlitSurface | undefined {
    return this.surfaces.get(surfaceId);
  }

  /** Return the shared backing canvas for a surface — the server sends
   *  one stream per `(cid, sid)`, so a single decoder and canvas per
   *  surface suffice.  The canvas is never attached to the DOM;
   *  callers blit from it into their visible canvases. */
  getCanvas(surfaceId: number): HTMLCanvasElement | null {
    return this.canvases.get(surfaceId)?.canvas ?? null;
  }

  setConnectionId(id: ConnectionId): void {
    this.connectionId = id;
  }

  handleSurfaceCreated(
    surfaceId: number,
    parentId: number,
    width: number,
    height: number,
    title: string,
    appId: string,
  ): void {
    this.surfaces.set(surfaceId, {
      connectionId: this.connectionId,
      surfaceId,
      parentId,
      title,
      appId,
      width,
      height,
    });
    // Don't create a canvas yet — canvases are per-subscription now,
    // keyed by sub_id, and we don't have one until a view subscribes.
    this.emitChange();
  }

  handleSurfaceDestroyed(surfaceId: number): void {
    this.surfaces.delete(surfaceId);
    this.encoderNames.delete(surfaceId);
    this.codecStrings.delete(surfaceId);
    this._surfaceFrameSamples.delete(surfaceId);
    this._surfaceOutputSamples.delete(surfaceId);
    this._surfaceDrops.delete(surfaceId);
    this._surfaceErrors.delete(surfaceId);
    this.discardPresenter(surfaceId);
    const entry = this.decoders.get(surfaceId);
    if (entry) safeClose(entry.decoder);
    this.decoders.delete(surfaceId);
    this.canvases.delete(surfaceId);
    this.emitChange();
  }

  handleSurfaceFrame(
    surfaceId: number,
    _timestamp: number,
    flags: number,
    width: number,
    height: number,
    data: Uint8Array,
  ): void {
    this._diag.received++;
    const isKey = (flags & SURFACE_FRAME_FLAG_KEYFRAME) !== 0;

    // Per-surface frame timeline sample.
    let samples = this._surfaceFrameSamples.get(surfaceId);
    if (!samples) {
      samples = [];
      this._surfaceFrameSamples.set(surfaceId, samples);
    }
    samples.push({ t: performance.now(), bytes: data.length, key: isKey });
    if (samples.length > SurfaceStore.FRAME_SAMPLE_MAX)
      samples.splice(0, samples.length - SurfaceStore.FRAME_SAMPLE_MAX);

    const codec = codecFromFlags(flags);

    let entry = this.decoders.get(surfaceId);
    if (!entry || entry.codec !== codec) {
      if (entry) {
        safeClose(entry.decoder);
      }
      this.decoders.delete(surfaceId);
      this.initDecoder(surfaceId, codec, width, height);
      entry = this.decoders.get(surfaceId);
    }
    if (!entry) {
      // No decoder — ACK immediately so the server doesn't stall.
      this.sendAck(surfaceId);
      return;
    }

    if (entry.pendingKeyframe && !isKey) {
      this._diag.dropped++;
      this._surfaceDrops.set(
        surfaceId,
        (this._surfaceDrops.get(surfaceId) ?? 0) + 1,
      );
      // Dropped frame — ACK immediately.
      this.sendAck(surfaceId);
      return;
    }
    entry.pendingKeyframe = false;
    // A keyframe landed (or at least was accepted for decode) — future
    // decode errors will legitimately need a fresh keyframe request, so
    // drop the "already asked" latch.
    entry.keyframeRequested = false;

    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      const wasEmpty = surface.width === 0 || surface.height === 0;
      // Mutate in place so downstream <For> children keep their object
      // identity (no remount → no decoder race).  Subscribers read the
      // fresh fields on the next emitChange-driven recomputation.
      surface.width = width;
      surface.height = height;
      // Emit a change when the surface gets its first real dimensions
      // (the compositor sends SurfaceCreated with 0×0 before the first
      // buffer commit).  Subsequent per-frame dimension tweaks are silent.
      if (wasEmpty && width > 0 && height > 0) {
        this.emitChange();
      }
    }

    this.ensureCanvas(surfaceId, width, height);

    try {
      let frameData: Uint8Array;

      if (codec === "av1") {
        // AV1: raw OBU "low-overhead bitstream format" per WebCodecs spec.
        // No description, no NAL splitting, no length-prefix — pass through.
        frameData = data;
      } else {
        // H.264: Annex B → AVCC length-prefixed + description
        const nals = splitNALs(data);
        if (isKey) {
          let sps: Uint8Array | undefined;
          let pps: Uint8Array | undefined;
          const vclNals: Uint8Array[] = [];
          for (const nal of nals) {
            const t = h264NalType(nal);
            if (t === 7) sps = nal;
            else if (t === 8) pps = nal;
            else vclNals.push(nal);
          }
          if (sps && pps) {
            const description = buildAvccDescription(sps, pps);
            const cs = h264CodecStringFromSps(sps) ?? "avc1.42001e";
            const dimsChanged =
              width !== entry.lastConfiguredWidth ||
              height !== entry.lastConfiguredHeight;
            if (cs !== entry.lastCodecString || dimsChanged) {
              entry.lastCodecString = cs;
              entry.lastDescription = description;
              entry.lastConfiguredWidth = width;
              entry.lastConfiguredHeight = height;
              // If the decoder already has queued work, calling
              // configure() directly resets its state and orphans any
              // in-flight VideoFrame objects — Chromium then logs
              // "A VideoFrame was garbage collected without being
              // closed" and eventually exhausts its frame pool,
              // stalling decode.  Queue a flush() first so pending
              // frames drain through the output callback (which
              // closes them) before the reset.  WebCodecs processes
              // control messages in order, so the subsequent
              // configure() and decode() of the current keyframe
              // simply run after the flush completes.
              if (entry.decoder.state === "configured") {
                entry.decoder.flush().catch(() => {
                  /* flush rejected — decoder likely closed */
                });
              }
              entry.decoder.configure({
                codec: cs,
                optimizeForLatency: true,
                description,
              });
            }
          }
          // In AVCC mode, parameter-set NALs (SPS/PPS) belong in the
          // description — strip them from the frame data.
          frameData = toLengthPrefixed(vclNals.length > 0 ? vclNals : nals);
        } else {
          frameData = toLengthPrefixed(nals);
        }
      }

      // Guard: don't decode if the decoder was never configured
      // (e.g., old server without VPS/SPS/PPS or HVCC prefix).
      if (entry.decoder.state !== "configured") {
        this._diag.dropped++;
        this.sendAck(surfaceId);
        return;
      }

      const chunk = new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: _timestamp * 1000,
        data: frameData,
      });
      entry.decoder.decode(chunk);
      this._diag.decoded++;

      // ACK immediately — the server already paces delivery via its own
      // inflight window and time-based send interval.  Deferring ACKs
      // until the output callback adds decode latency to the effective
      // round-trip, starving the server's pacing window on high-latency
      // or software-decode paths.
      this.sendAck(surfaceId);
    } catch (e) {
      console.warn(
        "[blit] surface decode error:",
        surfaceId,
        codec,
        `${width}x${height}`,
        isKey ? "key" : "delta",
        `${data.length}B`,
        e,
      );
      if (entry) entry.pendingKeyframe = true;
      this._diag.errors++;
      this._surfaceErrors.set(
        surfaceId,
        (this._surfaceErrors.get(surfaceId) ?? 0) + 1,
      );
      // Error — ACK immediately so the server doesn't permanently stall.
      this.sendAck(surfaceId);
      // Ask the server for a keyframe so the decoder can recover.
      // Fire at most once per pendingKeyframe episode — each request is
      // a SURFACE_SUBSCRIBE on the wire and resets server-side pacing.
      // The flag is cleared when a keyframe decodes successfully.
      if (entry && !entry.keyframeRequested) {
        entry.keyframeRequested = true;
        this._keyframeSender?.(surfaceId);
      }
    }
  }

  handleSurfaceTitle(surfaceId: number, title: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, title });
      this.emitChange();
    }
  }

  handleSurfaceCursor(surfaceId: number, shape: string): void {
    this.cursorShapes.set(surfaceId, shape);
    // Notify cursor listeners without triggering a full change cycle.
    for (const listener of this.cursorListeners) {
      try {
        listener(surfaceId, shape);
      } catch {}
    }
  }

  /** Get the current CSS cursor for a surface. */
  getCursor(surfaceId: number): string {
    return this.cursorShapes.get(surfaceId) ?? "default";
  }

  /** Register a callback for cursor shape changes. Returns unsubscribe fn. */
  onCursor(listener: (surfaceId: number, shape: string) => void): () => void {
    this.cursorListeners.add(listener);
    return () => {
      this.cursorListeners.delete(listener);
    };
  }

  handleSurfaceEncoder(surfaceId: number, rawPayload: string): void {
    // Format: "encoder-name\0codec-string" (NUL-separated).
    const nul = rawPayload.indexOf("\0");
    const encoderName = nul >= 0 ? rawPayload.slice(0, nul) : rawPayload;
    const codecString = nul >= 0 ? rawPayload.slice(nul + 1) : null;
    this.encoderNames.set(surfaceId, encoderName);
    if (codecString) {
      this.codecStrings.set(surfaceId, codecString);
    }
  }

  handleSurfaceAppId(surfaceId: number, appId: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, appId });
      this.emitChange();
    }
  }

  handleSurfaceResized(surfaceId: number, width: number, height: number): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      // Only emit a change for significant resizes (> 1px) to avoid
      // triggering a BSP re-render → ResizeObserver → resize feedback loop
      // from sub-pixel rounding in the compositor's physical↔logical
      // conversion.  The initial 0x0 → real size always emits.
      const significant =
        surface.width === 0 ||
        surface.height === 0 ||
        Math.abs(surface.width - width) > 1 ||
        Math.abs(surface.height - height) > 1;
      surface.width = width;
      surface.height = height;
      // Flush any queued frames from the old resolution.  Without this,
      // stale VideoFrames occupy the decode buffer pool and the presenter
      // draws a wrong-sized frame, stalling the pipeline.  Discarding
      // resets `initialized` so the first frame at the new resolution
      // paints synchronously (fast path).
      this.discardPresenter(surfaceId);
      // Proactively ask the server for a keyframe at the new dimensions
      // and drop any delta frames that arrive before it.  The decoder
      // must be reconfigured with the new SPS/PPS (H.264) or size hint
      // anyway, so a keyframe is mandatory; waiting passively for the
      // server to produce one adds an extra round-trip to the recovery.
      const entry = this.decoders.get(surfaceId);
      if (entry) {
        entry.pendingKeyframe = true;
        if (!entry.keyframeRequested) {
          entry.keyframeRequested = true;
          this._keyframeSender?.(surfaceId);
        }
      }
      if (significant) this.emitChange();
    }
  }

  /**
   * Full teardown on transport disconnect.  Clears all surfaces, canvases,
   * and decoders so the UI reflects the disconnected state immediately.
   * The server's initial message sequence after reconnect
   * ({@link reset} via S2C_HELLO, then S2C_SURFACE_CREATED) will rebuild
   * the surface list.  The generation counter is bumped so
   * {@link BlitSurfaceCanvas} instances detect the reconnect and
   * re-subscribe for video frames.
   */
  handleDisconnect(): void {
    this.discardAllPresenters();
    for (const entry of this.decoders.values()) {
      safeClose(entry.decoder);
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    this.encoderNames.clear();
    this.codecStrings.clear();
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._surfaceDrops.clear();
    this._surfaceErrors.clear();
    this._generation++;
    this.emitChange();
  }

  /**
   * Full surface reset — called when S2C_HELLO signals a (possibly new)
   * server instance.  Clears all surfaces, canvases, and decoders.  The
   * server's initial message sequence will rebuild the surface list via
   * individual S2C_SURFACE_CREATED messages.
   */
  reset(): void {
    this.discardAllPresenters();
    for (const entry of this.decoders.values()) {
      safeClose(entry.decoder);
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    this.encoderNames.clear();
    this.codecStrings.clear();
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._surfaceDrops.clear();
    this._surfaceErrors.clear();
    this._generation++;
    this.emitChange();
  }

  /**
   * Full teardown — only called when the connection is permanently disposed.
   */
  destroy(): void {
    if (this._diagTimer !== null) {
      clearInterval(this._diagTimer);
      this._diagTimer = null;
    }
    this.reset();
  }

  // -----------------------------------------------------------------------
  // Private
  // -----------------------------------------------------------------------

  /** Push a decoded frame into the surface's presenter, paint the very
   *  first one synchronously, and schedule the next vsync tick. */
  private enqueueFrame(surfaceId: number, frame: VideoFrame): void {
    let p = this.presenters.get(surfaceId);
    if (!p) {
      p = { queue: [], rafId: null, initialized: false };
      this.presenters.set(surfaceId, p);
    }

    if (!p.initialized) {
      p.initialized = true;
      this.presentFrame(surfaceId, frame);
      return;
    }

    p.queue.push(frame);
    this.schedulePresent(surfaceId);
  }

  private schedulePresent(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.rafId !== null) return;
    p.rafId = requestAnimationFrame(() => {
      p.rafId = null;
      this.tickPresent(surfaceId);
    });
  }

  /** vsync tick: present the newest queued frame, drop any older ones. */
  private tickPresent(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.queue.length === 0) return;
    const last = p.queue.length - 1;
    for (let i = 0; i < last; i++) {
      try {
        p.queue[i].close();
      } catch {
        /* already closed */
      }
    }
    const chosen = p.queue[last];
    p.queue.length = 0;
    this.presentFrame(surfaceId, chosen);
  }

  /** Draw a frame to the backing canvas and notify listeners.  Closes the
   *  frame on the way out. */
  private presentFrame(surfaceId: number, frame: VideoFrame): void {
    try {
      const ce = this.canvases.get(surfaceId);
      if (ce) {
        if (
          ce.canvas.width !== frame.displayWidth ||
          ce.canvas.height !== frame.displayHeight
        ) {
          ce.canvas.width = frame.displayWidth;
          ce.canvas.height = frame.displayHeight;
        }
        ce.ctx.drawImage(frame, 0, 0);
      }
    } finally {
      try {
        frame.close();
      } catch {
        /* already closed */
      }
    }
    for (const listener of this.frameListeners) {
      try {
        listener(surfaceId);
      } catch {
        // Prevent a single broken listener from blocking others.
      }
    }
  }

  private discardPresenter(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p) return;
    if (p.rafId !== null) cancelAnimationFrame(p.rafId);
    for (const f of p.queue) {
      try {
        f.close();
      } catch {
        /* already closed */
      }
    }
    this.presenters.delete(surfaceId);
  }

  private discardAllPresenters(): void {
    for (const sid of Array.from(this.presenters.keys())) {
      this.discardPresenter(sid);
    }
  }

  /**
   * Create an off-DOM canvas for *surfaceId* if one does not already exist.
   * Existing canvases are never resized here — resizing clears content and
   * must only happen inside the decoder output callback where a new frame is
   * immediately drawn afterwards.
   */
  private ensureCanvas(surfaceId: number, width: number, height: number): void {
    if (typeof document === "undefined") return;
    const w = width || 640;
    const h = height || 480;
    if (this.canvases.has(surfaceId)) return;
    try {
      const canvas = document.createElement("canvas");
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      this.canvases.set(surfaceId, { canvas, ctx });
    } catch {
      // Fallback for environments where canvas creation fails.
    }
  }

  private webCodecsUnavailableWarned = false;

  private initDecoder(
    surfaceId: number,
    codec: SurfaceCodec,
    width: number,
    height: number,
  ): void {
    if (!this.canDecodeVideo) {
      if (!this.webCodecsUnavailableWarned) {
        this.webCodecsUnavailableWarned = true;
        console.error(
          `[blit] Cannot decode surface video: ${this.videoUnavailableReason}.\n` +
            (typeof window !== "undefined" && !window.isSecureContext
              ? `Connect via HTTPS or localhost to enable surface streaming.`
              : `See https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API#browser_compatibility`),
        );
        this.emitChange();
      }
      return;
    }
    const decoder = new VideoDecoder({
      output: (frame) => {
        this._diag.output++;

        // Per-surface output sample for debug panel rate computation.
        let outputs = this._surfaceOutputSamples.get(surfaceId);
        if (!outputs) {
          outputs = [];
          this._surfaceOutputSamples.set(surfaceId, outputs);
        }
        outputs.push(performance.now());
        if (outputs.length > SurfaceStore.OUTPUT_SAMPLE_MAX)
          outputs.splice(0, outputs.length - SurfaceStore.OUTPUT_SAMPLE_MAX);

        // Queue + paced presentation absorbs network/decoder jitter and
        // prevents 30 fps content from juddering on a 120 Hz display.
        // The first frame paints synchronously inside enqueueFrame to
        // minimise time-to-first-pixel.
        this.enqueueFrame(surfaceId, frame);
      },
      error: (e: DOMException) => {
        console.warn(
          "[blit] surface decoder error:",
          surfaceId,
          `${width}x${height}`,
          e.name,
          e.message,
          e.code,
          "state:",
          decoder.state,
        );
        // Only clean up if this decoder is still the active one —
        // handleSurfaceFrame may have already replaced it with a fresh
        // instance by the time this async callback fires.
        const entry = this.decoders.get(surfaceId);
        if (entry?.decoder === decoder) {
          safeClose(entry.decoder);
          this.decoders.delete(surfaceId);
        }
        // Ask the server for a keyframe so the next decoder gets a
        // clean reference point.
        this._keyframeSender?.(surfaceId);
      },
    });
    // Defer configure() until the first keyframe provides the codec
    // description (AVCC for H.264).  Configuring without a description
    // then reconfiguring with one causes VideoToolbox on macOS to drop
    // the first decoded frame.
    // AV1 has no description — configure it eagerly using the server-
    // provided WebCodecs codec string.
    if (codec === "av1") {
      const cs = this.codecStrings.get(surfaceId);
      if (cs) {
        try {
          decoder.configure({
            codec: cs,
            optimizeForLatency: true,
          });
        } catch (e) {
          console.warn(
            "[blit] surface decoder configure failed:",
            surfaceId,
            codec,
            cs,
            e,
          );
          decoder.close();
          return;
        }
      }
    }
    this.decoders.set(surfaceId, {
      decoder,
      codec,
      pendingKeyframe: true,
      keyframeRequested: false,
      lastCodecString: null,
      lastDescription: null,
      lastConfiguredWidth: 0,
      lastConfiguredHeight: 0,
    });
  }

  private emitChange(): void {
    for (const listener of this.eventListeners) {
      try {
        listener(this.surfaces);
      } catch {
        // Prevent a single broken listener from blocking others.
      }
    }
  }
}
