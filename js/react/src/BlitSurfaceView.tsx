import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import type { BlitSurface, ConnectionId } from "@blit-sh/core";
import {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_UP,
  SURFACE_POINTER_MOVE,
} from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "./BlitContext";

export interface BlitSurfaceViewProps {
  connectionId: ConnectionId;
  surfaceId: number;
  className?: string;
  style?: React.CSSProperties;
}

export interface BlitSurfaceViewHandle {
  canvas: HTMLCanvasElement | null;
  surface: BlitSurface | undefined;
}

export const BlitSurfaceView = forwardRef<
  BlitSurfaceViewHandle,
  BlitSurfaceViewProps
>(function BlitSurfaceView({ connectionId, surfaceId, className, style }, ref) {
  const workspace = useRequiredBlitWorkspace();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [surface, setSurface] = useState<BlitSurface | undefined>();

  useImperativeHandle(ref, () => ({
    canvas: canvasRef.current,
    surface,
  }));

  const conn = workspace.getConnection(connectionId);

  useEffect(() => {
    if (!conn) return;
    const store = conn.surfaceStore;
    setSurface(store.getSurface(surfaceId));
    return store.onChange(() => {
      setSurface(store.getSurface(surfaceId));
    });
  }, [conn, surfaceId]);

  useEffect(() => {
    if (!conn) return;
    const canvas = canvasRef.current;
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    return conn.surfaceStore.onFrame((sid, frame) => {
      if (sid !== surfaceId) return;
      if (
        canvas.width !== frame.displayWidth ||
        canvas.height !== frame.displayHeight
      ) {
        canvas.width = frame.displayWidth;
        canvas.height = frame.displayHeight;
      }
      ctx.drawImage(frame, 0, 0);
    });
  }, [conn, surfaceId]);

  const handleMouseEvent = useCallback(
    (e: React.MouseEvent<HTMLCanvasElement>) => {
      if (!conn) return;
      const canvas = canvasRef.current;
      if (!canvas) return;
      const rect = canvas.getBoundingClientRect();
      const scaleX = canvas.width / rect.width;
      const scaleY = canvas.height / rect.height;
      const x = Math.round((e.clientX - rect.left) * scaleX);
      const y = Math.round((e.clientY - rect.top) * scaleY);
      let type = SURFACE_POINTER_MOVE;
      if (e.type === "mousedown") type = SURFACE_POINTER_DOWN;
      else if (e.type === "mouseup") type = SURFACE_POINTER_UP;
      if (surface)
        conn.sendSurfacePointer(
          surface.sessionId,
          surfaceId,
          type,
          e.button,
          x,
          y,
        );
    },
    [conn, surface, surfaceId],
  );

  const handleWheel = useCallback(
    (e: React.WheelEvent<HTMLCanvasElement>) => {
      if (!conn) return;
      e.preventDefault();
      const axis = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? 1 : 0;
      const value = axis === 0 ? e.deltaY : e.deltaX;
      if (surface)
        conn.sendSurfaceAxis(
          surface.sessionId,
          surfaceId,
          axis,
          Math.round(value * 100),
        );
    },
    [conn, surface, surfaceId],
  );

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent<HTMLCanvasElement>) => {
      if (!conn) return;
      e.preventDefault();
      const keycode = domKeyToEvdev(e.code);
      if (keycode !== 0) {
        if (surface)
          conn.sendSurfaceInput(surface.sessionId, surfaceId, keycode, true);
      }
    },
    [conn, surface, surfaceId],
  );

  const handleKeyUp = useCallback(
    (e: React.KeyboardEvent<HTMLCanvasElement>) => {
      if (!conn) return;
      e.preventDefault();
      const keycode = domKeyToEvdev(e.code);
      if (keycode !== 0) {
        if (surface)
          conn.sendSurfaceInput(surface.sessionId, surfaceId, keycode, false);
      }
    },
    [conn, surface, surfaceId],
  );

  return (
    <canvas
      ref={canvasRef}
      className={className}
      style={{ display: "block", ...style }}
      tabIndex={0}
      width={surface?.width || 640}
      height={surface?.height || 480}
      onMouseDown={handleMouseEvent}
      onMouseUp={handleMouseEvent}
      onMouseMove={handleMouseEvent}
      onWheel={handleWheel}
      onKeyDown={handleKeyDown}
      onKeyUp={handleKeyUp}
      onFocus={() =>
        surface && conn?.sendSurfaceFocus(surface.sessionId, surfaceId)
      }
    />
  );
});

const EVDEV_MAP: Record<string, number> = {
  Escape: 1,
  Digit1: 2,
  Digit2: 3,
  Digit3: 4,
  Digit4: 5,
  Digit5: 6,
  Digit6: 7,
  Digit7: 8,
  Digit8: 9,
  Digit9: 10,
  Digit0: 11,
  Minus: 12,
  Equal: 13,
  Backspace: 14,
  Tab: 15,
  KeyQ: 16,
  KeyW: 17,
  KeyE: 18,
  KeyR: 19,
  KeyT: 20,
  KeyY: 21,
  KeyU: 22,
  KeyI: 23,
  KeyO: 24,
  KeyP: 25,
  BracketLeft: 26,
  BracketRight: 27,
  Enter: 28,
  ControlLeft: 29,
  KeyA: 30,
  KeyS: 31,
  KeyD: 32,
  KeyF: 33,
  KeyG: 34,
  KeyH: 35,
  KeyJ: 36,
  KeyK: 37,
  KeyL: 38,
  Semicolon: 39,
  Quote: 40,
  Backquote: 41,
  ShiftLeft: 42,
  Backslash: 43,
  KeyZ: 44,
  KeyX: 45,
  KeyC: 46,
  KeyV: 47,
  KeyB: 48,
  KeyN: 49,
  KeyM: 50,
  Comma: 51,
  Period: 52,
  Slash: 53,
  ShiftRight: 54,
  AltLeft: 56,
  Space: 57,
  CapsLock: 58,
  F1: 59,
  F2: 60,
  F3: 61,
  F4: 62,
  F5: 63,
  F6: 64,
  F7: 65,
  F8: 66,
  F9: 67,
  F10: 68,
  F11: 87,
  F12: 88,
  ArrowUp: 103,
  ArrowLeft: 105,
  ArrowRight: 106,
  ArrowDown: 108,
  Home: 102,
  End: 107,
  PageUp: 104,
  PageDown: 109,
  Insert: 110,
  Delete: 111,
  ControlRight: 97,
  AltRight: 100,
  MetaLeft: 125,
  MetaRight: 126,
};

function domKeyToEvdev(code: string): number {
  return EVDEV_MAP[code] ?? 0;
}
