import type { BlitSurface } from "./types";
import { SURFACE_FRAME_FLAG_KEYFRAME } from "./types";

export type SurfaceFrameCallback = (
  surfaceId: number,
  frame: VideoFrame,
) => void;

export type SurfaceEventCallback = (
  surfaces: ReadonlyMap<number, BlitSurface>,
) => void;

interface DecoderEntry {
  decoder: VideoDecoder;
  pendingKeyframe: boolean;
}

export class SurfaceStore {
  private surfaces = new Map<number, BlitSurface>();
  private decoders = new Map<number, DecoderEntry>();
  private frameListeners = new Set<SurfaceFrameCallback>();
  private eventListeners = new Set<SurfaceEventCallback>();

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

  getSurface(surfaceId: number): BlitSurface | undefined {
    return this.surfaces.get(surfaceId);
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
    this.initDecoder(surfaceId);
    this.emitChange();
  }

  handleSurfaceDestroyed(surfaceId: number): void {
    this.surfaces.delete(surfaceId);
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
    const entry = this.decoders.get(surfaceId);
    if (!entry) return;
    const isKey = (flags & SURFACE_FRAME_FLAG_KEYFRAME) !== 0;
    if (entry.pendingKeyframe && !isKey) return;
    entry.pendingKeyframe = false;

    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      surface.width = width;
      surface.height = height;
      this.emitChange();
    }

    const chunk = new EncodedVideoChunk({
      type: isKey ? "key" : "delta",
      timestamp: _timestamp * 1000,
      data,
    });
    try {
      entry.decoder.decode(chunk);
    } catch {
      entry.pendingKeyframe = true;
    }
  }

  handleSurfaceTitle(surfaceId: number, title: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      surface.title = title;
      this.emitChange();
    }
  }

  handleSurfaceResized(surfaceId: number, width: number, height: number): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      surface.width = width;
      surface.height = height;
      this.emitChange();
    }
  }

  destroy(): void {
    for (const entry of this.decoders.values()) {
      entry.decoder.close();
    }
    this.decoders.clear();
    this.surfaces.clear();
    this.frameListeners.clear();
    this.eventListeners.clear();
  }

  private initDecoder(surfaceId: number): void {
    if (typeof VideoDecoder === "undefined") return;
    const decoder = new VideoDecoder({
      output: (frame) => {
        for (const listener of this.frameListeners) {
          listener(surfaceId, frame);
        }
        frame.close();
      },
      error: () => {
        const entry = this.decoders.get(surfaceId);
        if (entry) entry.pendingKeyframe = true;
      },
    });
    decoder.configure({
      codec: "avc1.42001f",
      optimizeForLatency: true,
    });
    this.decoders.set(surfaceId, { decoder, pendingKeyframe: true });
  }

  private emitChange(): void {
    for (const listener of this.eventListeners) {
      listener(this.surfaces);
    }
  }
}
