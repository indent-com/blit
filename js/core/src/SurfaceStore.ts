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
  if (codec === "h265") return "hev1.1.6.L93.B0"; // Main profile, level 3.1
  return "avc1.42001f"; // Constrained Baseline, level 3.1
}

export class SurfaceStore {
  private surfaces = new Map<number, BlitSurface>();
  private decoders = new Map<number, DecoderEntry>();
  private canvases = new Map<number, CanvasEntry>();
  private frameListeners = new Set<SurfaceFrameCallback>();
  private eventListeners = new Set<SurfaceEventCallback>();

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
    if (entry.pendingKeyframe && !isKey) return;
    entry.pendingKeyframe = false;

    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      this.surfaces.set(surfaceId, { ...surface, width, height });
      this.emitChange();
    }

    this.ensureCanvas(surfaceId, width, height);

    try {
      const chunk = new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: _timestamp * 1000,
        data,
      });
      entry.decoder.decode(chunk);
    } catch {
      entry.pendingKeyframe = true;
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
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, width, height });
      this.ensureCanvas(surfaceId, width, height);
      this.emitChange();
    }
  }

  destroy(): void {
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
    this.decoders.set(surfaceId, { decoder, codec, pendingKeyframe: true });
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
