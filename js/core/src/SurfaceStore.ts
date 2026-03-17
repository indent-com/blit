import type { BlitSurface } from "./types";
import {
  SURFACE_FRAME_FLAG_KEYFRAME,
  SURFACE_FRAME_CODEC_MASK,
  SURFACE_FRAME_CODEC_H264,
  SURFACE_FRAME_CODEC_AV1,
  SURFACE_FRAME_CODEC_H265,
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

type SurfaceCodec = "h264" | "av1" | "h265";

interface DecoderEntry {
  decoder: VideoDecoder;
  codec: SurfaceCodec;
  pendingKeyframe: boolean;
  /** Last HVCC/AVCC description bytes, used to avoid reconfiguring on every
   *  keyframe when the parameter sets haven't changed. */
  lastDescription: string | null;
}

interface CanvasEntry {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
}

function codecFromFlags(flags: number): SurfaceCodec {
  const bits = flags & SURFACE_FRAME_CODEC_MASK;
  if (bits === SURFACE_FRAME_CODEC_AV1) return "av1";
  if (bits === SURFACE_FRAME_CODEC_H265) return "h265";
  return "h264";
}

/** WebCodecs codec string for the given surface codec. */
function codecString(codec: SurfaceCodec): string {
  if (codec === "av1") return "av01.0.01M.08"; // Main profile, level 2.1, 8-bit
  // hev1 / avc3: parameter sets in-band (description optional but provided
  // on keyframes for platform decoders that need it up front).
  if (codec === "h265") return "hev1.1.6.L93.B0"; // Main profile, level 3.1
  return "avc3.42001f"; // Constrained Baseline, level 3.1
}

// ---------------------------------------------------------------------------
// Annex B → length-prefixed NAL conversion
//
// The server sends Annex B bitstreams (start-code delimited NAL units).
// WebCodecs defaults to length-prefixed containers (AVCC for H.264,
// HVCC for H.265).  The `avc.format` / `hevc.format` annexb hints are
// not universally supported (macOS VideoToolbox rejects with -12909,
// Windows Media Foundation doesn't support the option at all), so we
// convert Annex B → 4-byte-length-prefixed on every frame.
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

/** HEVC NAL unit type from the first byte of the NAL. */
function hevcNalType(nal: Uint8Array): number {
  return (nal[0] >>> 1) & 0x3f;
}

/** Remove Annex B emulation prevention bytes (0x00 0x00 0x03 → 0x00 0x00). */
function removeEPB(nal: Uint8Array): Uint8Array {
  const out: number[] = [];
  let i = 0;
  while (i < nal.length) {
    if (
      i + 2 < nal.length &&
      nal[i] === 0 &&
      nal[i + 1] === 0 &&
      nal[i + 2] === 3
    ) {
      out.push(0, 0);
      i += 3;
    } else {
      out.push(nal[i]);
      i++;
    }
  }
  return new Uint8Array(out);
}

/**
 * Build an HEVCDecoderConfigurationRecord (ISO 14496-15 §8.3.3.1)
 * from raw VPS, SPS, and PPS NAL units (without start codes).
 */
function buildHvccDescription(
  vps: Uint8Array,
  sps: Uint8Array,
  pps: Uint8Array,
): ArrayBuffer {
  // Parse profile/tier/level from VPS RBSP (must strip emulation prevention
  // bytes first — the raw NAL contains 0x00 0x00 0x03 sequences that shift
  // byte offsets).
  const rbsp = removeEPB(vps);
  const profileSpace = (rbsp[6] >>> 6) & 0x3;
  const tierFlag = (rbsp[6] >>> 5) & 0x1;
  const profileIdc = rbsp[6] & 0x1f;
  const compatFlags =
    (rbsp[7] << 24) | (rbsp[8] << 16) | (rbsp[9] << 8) | rbsp[10];
  const constraintBytes = rbsp.subarray(11, 17); // 6 bytes
  const levelIdc = rbsp[17];

  // Fixed fields for our use case
  const lengthSizeMinusOne = 3; // 4-byte NAL lengths

  const arrays = [
    { type: 32, nals: [vps] }, // VPS
    { type: 33, nals: [sps] }, // SPS
    { type: 34, nals: [pps] }, // PPS
  ];

  // Calculate total size
  let size = 23; // fixed header
  for (const a of arrays) {
    size += 3; // array header: completeness+type(1) + numNalus(2)
    for (const n of a.nals) size += 2 + n.length; // nalUnitLength(2) + data
  }

  const buf = new ArrayBuffer(size);
  const v = new DataView(buf);
  const u = new Uint8Array(buf);
  let o = 0;

  v.setUint8(o++, 1); // configurationVersion
  v.setUint8(o++, (profileSpace << 6) | (tierFlag << 5) | profileIdc);
  v.setUint32(o, compatFlags);
  o += 4;
  u.set(constraintBytes, o);
  o += 6;
  v.setUint8(o++, levelIdc);
  v.setUint16(o, 0xf000);
  o += 2; // min_spatial_segmentation_idc (reserved 4 bits + 12 bits = 0)
  v.setUint8(o++, 0xfc); // parallelismType (reserved 6 bits + 2 bits = 0)
  v.setUint8(o++, 0xfc | 1); // chromaFormat = 1 (4:2:0)
  v.setUint8(o++, 0xf8); // bitDepthLumaMinus8 = 0 (reserved 5 bits + 3 bits)
  v.setUint8(o++, 0xf8); // bitDepthChromaMinus8 = 0
  v.setUint16(o, 0);
  o += 2; // avgFrameRate = 0
  v.setUint8(o++, (lengthSizeMinusOne & 0x3) | 0x0c); // constantFrameRate=0, numTemporalLayers=1, temporalIdNested=1, lengthSizeMinusOne=3
  v.setUint8(o++, arrays.length); // numOfArrays

  for (const a of arrays) {
    v.setUint8(o++, 0x80 | (a.type & 0x3f)); // array_completeness=1 + NAL type
    v.setUint16(o, a.nals.length);
    o += 2;
    for (const n of a.nals) {
      v.setUint16(o, n.length);
      o += 2;
      u.set(n, o);
      o += n.length;
    }
  }

  return buf;
}

export class SurfaceStore {
  private surfaces = new Map<number, BlitSurface>();
  private decoders = new Map<number, DecoderEntry>();
  private canvases = new Map<number, CanvasEntry>();
  private frameListeners = new Set<SurfaceFrameCallback>();
  private eventListeners = new Set<SurfaceEventCallback>();
  private _diag = { received: 0, decoded: 0, output: 0, dropped: 0, errors: 0 };
  private _diagTimer: ReturnType<typeof setInterval> | null = null;

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

  /** Debug info about active surface decoders. */
  getDebugStats(): {
    surfaceId: number;
    codec: string;
    width: number;
    height: number;
  }[] {
    const result: {
      surfaceId: number;
      codec: string;
      width: number;
      height: number;
    }[] = [];
    for (const [id, entry] of this.decoders) {
      const surface = this.surfaces.get(id);
      result.push({
        surfaceId: id,
        codec: entry.codec,
        width: surface?.width ?? 0,
        height: surface?.height ?? 0,
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

  handleSurfaceCreated(
    sessionId: number,
    surfaceId: number,
    parentId: number,
    width: number,
    height: number,
    title: string,
    appId: string,
  ): void {
    this.surfaces.set(surfaceId, {
      sessionId,
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
    const codec = codecFromFlags(flags);

    // Ensure we have a decoder for this surface with the right codec.
    let entry = this.decoders.get(surfaceId);
    if (!entry || entry.codec !== codec) {
      // Close old decoder if codec changed.
      if (entry) entry.decoder.close();
      this.decoders.delete(surfaceId);
      this.initDecoder(surfaceId, codec);
      entry = this.decoders.get(surfaceId);
    }
    if (!entry) return;

    const isKey = (flags & SURFACE_FRAME_FLAG_KEYFRAME) !== 0;
    if (entry.pendingKeyframe && !isKey) {
      this._diag.dropped++;
      return;
    }
    entry.pendingKeyframe = false;

    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      // Update dimensions silently — no emitChange().  Dimension updates
      // from every frame cause Solid re-renders → BlitSurfaceCanvas
      // unmount/remount → subscribe/unsubscribe spam.  The canvas gets
      // the correct size from the decoded frame directly.
      this.surfaces.set(surfaceId, { ...surface, width, height });
    }

    this.ensureCanvas(surfaceId, width, height);

    try {
      const chunk = new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: _timestamp * 1000,
        data,
      });
      entry.decoder.decode(chunk);
      this._diag.decoded++;
    } catch {
      if (entry) entry.pendingKeyframe = true;
      this._diag.errors++;
    }
  }

  handleSurfaceTitle(surfaceId: number, title: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, title });
      this.emitChange();
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
      this.surfaces.set(surfaceId, { ...surface, width, height });
      this.ensureCanvas(surfaceId, width, height);
      if (significant) this.emitChange();
    }
  }

  /** Build HEVCDecoderConfigurationRecord from NALs in a keyframe. */
  private buildHevcDescription(nals: Uint8Array[]): ArrayBuffer | undefined {
    let vps: Uint8Array | undefined;
    let sps: Uint8Array | undefined;
    let pps: Uint8Array | undefined;
    for (const nal of nals) {
      const t = hevcNalType(nal);
      if (t === 32) vps = nal;
      else if (t === 33) sps = nal;
      else if (t === 34) pps = nal;
    }
    if (vps && sps && pps) return buildHvccDescription(vps, sps, pps);
    return undefined;
  }

  destroy(): void {
    if (this._diagTimer !== null) {
      clearInterval(this._diagTimer);
      this._diagTimer = null;
    }
    for (const entry of this.decoders.values()) {
      entry.decoder.close();
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    // Preserve eventListeners and frameListeners — they are owned by
    // long-lived UI components (e.g. the Workspace surface aggregation
    // effect) and must survive disconnect/reconnect cycles.  Notify
    // listeners so they see the now-empty surface set.
    this.emitChange();
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

  private initDecoder(surfaceId: number, codec: SurfaceCodec): void {
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

        // Notify listeners — they blit from getCanvas().
        for (const listener of this.frameListeners) {
          try {
            listener(surfaceId);
          } catch {
            // Prevent a single broken listener from blocking others.
          }
        }
      },
      error: (e: DOMException) => {
        console.warn("[blit] surface decoder error:", surfaceId, e.message);
        const entry = this.decoders.get(surfaceId);
        if (entry) {
          try {
            entry.decoder.close();
          } catch {
            // Already closed.
          }
          // Remove the broken decoder so the next frame triggers
          // re-initialization via initDecoder().
          this.decoders.delete(surfaceId);
        }
      },
    });
    try {
      decoder.configure({
        codec: codecString(codec),
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
    this.decoders.set(surfaceId, {
      decoder,
      codec,
      pendingKeyframe: true,
      lastDescription: null,
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
