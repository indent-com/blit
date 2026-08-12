import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import { BlitSurfaceCanvas } from "@blit-sh/core";
import type {
  BlitSurface,
  ConnectionId,
  SurfaceTouchMode,
} from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "./BlitContext";

export interface BlitSurfaceViewProps {
  connectionId: ConnectionId;
  surfaceId: number;
  className?: string;
  style?: React.CSSProperties;
  /** Render cached frames without owning a server stream when false. */
  live?: boolean;
  /** How touchscreen contacts are delivered. Defaults to pointer emulation. */
  touchMode?: SurfaceTouchMode;
}

export interface BlitSurfaceViewHandle {
  canvas: HTMLCanvasElement | null;
  surface: BlitSurface | undefined;
}

export const BlitSurfaceView = forwardRef<
  BlitSurfaceViewHandle,
  BlitSurfaceViewProps
>(function BlitSurfaceView(
  { connectionId, surfaceId, className, style, live, touchMode },
  ref,
) {
  const workspace = useRequiredBlitWorkspace();
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<BlitSurfaceCanvas | null>(null);

  useImperativeHandle(ref, () => ({
    get canvas() {
      return canvasRef.current?.canvasElement ?? null;
    },
    get surface() {
      return canvasRef.current?.surfaceInfo;
    },
  }));

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;
    const surface = new BlitSurfaceCanvas({
      workspace,
      connectionId,
      surfaceId,
      live,
      touchMode,
    });
    surface.attach(container);
    canvasRef.current = surface;
    return () => {
      surface.dispose();
      canvasRef.current = null;
    };
    // `touchMode` is deliberately absent: it is applied below instead. Listing
    // it here would tear down and rebuild the decoder and the server-side
    // stream on a setting that only changes which opcode input events use.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace, connectionId, surfaceId, live]);

  useEffect(() => {
    canvasRef.current?.setTouchMode(touchMode ?? "pointer");
  }, [touchMode]);

  return (
    <div
      ref={containerRef}
      className={className}
      style={{ display: "block", ...style }}
    />
  );
});
