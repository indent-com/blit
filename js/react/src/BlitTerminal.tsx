import { useEffect, useImperativeHandle, useRef } from "react";
import type { Ref } from "react";
import type { ConnectionStatus } from "@blit-sh/core";
import type { Terminal } from "@blit-sh/browser";
import { BlitTerminalSurface } from "@blit-sh/core";
import type { BlitTerminalProps } from "./types";
import { useBlitContext, useRequiredBlitWorkspace } from "./BlitContext";
import { useBlitConnection } from "./hooks/useBlitConnection";
import { useBlitSession } from "./hooks/useBlitSession";

// ---------------------------------------------------------------------------
// Public handle exposed via ref
// ---------------------------------------------------------------------------

export interface BlitTerminalHandle {
  /** The underlying WASM Terminal instance, if initialised. */
  terminal: Terminal | null;
  /** Current grid dimensions. */
  rows: number;
  cols: number;
  /** Current connection status. */
  status: ConnectionStatus;
  /** Focus the input sink so the terminal can receive keyboard events. */
  focus(): void;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/**
 * BlitTerminal renders a blit terminal inside a WebGL canvas.
 *
 * This is a thin wrapper over `BlitTerminalSurface` from `@blit-sh/core`.
 * It renders a container `<div>`, attaches the surface to it on mount,
 * and forwards prop changes to the surface's setters.
 */
export function BlitTerminal({
  ref,
  ...props
}: BlitTerminalProps & { ref?: Ref<BlitTerminalHandle> }) {
  const ctx = useBlitContext();
  const workspace = useRequiredBlitWorkspace();
  const session = useBlitSession(props.sessionId);
  const connection = useBlitConnection(session?.connectionId);
  const blitConn = session
    ? workspace.getConnection(session.connectionId)
    : null;

  const {
    sessionId,
    fontFamily = ctx.fontFamily,
    fontSize = ctx.fontSize,
    className,
    style,
    palette = ctx.palette,
    readOnly,
    showCursor,
    onRender,
    scrollbarColor,
    scrollbarWidth,
    advanceRatio = ctx.advanceRatio,
  } = props;

  const containerRef = useRef<HTMLDivElement>(null);
  const surfaceRef = useRef<BlitTerminalSurface | null>(null);

  // Create the surface once on mount.
  useEffect(() => {
    const surface = new BlitTerminalSurface({
      sessionId,
      fontFamily,
      fontSize,
      palette,
      readOnly,
      showCursor,
      onRender,
      scrollbarColor,
      scrollbarWidth,
      advanceRatio,
    });
    surfaceRef.current = surface;
    props.surfaceRef?.(surface);

    return () => {
      props.surfaceRef?.(null);
      surface.dispose();
      surfaceRef.current = null;
    };
    // Only create/destroy on mount/unmount. Props are forwarded via setters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Attach/detach to the container div.
  useEffect(() => {
    const surface = surfaceRef.current;
    const container = containerRef.current;
    if (!surface || !container) return;
    surface.attach(container);
    return () => surface.detach();
  }, []);

  // Forward workspace + connection.
  useEffect(() => {
    surfaceRef.current?.setWorkspace(workspace);
  }, [workspace]);

  useEffect(() => {
    surfaceRef.current?.setConnection(blitConn);
  }, [blitConn]);

  // Forward all prop changes.
  useEffect(() => {
    surfaceRef.current?.setSessionId(sessionId);
  }, [sessionId]);

  useEffect(() => {
    surfaceRef.current?.setPalette(palette);
  }, [palette]);

  useEffect(() => {
    surfaceRef.current?.setFontFamily(fontFamily);
  }, [fontFamily]);

  useEffect(() => {
    surfaceRef.current?.setFontSize(fontSize);
  }, [fontSize]);

  useEffect(() => {
    surfaceRef.current?.setShowCursor(showCursor);
  }, [showCursor]);

  useEffect(() => {
    surfaceRef.current?.setOnRender(onRender);
  }, [onRender]);

  useEffect(() => {
    surfaceRef.current?.setAdvanceRatio(advanceRatio);
  }, [advanceRatio]);

  useEffect(() => {
    surfaceRef.current?.setReadOnly(readOnly);
  }, [readOnly]);

  // Re-send dimensions when connection becomes ready.
  const status: ConnectionStatus = connection?.status ?? "disconnected";
  useEffect(() => {
    if (status === "connected" && sessionId !== null && !readOnly) {
      surfaceRef.current?.resendSize();
    }
  }, [status, sessionId, readOnly]);

  // Imperative handle.
  useImperativeHandle(
    ref,
    () => ({
      get terminal() {
        return surfaceRef.current?.currentTerminal ?? null;
      },
      get rows() {
        return surfaceRef.current?.rows ?? 24;
      },
      get cols() {
        return surfaceRef.current?.cols ?? 80;
      },
      status,
      focus() {
        surfaceRef.current?.focus();
      },
    }),
    [status],
  );

  return (
    <div
      ref={containerRef}
      className={className}
      style={{
        position: "relative",
        overflow: "hidden",
        ...style,
      }}
    />
  );
}
