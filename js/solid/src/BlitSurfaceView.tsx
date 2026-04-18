import {
  onMount,
  onCleanup,
  createEffect,
  createSignal,
  Show,
  type JSX,
} from "solid-js";
import { BlitSurfaceCanvas, detectCodecSupport } from "@blit-sh/core";
import type { ConnectionId } from "@blit-sh/core";
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

  // Focus the canvas when props.focus is true AND the surface is mounted.
  createEffect(() => {
    const s = mounted();
    if (props.focus && s) {
      s.canvasElement?.focus();
    }
  });

  // Observe container size and request a server-side resize when resizable.
  // The canvas resolution is set immediately via setDisplaySize so there is
  // no CSS-scaling gap while waiting for the Wayland app to resize.
  // The server resize request is debounced to avoid flooding the compositor
  // with redundant configure cycles and encoder recreations during a
  // drag-resize.
  createEffect(() => {
    const s = mounted();
    if (!props.resizable || !s) return;

    const dprToScale120 = () => Math.round(devicePixelRatio * 120);
    detectCodecSupport();

    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    let lastResizeAt = 0;
    let lastSentW = 0;
    let lastSentH = 0;
    const RESIZE_DEBOUNCE_MS = 100;
    // If no resize event for this long, the next one is treated as the
    // start of a fresh drag and fires immediately — so each user-visible
    // drag gets a leading-edge dispatch and the perceived reaction is
    // bounded by RTT rather than the trailing-edge debounce.
    const DRAG_GAP_MS = 250;

    const send = (w: number, h: number) => {
      if (w === lastSentW && h === lastSentH) return;
      lastSentW = w;
      lastSentH = h;
      s.requestResize(w, h, dprToScale120());
    };

    const applySize = (cssW: number, cssH: number) => {
      const w = Math.round(cssW * devicePixelRatio);
      const h = Math.round(cssH * devicePixelRatio);
      if (w <= 0 || h <= 0) return;
      s.setDisplaySize(w, h);
      const now = performance.now();
      const isDragStart = now - lastResizeAt > DRAG_GAP_MS;
      lastResizeAt = now;
      // Leading edge: first event of a new interaction dispatches at
      // wire speed so the server pipeline (configure → repaint → encode)
      // starts as soon as possible.
      if (isDragStart) send(w, h);
      // Trailing edge: settle on the final size after the interaction
      // ends, in case it differs from the leading-edge value.
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => send(w, h), RESIZE_DEBOUNCE_MS);
    };

    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const { width, height } = entry.contentRect;
        if (width > 0 && height > 0) {
          applySize(width, height);
        }
      }
    });
    ro.observe(containerRef);

    const rect = containerRef.getBoundingClientRect();
    if (rect.width > 0 && rect.height > 0) {
      applySize(rect.width, rect.height);
    }

    onCleanup(() => {
      clearTimeout(resizeTimer);
      ro.disconnect();
      s.setDisplaySize(null);
    });
  });

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
