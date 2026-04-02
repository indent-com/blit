import { onMount, onCleanup, createEffect, type JSX } from "solid-js";
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
  let surface: BlitTerminalSurface | null = null;

  onMount(() => {
    surface = new BlitTerminalSurface({
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

    surface.setWorkspace(workspace);
    surface.attach(containerRef);
  });

  onCleanup(() => {
    surface?.dispose();
    surface = null;
  });

  // Forward connection changes. Reading snapshot() inside createEffect makes
  // this reactive — it re-runs whenever the workspace snapshot changes
  // (connection status transitions, new sessions, etc.).
  createEffect(() => {
    const snap = snapshot();
    const session = props.sessionId
      ? snap.sessions.find((s) => s.id === props.sessionId) ?? null
      : null;
    const conn = session
      ? workspace.getConnection(session.connectionId)
      : null;
    surface?.setConnection(conn);
  });

  // Forward prop changes.
  createEffect(() => surface?.setSessionId(props.sessionId));
  createEffect(() =>
    surface?.setPalette(props.palette ?? ctx.palette),
  );
  createEffect(() =>
    surface?.setFontFamily(props.fontFamily ?? ctx.fontFamily),
  );
  createEffect(() =>
    surface?.setFontSize(props.fontSize ?? ctx.fontSize),
  );
  createEffect(() => surface?.setShowCursor(props.showCursor));
  createEffect(() => surface?.setOnRender(props.onRender));
  createEffect(() =>
    surface?.setAdvanceRatio(props.advanceRatio ?? ctx.advanceRatio),
  );
  createEffect(() => surface?.setReadOnly(props.readOnly));

  // Re-send dimensions when connection becomes ready.
  createEffect(() => {
    const snap = snapshot();
    const session = props.sessionId
      ? snap.sessions.find((s) => s.id === props.sessionId) ?? null
      : null;
    const connection = session
      ? snap.connections.find((c) => c.id === session.connectionId) ?? null
      : null;
    if (
      connection?.status === "connected" &&
      props.sessionId !== null &&
      !props.readOnly
    ) {
      surface?.resendSize();
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
