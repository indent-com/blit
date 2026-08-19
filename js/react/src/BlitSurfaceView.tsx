import {
  forwardRef,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import {
  BlitSurfaceCanvas,
  detectCodecSupport,
  driveSurfaceResize,
} from "@blit-sh/core";
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
  /**
   * Whether this view owns its surface's size, resizing it to fill the
   * container.  Defaults to true.
   *
   * Pass `false` only for a passive preview — a dock card, a switcher
   * thumbnail — that shares another view's stream.  Such a view is served a
   * fixed downscale capped at a thumbnail cadence and takes no input at all:
   * every pointer, wheel, keyboard and IME path is gated on having a display
   * size.
   */
  resizable?: boolean;
  /**
   * Surface zoom factor, e.g. 1.25 for 125% or an exact 1.25x scale.
   *
   * How this value is interpreted is controlled by `zoomMode`. Defaults to
   * 1. Only resizable views drive the surface's scale, so it has no effect
   * elsewhere.
   */
  zoom?: number;
  /**
   * `relative` multiplies the display's DPI by `zoom`; `exact` uses `zoom` as
   * the absolute surface scale, independent of display DPI. Defaults to
   * `relative`.
   */
  zoomMode?: "relative" | "exact";
}

export interface BlitSurfaceViewHandle {
  canvas: HTMLCanvasElement | null;
  surface: BlitSurface | undefined;
}

export const BlitSurfaceView = forwardRef<
  BlitSurfaceViewHandle,
  BlitSurfaceViewProps
>(function BlitSurfaceView(
  {
    connectionId,
    surfaceId,
    className,
    style,
    live,
    touchMode,
    resizable,
    zoom,
    zoomMode,
  },
  ref,
) {
  const workspace = useRequiredBlitWorkspace();
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<BlitSurfaceCanvas | null>(null);
  // State, not a ref, so the resize effect below re-runs when the canvas is
  // rebuilt without having to restate the mount effect's dependencies.
  const [mounted, setMounted] = useState<BlitSurfaceCanvas | null>(null);

  // The driver reads zoom on every measurement rather than capturing it, so a
  // zoom change does not have to tear the observer down — that would
  // unsubscribe the view and cost a keyframe.
  const zoomRef = useRef({ zoom, zoomMode });
  zoomRef.current = { zoom, zoomMode };
  const driverRef = useRef<{ reapply(): void } | null>(null);

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
    // Unconditional: a page showing only passive previews still needs the
    // probe, or its subscribes carry codec_support=0 ("accept anything")
    // forever.
    detectCodecSupport();
    const surface = new BlitSurfaceCanvas({
      workspace,
      connectionId,
      surfaceId,
      live,
      resizable: resizable !== false,
      touchMode,
    });
    surface.attach(container);
    canvasRef.current = surface;
    setMounted(surface);
    return () => {
      setMounted(null);
      surface.dispose();
      canvasRef.current = null;
    };
    // `touchMode` and `zoom`/`zoomMode` are deliberately absent: they are
    // applied below instead. Listing them here would tear down and rebuild the
    // decoder and the server-side stream on settings that only change which
    // opcode input events use, or what scale the surface is asked for.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspace, connectionId, surfaceId, live, resizable]);

  useEffect(() => {
    canvasRef.current?.setTouchMode(touchMode ?? "direct");
  }, [touchMode]);

  // Own the surface's size while resizable.  The policy — even extents, the
  // leading/trailing-edge resize debounce, the zoom modes — lives in core so
  // every binding drives it identically; this only wires it to React's
  // lifecycle.  The canvas resolution is set immediately via setDisplaySize so
  // there is no CSS-scaling gap while waiting for the Wayland app to resize.
  useEffect(() => {
    const container = containerRef.current;
    if (resizable === false || !mounted || !container) return;
    const driver = driveSurfaceResize(mounted, container, () => ({
      zoom: zoomRef.current.zoom,
      mode: zoomRef.current.zoomMode,
    }));
    driverRef.current = driver;
    return () => {
      driverRef.current = null;
      driver.dispose();
    };
  }, [mounted, resizable]);

  // Tracks the zoom controls only. The box has not moved, so the observer will
  // never fire on its own.  Skips the mount run — the driver has already
  // applied the initial box with these values.
  const zoomApplied = useRef(false);
  useEffect(() => {
    if (!zoomApplied.current) {
      zoomApplied.current = true;
      return;
    }
    driverRef.current?.reapply();
  }, [zoom, zoomMode]);

  return (
    <div
      ref={containerRef}
      className={className}
      style={{ display: "block", ...style }}
    />
  );
});
