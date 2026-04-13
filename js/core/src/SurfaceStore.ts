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

/**
 * Maximum decode queue depth before we defer the surface ACK.  When the
 * WebCodecs decoder's internal queue is at or above this threshold the ACK
 * is held back, which stalls the server's pacing window and prevents it
 * from flooding the client faster than it can decode.
 */
const MAX_DECODE_QUEUE_FOR_ACK = 2;

interface DecoderEntry {
  decoder: VideoDecoder;
  codec: SurfaceCodec;
  pendingKeyframe: boolean;
  /** Last HVCC/AVCC description bytes, used to avoid reconfiguring on every
   *  keyframe when the parameter sets haven't changed. */
  lastDescription: string | null;
  /** Surface IDs awaiting ACK, held back because the decode queue was deep. */
  pendingAcks: number;
}

interface CanvasEntry {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
}

function codecFromFlags(flags: number): SurfaceCodec {
  const bits = flags & SURFACE_FRAME_CODEC_MASK;
  if (bits === SURFACE_FRAME_CODEC_AV1) return "av1";
  return "h264";
}

/**
 * Compute AV1 level index from coded dimensions, mirroring the server's
 * `compute_level()` in vaapi_encode.rs.  Returns the two-digit level
 * string used in the av01 codec string (e.g. "05" for level 3.1).
 */
function av1LevelString(width: number, height: number): string {
  // Assume 60 fps — matches the server's compute_level(w, h, 60).
  const sps = width * height * 60;
  const specs: [string, number, number, number][] = [
    ["00", 2048, 1152, 5529600],
    ["01", 2816, 1152, 10454400],
    ["04", 4352, 2448, 24969600],
    ["05", 5504, 3096, 39938400],
    ["08", 6144, 3456, 77856768],
    ["09", 6144, 3456, 155713536],
    ["12", 8192, 4352, 273715200],
    ["13", 8192, 4352, 547430400],
    ["16", 16384, 8704, 1176502272],
  ];
  for (const [level, maxW, maxH, maxRate] of specs) {
    if (width <= maxW && height <= maxH && sps <= maxRate) return level;
  }
  return "16";
}

/** WebCodecs codec string for the given surface codec. */
function codecString(
  codec: SurfaceCodec,
  width?: number,
  height?: number,
  sps?: Uint8Array,
): string {
  if (codec === "av1") {
    const level = width && height ? av1LevelString(width, height) : "13";
    return `av01.0.${level}M.08`;
  }
  // Derive the avc1 codec string from the SPS so it matches the actual
  // profile/level NVENC (or any encoder) produces.  The old hardcoded
  // "avc1.420034" claimed Baseline profile, but NVENC's P1 preset emits
  // High profile (CABAC).  A decoder selected for Baseline can't handle
  // CABAC, producing black macroblocks.
  if (sps && sps.length >= 4) {
    const profile = sps[1];
    const compat = sps[2];
    const level = sps[3];
    const hex = (b: number) => b.toString(16).padStart(2, "0");
    return `avc1.${hex(profile)}${hex(compat)}${hex(level)}`;
  }
  // Fallback: High profile level 5.2 — safe for any modern decoder.
  return "avc1.640034";
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
    /** ACKs deferred due to decode backpressure. */
    pendingAcks: number;
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
        pendingAcks: entry?.pendingAcks ?? 0,
      });
    }
    return result;
  }

  getSurface(surfaceId: number): BlitSurface | undefined {
    return this.surfaces.get(surfaceId);
  }

  /**
   * Return the shared backing canvas for *surfaceId*.  The canvas always
   * contains the most-recently decoded frame and can be used as a source for
   * `drawImage` on any number of visible canvases.  The canvas is never
   * attached to the DOM.
   */
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
    this.ensureCanvas(surfaceId, width, height);
    // Don't init decoder yet — we'll init on the first frame when we know
    // the codec from the flags byte.
    this.emitChange();
  }

  handleSurfaceDestroyed(surfaceId: number): void {
    this.surfaces.delete(surfaceId);
    this.canvases.delete(surfaceId);
    this.encoderNames.delete(surfaceId);
    this._surfaceFrameSamples.delete(surfaceId);
    this._surfaceOutputSamples.delete(surfaceId);
    this._surfaceDrops.delete(surfaceId);
    this._surfaceErrors.delete(surfaceId);
    const entry = this.decoders.get(surfaceId);
    if (entry) {
      entry.decoder.close();
      this.decoders.delete(surfaceId);
    }
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

    // Ensure we have a decoder for this surface with the right codec.
    let entry = this.decoders.get(surfaceId);
    if (!entry || entry.codec !== codec) {
      // Flush deferred ACKs before discarding the old decoder —
      // losing them permanently stalls the server's pacing window.
      if (entry) {
        for (let i = 0; i < entry.pendingAcks; i++) {
          this.sendAck(surfaceId);
        }
        entry.decoder.close();
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

    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      const wasEmpty = surface.width === 0 || surface.height === 0;
      this.surfaces.set(surfaceId, { ...surface, width, height });
      // Emit a change when the surface gets its first real dimensions
      // (the compositor sends SurfaceCreated with 0×0 before the first
      // buffer commit).  Subsequent per-frame dimension tweaks are silent
      // to avoid Solid re-render → unmount/remount → sub/unsub churn.
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
          for (const nal of nals) {
            const t = h264NalType(nal);
            if (t === 7) sps = nal;
            else if (t === 8) pps = nal;
          }
          if (sps && pps) {
            const description = buildAvccDescription(sps, pps);
            const descKey = Array.from(new Uint8Array(description)).join(",");
            if (descKey !== entry.lastDescription) {
              entry.lastDescription = descKey;
              entry.decoder.configure({
                codec: codecString(codec, width, height, sps),
                optimizeForLatency: true,
                description,
              });
            }
          }
        }
        frameData = toLengthPrefixed(nals);
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

      // Backpressure: only ACK immediately if the decode queue is shallow.
      // When the queue is deep the ACK is deferred until the decoder's
      // output callback drains it below the threshold — this stalls the
      // server's pacing window and prevents flooding.
      if (entry.decoder.decodeQueueSize < MAX_DECODE_QUEUE_FOR_ACK) {
        this.sendAck(surfaceId);
      } else {
        entry.pendingAcks++;
      }
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
      // Without this, the client drops every P-frame (pendingKeyframe
      // gate) and the server never knows to send a keyframe.
      this._keyframeSender?.(surfaceId);
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

  handleSurfaceEncoder(surfaceId: number, encoderName: string): void {
    this.encoderNames.set(surfaceId, encoderName);
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
      this.surfaces.set(surfaceId, { ...surface, width, height });
      this.ensureCanvas(surfaceId, width, height);
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
    for (const entry of this.decoders.values()) {
      entry.decoder.close();
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
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
    for (const entry of this.decoders.values()) {
      entry.decoder.close();
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
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

  /**
   * Create an off-DOM canvas for *surfaceId* if one does not already exist.
   * Existing canvases are never resized here — resizing clears content and
   * must only happen inside the decoder output callback where a new frame is
   * immediately drawn afterwards.
   */
  private ensureCanvas(surfaceId: number, width: number, height: number): void {
    if (typeof document === "undefined") return;
    if (this.canvases.has(surfaceId)) return;
    const w = width || 640;
    const h = height || 480;
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
    width?: number,
    height?: number,
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

        try {
          // Draw to the shared backing canvas.
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
          frame.close();
        }

        // Flush deferred ACKs now that the decode queue has drained.
        const entry = this.decoders.get(surfaceId);
        if (entry && entry.pendingAcks > 0) {
          // Send one ACK per deferred frame.  The server's pacing window
          // opens one slot per ACK, so this naturally meters delivery.
          const toSend = entry.pendingAcks;
          entry.pendingAcks = 0;
          for (let i = 0; i < toSend; i++) {
            this.sendAck(surfaceId);
          }
        }

        // Notify frame listeners so they blit from getCanvas().
        for (const listener of this.frameListeners) {
          try {
            listener(surfaceId);
          } catch {
            // Prevent a single broken listener from blocking others.
          }
        }
      },
      error: (e: DOMException) => {
        console.warn(
          "[blit] surface decoder error:",
          surfaceId,
          e.name,
          e.message,
          e.code,
          "state:",
          this.decoders.get(surfaceId)?.decoder?.state,
        );
        const entry = this.decoders.get(surfaceId);
        if (entry) {
          // Flush any deferred ACKs before destroying the entry —
          // losing them permanently shrinks the server's pacing window,
          // eventually stalling frame delivery for this surface.
          for (let i = 0; i < entry.pendingAcks; i++) {
            this.sendAck(surfaceId);
          }
          try {
            entry.decoder.close();
          } catch {
            // Already closed.
          }
          // Remove the broken decoder so the next frame triggers
          // re-initialization via initDecoder().
          this.decoders.delete(surfaceId);
        }
        // Ask the server for a keyframe so the new decoder gets a
        // clean reference point instead of only P-frames.
        this._keyframeSender?.(surfaceId);
      },
    });
    // Defer configure() until the first keyframe provides the codec
    // description (AVCC for H.264).  Configuring without a description
    // then reconfiguring with one causes VideoToolbox on macOS to drop
    // the first decoded frame.
    // AV1 has no description — configure it eagerly.
    if (codec === "av1") {
      try {
        decoder.configure({
          codec: codecString(codec, width, height),
          optimizeForLatency: true,
        });
      } catch (e) {
        console.warn(
          "[blit] surface decoder configure failed:",
          surfaceId,
          codec,
          e,
        );
        decoder.close();
        return;
      }
    }
    this.decoders.set(surfaceId, {
      decoder,
      codec,
      pendingKeyframe: true,
      lastDescription: null,
      pendingAcks: 0,
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
