import {
  onMount,
  onCleanup,
  createEffect,
  createSignal,
  type JSX,
} from "solid-js";
import { BlitTerminalSurface } from "@blit-sh/core";
import type { SessionId, TerminalPalette } from "@blit-sh/core";
import { useBlitContext, useRequiredBlitWorkspace } from "./BlitContext";
import { createBlitWorkspaceState } from "./hooks/createBlitWorkspace";

export interface BlitTerminalProps {
  sessionId: SessionId | null;
  fontFamily?: string;
  fontSize?: number;
  class?: string;
  style?: JSX.CSSProperties;
  palette?: TerminalPalette;
  readOnly?: boolean;
  showCursor?: boolean;
  onRender?: (renderMs: number) => void;
  scrollbarColor?: string;
  scrollbarWidth?: number;
  advanceRatio?: number;
  /** Callback to receive the underlying BlitTerminalSurface after mount. */
  surfaceRef?: (surface: BlitTerminalSurface | null) => void;
}

/**
 * BlitTerminal renders a blit terminal inside a WebGL canvas.
 *
 * This is a thin Solid wrapper over `BlitTerminalSurface` from `@blit-sh/core`.
 * It renders a container `<div>`, attaches the surface on mount, and uses
 * `createEffect` to forward reactive prop changes to the surface.
 */
export function BlitTerminal(props: BlitTerminalProps) {
  const ctx = useBlitContext();
  const workspace = useRequiredBlitWorkspace();
  const snapshot = createBlitWorkspaceState(workspace);

  let containerRef!: HTMLDivElement;
  // Use a signal so that effects re-run when the surface is created in
  // onMount.  Without this, effects that run during component init see
  // `null` and never retry after the surface is attached.
  const [surface, setSurface] = createSignal<BlitTerminalSurface | null>(null);

  onMount(() => {
    const s = new BlitTerminalSurface({
      sessionId: props.sessionId,
      fontFamily: props.fontFamily ?? ctx.fontFamily,
      fontSize: props.fontSize ?? ctx.fontSize,
      palette: props.palette ?? ctx.palette,
      readOnly: props.readOnly,
      showCursor: props.showCursor,
      onRender: props.onRender,
      scrollbarColor: props.scrollbarColor,
      scrollbarWidth: props.scrollbarWidth,
      advanceRatio: props.advanceRatio ?? ctx.advanceRatio,
    });

    s.setWorkspace(workspace);
    s.attach(containerRef);
    setSurface(s);
    props.surfaceRef?.(s);
  });

  onCleanup(() => {
    props.surfaceRef?.(null);
    surface()?.dispose();
    setSurface(null);
  });

  // Forward connection changes. Reading snapshot() inside createEffect makes
  // this reactive — it re-runs whenever the workspace snapshot changes
  // (connection status transitions, new sessions, etc.).
  createEffect(() => {
    const s = surface();
    const snap = snapshot();
    const session = props.sessionId
      ? (snap.sessions.find((ss) => ss.id === props.sessionId) ?? null)
      : null;
    const conn = session ? workspace.getConnection(session.connectionId) : null;
    s?.setConnection(conn);
  });

  // Forward prop changes.
  createEffect(() => surface()?.setSessionId(props.sessionId));
  createEffect(() => surface()?.setPalette(props.palette ?? ctx.palette));
  createEffect(() =>
    surface()?.setFontFamily(props.fontFamily ?? ctx.fontFamily),
  );
  createEffect(() => surface()?.setFontSize(props.fontSize ?? ctx.fontSize));
  createEffect(() => surface()?.setShowCursor(props.showCursor));
  createEffect(() => surface()?.setOnRender(props.onRender));
  createEffect(() =>
    surface()?.setAdvanceRatio(props.advanceRatio ?? ctx.advanceRatio),
  );
  createEffect(() => surface()?.setReadOnly(props.readOnly));

  // Re-send dimensions when connection becomes ready.
  createEffect(() => {
    const s = surface();
    const snap = snapshot();
    const session = props.sessionId
      ? (snap.sessions.find((ss) => ss.id === props.sessionId) ?? null)
      : null;
    const connection = session
      ? (snap.connections.find((c) => c.id === session.connectionId) ?? null)
      : null;
    if (
      connection?.status === "connected" &&
      props.sessionId !== null &&
      !props.readOnly
    ) {
      s?.resendSize();
    }
  });

  return (
    <div
      ref={containerRef}
      class={props.class}
      style={{
        position: "relative",
        overflow: "hidden",
        ...props.style,
      }}
    />
  );
}
