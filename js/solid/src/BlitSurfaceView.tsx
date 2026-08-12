import {
  onMount,
  onCleanup,
  createEffect,
  createSignal,
  on,
  untrack,
  Show,
  type JSX,
} from "solid-js";
import { BlitSurfaceCanvas, detectCodecSupport } from "@blit-sh/core";
import type { ConnectionId, SurfaceTouchMode } from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "./BlitContext";

export interface BlitSurfaceViewProps {
  connectionId: ConnectionId;
  surfaceId: number;
  class?: string;
  style?: JSX.CSSProperties;
  /** When true the inner canvas is focused so it receives keyboard input. */
  focus?: boolean;
  /** When true the surface is resized to fill the container. */
  resizable?: boolean;
  /**
   * When false, render only frames already present in the shared cache and
   * do not create a server-side video subscription. Defaults to true.
   */
  live?: boolean;
  /** How touchscreen contacts are delivered. Defaults to pointer emulation. */
  touchMode?: SurfaceTouchMode;
  /**
   * Surface zoom factor, e.g. 1.25 for 125% or an exact 1.25x scale.
   *
   * How this value is interpreted is controlled by `zoomMode`. Defaults to
   * 1. Only resizable views drive the surface's scale, so it has no effect
   * elsewhere.
   */
  zoom?: number;
  /**
   * `relative` multiplies the display's DPI by `zoom`; `exact` uses `zoom`
   * as the absolute surface scale, independent of display DPI. Defaults to
   * `relative` for backwards compatibility.
   */
  zoomMode?: "relative" | "exact";
}

/** Clamp to a range that stays useful at both ends: below 0.25 an app is
 *  handed a logical size most toolkits refuse to lay out, and above 4 one
 *  pane's demand for scale would dominate every co-viewer's stream. */
function clampZoom(zoom: number | undefined): number {
  if (typeof zoom !== "number" || !Number.isFinite(zoom) || zoom <= 0) return 1;
  return Math.min(4, Math.max(0.25, zoom));
}

export function BlitSurfaceView(props: BlitSurfaceViewProps) {
  const workspace = useRequiredBlitWorkspace();
  let containerRef!: HTMLDivElement;
  const [mounted, setMounted] = createSignal<BlitSurfaceCanvas | null>(null);
  const [videoError, setVideoError] = createSignal<string | null>(null);

  onMount(() => {
    const conn = workspace.getConnection(props.connectionId);
    if (conn?.surfaceStore.videoUnavailableReason) {
      setVideoError(conn.surfaceStore.videoUnavailableReason);
    }
    const surface = new BlitSurfaceCanvas({
      workspace,
      connectionId: props.connectionId,
      surfaceId: props.surfaceId,
      live: props.live,
      resizable: props.resizable,
      touchMode: props.touchMode,
    });
    surface.attach(containerRef);
    setMounted(surface);

    // Re-check after first frame attempt.
    const unsub = conn?.surfaceStore.onChange(() => {
      if (conn.surfaceStore.videoUnavailableReason) {
        setVideoError(conn.surfaceStore.videoUnavailableReason);
      }
    });
    onCleanup(() => unsub?.());
  });

  onCleanup(() => {
    mounted()?.dispose();
    setMounted(null);
  });

  createEffect(() => mounted()?.setConnectionId(props.connectionId));
  createEffect(() => mounted()?.setSurfaceId(props.surfaceId));
  createEffect(() => mounted()?.setLive(props.live !== false));
  createEffect(() => mounted()?.setTouchMode(props.touchMode ?? "pointer"));

  // Focus the canvas when props.focus is true AND the surface is mounted.
  createEffect(() => {
    const s = mounted();
    if (props.focus && s) {
      s.canvasElement?.focus();
    }
  });

  /** Set by the resize effect while it owns an observer; re-sends the
   *  current box after the zoom factor changes. */
  let reapplyZoom: (() => void) | null = null;

  // Observe container size and request a server-side resize when resizable.
  // The canvas resolution is set immediately via setDisplaySize so there is
  // no CSS-scaling gap while waiting for the Wayland app to resize.
  // The server resize request is debounced to avoid flooding the compositor
  // with redundant configure cycles and encoder recreations during a
  // drag-resize.
  createEffect(() => {
    const s = mounted();
    if (!props.resizable || !s) return;

    const fallbackScale120 = () =>
      Math.round((window.devicePixelRatio || 1) * 120);
    detectCodecSupport();

    // Read untracked: a zoom change must not tear this effect down and
    // rebuild the observer (that unsubscribes the view and costs a keyframe).
    // The dedicated effect below re-applies the last box instead.
    const zoom = () => clampZoom(untrack(() => props.zoom));
    const zoomMode = () => untrack(() => props.zoomMode ?? "relative");
    // The last box the observer reported, so a zoom change can be re-applied
    // without waiting for the container to change size — it never will.
    let lastBox: {
      cssW: number;
      cssH: number;
      physicalW?: number;
      physicalH?: number;
    } | null = null;

    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    let lastResizeAt = 0;
    let lastSentW = 0;
    let lastSentH = 0;
    let lastSentScale120 = 0;
    // Short, because the server coalesces on its own: a configure opens a
    // settle window there and every size that lands inside it is folded
    // into one configure at the end.  A long trailing edge here doesn't
    // save the compositor anything, it just delays the last size — and
    // some layout changes are two box changes in quick succession rather
    // than a drag.  Restoring a parked surface is one: the pane appears,
    // then widens again as the dock the card left closes, and the second
    // size used to sit here for 100 ms while the server built an encoder
    // for the first.
    const RESIZE_DEBOUNCE_MS = 30;
    // If no resize event for this long, the next one is treated as the
    // start of a fresh drag and fires immediately — so each user-visible
    // drag gets a leading-edge dispatch and the perceived reaction is
    // bounded by RTT rather than the trailing-edge debounce.
    const DRAG_GAP_MS = 250;

    const send = (w: number, h: number, scale120: number) => {
      if (w === lastSentW && h === lastSentH && scale120 === lastSentScale120)
        return;
      lastSentW = w;
      lastSentH = h;
      lastSentScale120 = scale120;
      s.requestResize(w, h, scale120);
    };

    const applySize = (
      cssW: number,
      cssH: number,
      physicalW?: number,
      physicalH?: number,
    ) => {
      // Even, because the encoder rounds each axis *down* to even on its own
      // (H.264/HEVC/AV1 NV12 sampling grids). Asking for an odd extent means
      // the frame comes back a pixel short of the pane on that axis only, so
      // the aspect no longer matches and `object-fit: contain` letterboxes
      // the difference. Giving up the odd pixel here costs nothing — it was
      // never going to carry image — and makes the server's rounding a no-op.
      const even = (n: number) => Math.max(2, n - (n % 2));
      const w = even(
        Math.round(physicalW ?? cssW * (window.devicePixelRatio || 1)),
      );
      const h = even(
        Math.round(physicalH ?? cssH * (window.devicePixelRatio || 1)),
      );
      if (w <= 0 || h <= 0) return;
      // The container's measured device-pixel ratio, which is what converts
      // the canvas's device pixels back to a CSS box.
      const cssScale120 =
        cssW > 0 && cssH > 0
          ? Math.round(((w / cssW + h / cssH) / 2) * 120)
          : fallbackScale120();
      // The pane always holds `w × h` device pixels. Relative zoom rides
      // on its DPI; exact zoom names the surface scale directly. A sub-1x
      // scale is meaningful: the server gives the app a larger logical
      // window, composites at Wayland's 1x floor, and downsamples the stream
      // into this pane.
      const scale120 = Math.max(
        1,
        Math.round((zoomMode() === "exact" ? 120 : cssScale120) * zoom()),
      );
      s.setDisplaySize(w, h, scale120, cssScale120);
      lastBox = { cssW, cssH, physicalW, physicalH };
      const now = performance.now();
      const isDragStart = now - lastResizeAt > DRAG_GAP_MS;
      lastResizeAt = now;
      // Leading edge: first event of a new interaction dispatches at
      // wire speed so the server pipeline (configure → repaint → encode)
      // starts as soon as possible.
      if (isDragStart) send(w, h, scale120);
      // Trailing edge: settle on the final size after the interaction
      // ends, in case it differs from the leading-edge value.
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => send(w, h, scale120), RESIZE_DEBOUNCE_MS);
    };

    const devicePixelSize = (entry: ResizeObserverEntry) => {
      const box = entry.devicePixelContentBoxSize;
      const size = Array.isArray(box) ? box[0] : box;
      if (!size) return null;
      const width = Math.round(size.inlineSize);
      const height = Math.round(size.blockSize);
      return width > 0 && height > 0 ? { width, height } : null;
    };

    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        if (width > 0 && height > 0) {
          const dpx = devicePixelSize(entry);
          applySize(width, height, dpx?.width, dpx?.height);
        }
      }
    });
    try {
      ro.observe(containerRef, { box: "device-pixel-content-box" });
    } catch {
      ro.observe(containerRef);
    }

    const rect = containerRef.getBoundingClientRect();
    if (rect.width > 0 && rect.height > 0) {
      applySize(rect.width, rect.height);
    }

    // Changing the zoom is a resize as far as the surface is concerned: the
    // box is unchanged, so the observer will never fire, but the logical
    // size the app is being handed just moved.  Re-apply the last box under
    // the new factor — through applySize, so it takes the same debounce and
    // the same de-duplication as a drag.
    reapplyZoom = () => {
      if (!lastBox) return;
      applySize(
        lastBox.cssW,
        lastBox.cssH,
        lastBox.physicalW,
        lastBox.physicalH,
      );
    };

    onCleanup(() => {
      reapplyZoom = null;
      clearTimeout(resizeTimer);
      ro.disconnect();
      s.setDisplaySize(null);
    });
  });

  // Tracks the zoom controls only, and `defer` skips the mount run — the
  // effect above has already applied the initial box with them.
  createEffect(
    on([() => props.zoom, () => props.zoomMode], () => reapplyZoom?.(), {
      defer: true,
    }),
  );

  return (
    <div
      ref={containerRef}
      class={props.class}
      style={{ display: "block", position: "relative", ...props.style }}
    >
      <Show when={videoError()}>
        {(err) => (
          <div
            style={{
              position: "absolute",
              inset: "0",
              display: "flex",
              "align-items": "center",
              "justify-content": "center",
              "text-align": "center",
              padding: "2em",
              color: "rgba(255,255,255,0.7)",
              "background-color": "rgba(0,0,0,0.8)",
              "font-size": "14px",
              "line-height": "1.5",
              "z-index": "1",
            }}
          >
            <div>
              <div style={{ "font-weight": "bold", "margin-bottom": "0.5em" }}>
                Surface video unavailable
              </div>
              <div>{err()}</div>
            </div>
          </div>
        )}
      </Show>
    </div>
  );
}
