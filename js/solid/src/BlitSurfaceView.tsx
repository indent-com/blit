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
import {
  BlitSurfaceCanvas,
  detectCodecSupport,
  driveSurfaceResize,
} from "@blit-sh/core";
import type { ConnectionId, SurfaceTouchMode } from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "./BlitContext";

export interface BlitSurfaceViewProps {
  connectionId: ConnectionId;
  surfaceId: number;
  class?: string;
  style?: JSX.CSSProperties;
  /** When true the inner canvas is focused so it receives keyboard input. */
  focus?: boolean;
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

export function BlitSurfaceView(props: BlitSurfaceViewProps) {
  const workspace = useRequiredBlitWorkspace();
  /** Interactive unless explicitly opted out; see
   *  {@link BlitSurfaceViewProps.resizable}. */
  const resizable = () => props.resizable !== false;
  let containerRef!: HTMLDivElement;
  const [mounted, setMounted] = createSignal<BlitSurfaceCanvas | null>(null);
  const [videoError, setVideoError] = createSignal<string | null>(null);

  onMount(() => {
    // Unconditional: a page showing only passive previews still needs the probe,
    // or its subscribes carry codec_support=0 ("accept anything") forever.
    detectCodecSupport();
    const conn = workspace.getConnection(props.connectionId);
    if (conn?.surfaceStore.videoUnavailableReason) {
      setVideoError(conn.surfaceStore.videoUnavailableReason);
    }
    const surface = new BlitSurfaceCanvas({
      workspace,
      connectionId: props.connectionId,
      surfaceId: props.surfaceId,
      live: props.live,
      resizable: resizable(),
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
  createEffect(() => mounted()?.setTouchMode(props.touchMode ?? "direct"));

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

  // Own the surface's size while resizable.  The policy — even extents, the
  // leading/trailing-edge resize debounce, the zoom modes — lives in core so
  // every binding drives it identically; this only wires it to Solid's
  // lifecycle.  The canvas resolution is set immediately via setDisplaySize so
  // there is no CSS-scaling gap while waiting for the Wayland app to resize.
  createEffect(() => {
    const s = mounted();
    if (!resizable() || !s) return;
    // Read zoom untracked: a zoom change must not tear this effect down and
    // rebuild the observer (that unsubscribes the view and costs a keyframe).
    // The dedicated effect below re-applies the last box instead.
    const driver = driveSurfaceResize(s, containerRef, () => ({
      zoom: untrack(() => props.zoom),
      mode: untrack(() => props.zoomMode),
    }));
    reapplyZoom = () => driver.reapply();
    onCleanup(() => {
      reapplyZoom = null;
      driver.dispose();
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
