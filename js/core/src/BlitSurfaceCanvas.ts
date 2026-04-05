import type { ConnectionId, BlitSurface } from "./types";
import {
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H265,
} from "./types";
import type { BlitWorkspace } from "./BlitWorkspace";
import type { BlitConnection } from "./BlitConnection";
import {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_UP,
  SURFACE_POINTER_MOVE,
} from "./protocol";

/** Cached codec support bitmask.  Computed once, reused for all resize messages. */
let _codecSupport: number | null = null;

/**
 * Probe which video codecs the browser can decode via WebCodecs and return
 * a bitmask of CODEC_SUPPORT_* flags.  Result is cached after first call.
 */
export async function detectCodecSupport(): Promise<number> {
  if (_codecSupport !== null) return _codecSupport;
  if (typeof VideoDecoder === "undefined") {
    _codecSupport = 0;
    return 0;
  }
  let mask = 0;
  const checks: [string, number][] = [
    ["avc1.42001f", CODEC_SUPPORT_H264],
    ["av01.0.01M.08", CODEC_SUPPORT_AV1],
    ["hev1.1.6.L93.B0", CODEC_SUPPORT_H265],
  ];
  await Promise.all(
    checks.map(async ([codec, bit]) => {
      try {
        const r = await VideoDecoder.isConfigSupported({
          codec,
          codedWidth: 1920,
          codedHeight: 1080,
        });
        if (r.supported) mask |= bit;
      } catch {
        // not supported
      }
    }),
  );
  _codecSupport = mask;
  return mask;
}

/** Return the cached codec support, or 0 if not yet probed. */
export function getCodecSupport(): number {
  return _codecSupport ?? 0;
}

// ---------------------------------------------------------------------------
// CapsLock state tracking
// ---------------------------------------------------------------------------

// Track the believed CapsLock state inside each connection's compositor.
// Keyed by connectionId.  Defaults to false because XkbConfig::default()
// starts with all lock modifiers off.  A module-level map is used so the
// state survives across BlitSurfaceCanvas instances that share the same
// connection (e.g. switching surfaces in a BSP layout).
const _compositorCapsLock = new Map<string, boolean>();

// ---------------------------------------------------------------------------
// EVDEV keycode map (DOM KeyboardEvent.code → Linux evdev scancode)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// BlitSurfaceCanvas
// ---------------------------------------------------------------------------

export interface BlitSurfaceCanvasOptions {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  surfaceId: number;
}

/**
 * Framework-agnostic surface canvas. Manages a `<canvas>` element that renders
 * decoded video frames from a Wayland-like surface, and forwards
 * pointer / keyboard / wheel input back to the server.
 *
 * Framework bindings (React, Solid, etc.) attach this to a container element
 * and forward option changes via setters.
 */
export class BlitSurfaceCanvas {
  private _workspace: BlitWorkspace;
  private _connectionId: ConnectionId;
  private _surfaceId: number;

  private container: HTMLElement | null = null;
  private canvas: HTMLCanvasElement | null = null;
  private ctx: CanvasRenderingContext2D | null = null;

  private surface: BlitSurface | undefined;
  private disposed = false;

  /** Track which mouse buttons are currently pressed so we can send synthetic
   *  pointer-up events on dispose — preventing a dangling compositor grab. */
  private pressedButtons = new Set<number>();

  /** Track which keyboard keys are currently pressed (evdev keycodes) so we
   *  can release them when focus leaves or the canvas is disposed — preventing
   *  stuck modifiers and runaway key-repeat in the compositor. */
  private pressedKeys = new Set<number>();

  /**
   * When non-null the canvas internal resolution is controlled externally
   * (by the framework binding's ResizeObserver) and frames are drawn
   * scaled to fill the canvas rather than the canvas being resized to
   * match each frame.
   */
  private _displaySize: { width: number; height: number } | null = null;

  // subscriptions
  private unsubFrame: (() => void) | null = null;
  private unsubChange: (() => void) | null = null;

  // bound event handlers
  private boundMouseDown: ((e: MouseEvent) => void) | null = null;
  private boundMouseUp: ((e: MouseEvent) => void) | null = null;
  private boundMouseMove: ((e: MouseEvent) => void) | null = null;
  private boundWheel: ((e: WheelEvent) => void) | null = null;
  private boundKeyDown: ((e: KeyboardEvent) => void) | null = null;
  private boundKeyUp: ((e: KeyboardEvent) => void) | null = null;
  private boundFocus: (() => void) | null = null;
  private boundBlur: (() => void) | null = null;
  private boundContextMenu: ((e: Event) => void) | null = null;

  constructor(options: BlitSurfaceCanvasOptions) {
    this._workspace = options.workspace;
    this._connectionId = options.connectionId;
    this._surfaceId = options.surfaceId;
  }

  // -----------------------------------------------------------------------
  // Public API
  // -----------------------------------------------------------------------

  get surfaceInfo(): BlitSurface | undefined {
    return this.surface;
  }

  get canvasElement(): HTMLCanvasElement | null {
    return this.canvas;
  }

  attach(container: HTMLElement): void {
    if (this.disposed) return;
    this.container = container;

    const canvas = document.createElement("canvas");
    canvas.tabIndex = 0;
    canvas.style.display = "block";
    canvas.style.outline = "none";
    canvas.style.width = "100%";
    canvas.style.height = "100%";
    if (this._displaySize) {
      // Resizable mode: canvas resolution is pinned by the framework
      // binding.  No object-fit needed since canvas.width/height matches
      // the container's physical pixel size.
      canvas.width = this._displaySize.width;
      canvas.height = this._displaySize.height;
    } else {
      // Non-resizable (thumbnail) mode: scale to fit.
      canvas.style.objectFit = "contain";
      canvas.width = this.surface?.width || 640;
      canvas.height = this.surface?.height || 480;
    }
    container.appendChild(canvas);

    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");

    this.subscribe();
    this.attachEvents();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.releaseAllKeys();
    this.releaseAllButtons();
    this.serverUnsubscribe();
    this.detachEvents();
    this.unsubFrame?.();
    this.unsubChange?.();
    this.unsubFrame = null;
    this.unsubChange = null;
    if (this.canvas && this.container) {
      this.container.removeChild(this.canvas);
    }
    this.canvas = null;
    this.ctx = null;
    this.container = null;
  }

  setConnectionId(connectionId: ConnectionId): void {
    if (this._connectionId === connectionId) return;
    this._connectionId = connectionId;
    this.resubscribe();
  }

  setSurfaceId(surfaceId: number): void {
    if (this._surfaceId === surfaceId) return;
    this._surfaceId = surfaceId;
    this.resubscribe();
  }

  /**
   * Request the server to resize the surface to the given pixel dimensions.
   * The server will respond with a SURFACE_RESIZED message that updates the
   * surface metadata and canvas size via the normal onChange path.
   */
  requestResize(
    width: number,
    height: number,
    scale120: number = 0,
    codecSupport: number = 0,
  ): void {
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    const w = Math.round(width);
    const h = Math.round(height);
    if (w <= 0 || h <= 0) return;
    conn.sendSurfaceResize(
      this.surface.sessionId,
      this._surfaceId,
      w,
      h,
      scale120,
      codecSupport,
    );
  }

  /**
   * Set the display (canvas backing-buffer) size in physical pixels.
   * When set, the canvas resolution is pinned to these dimensions and frames
   * are drawn scaled to fill rather than the canvas being resized to match
   * each incoming frame.  Call with `null` to revert to frame-tracking mode.
   *
   * This should be called by the framework binding's ResizeObserver so the
   * canvas is immediately at the correct resolution — no CSS scaling needed.
   */
  setDisplaySize(width: number | null, height?: number): void {
    if (width == null) {
      this._displaySize = null;
      if (this.canvas) {
        this.canvas.style.objectFit = "contain";
      }
      return;
    }
    const w = Math.round(width);
    const h = Math.round(height!);
    if (w <= 0 || h <= 0) return;
    this._displaySize = { width: w, height: h };
    if (this.canvas) {
      // Switch from object-fit scaling to display-size pinned mode.
      this.canvas.style.objectFit = "";
      if (this.canvas.width !== w || this.canvas.height !== h) {
        this.canvas.width = w;
        this.canvas.height = h;
      }
      // Re-blit the last frame at the new canvas size.
      const conn = this.getConn();
      if (conn) this.blitFromStore(conn.surfaceStore);
    }
  }

  // -----------------------------------------------------------------------
  // Connection helper
  // -----------------------------------------------------------------------

  private getConn(): BlitConnection | null {
    return (this._workspace as any).getConnection(this._connectionId) ?? null;
  }

  // -----------------------------------------------------------------------
  // Subscriptions
  // -----------------------------------------------------------------------

  private subscribe(): void {
    const conn = this.getConn();
    if (!conn) return;
    const store = conn.surfaceStore;

    this.surface = store.getSurface(this._surfaceId);

    // Tell the server we want frames for this surface, but only if the
    // browser can actually decode the video.  Subscribing when WebCodecs
    // is unavailable (non-secure context) drives the server encoder for
    // nothing and can crash it.
    if (this.surface && store.canDecodeVideo) {
      conn.sendSurfaceSubscribe(this.surface.sessionId, this._surfaceId);
    }

    // Paint the latest frame immediately so newly-mounted views aren't blank.
    this.blitFromStore(store);

    this.unsubChange = store.onChange(() => {
      const prev = this.surface;
      this.surface = store.getSurface(this._surfaceId);
      // Late-subscribe: the surface info may arrive after we attach.
      if (!prev && this.surface && store.canDecodeVideo) {
        conn.sendSurfaceSubscribe(this.surface.sessionId, this._surfaceId);
        // Update canvas size to match actual surface dimensions,
        // unless the display size is pinned by a ResizeObserver.
        if (this.canvas && !this._displaySize) {
          this.canvas.width = this.surface.width;
          this.canvas.height = this.surface.height;
        }
      }
    });

    this.unsubFrame = store.onFrame((sid) => {
      if (sid !== this._surfaceId) return;
      this.blitFromStore(store);
    });
  }

  /** Copy the shared backing canvas onto our visible canvas. */
  private blitFromStore(store: import("./SurfaceStore").SurfaceStore): void {
    const src = store.getCanvas(this._surfaceId);
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!src || !canvas || !ctx) return;
    if (src.width === 0 || src.height === 0) return;

    if (this._displaySize) {
      // Resizable mode: canvas resolution is pinned to the container's
      // physical pixel size.  Draw the source frame scaled to fill.
      ctx.drawImage(src, 0, 0, canvas.width, canvas.height);
    } else {
      // Frame-tracking mode: canvas resolution follows the source frame.
      if (canvas.width !== src.width || canvas.height !== src.height) {
        canvas.width = src.width;
        canvas.height = src.height;
      }
      ctx.drawImage(src, 0, 0);
    }
  }

  private resubscribe(): void {
    this.serverUnsubscribe();
    this.unsubFrame?.();
    this.unsubChange?.();
    this.unsubFrame = null;
    this.unsubChange = null;
    if (!this.disposed) this.subscribe();
  }

  private serverUnsubscribe(): void {
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    conn.sendSurfaceUnsubscribe(this.surface.sessionId, this._surfaceId);
  }

  // -----------------------------------------------------------------------
  // Event handling
  // -----------------------------------------------------------------------

  private attachEvents(): void {
    const canvas = this.canvas;
    if (!canvas) return;

    this.boundMouseDown = (e) => this.handleMouse(e, SURFACE_POINTER_DOWN);
    this.boundMouseUp = (e) => this.handleMouse(e, SURFACE_POINTER_UP);
    this.boundMouseMove = (e) => this.handleMouse(e, SURFACE_POINTER_MOVE);
    this.boundWheel = (e) => this.handleWheel(e);
    this.boundKeyDown = (e) => this.handleKey(e, true);
    this.boundKeyUp = (e) => this.handleKey(e, false);
    this.boundFocus = () => this.handleFocus();
    this.boundBlur = () => this.handleBlur();
    this.boundContextMenu = (e) => e.preventDefault();

    canvas.addEventListener("mousedown", this.boundMouseDown);
    canvas.addEventListener("mouseup", this.boundMouseUp);
    canvas.addEventListener("mousemove", this.boundMouseMove);
    canvas.addEventListener("wheel", this.boundWheel, { passive: false });
    canvas.addEventListener("keydown", this.boundKeyDown);
    canvas.addEventListener("keyup", this.boundKeyUp);
    canvas.addEventListener("focus", this.boundFocus);
    canvas.addEventListener("blur", this.boundBlur);
    canvas.addEventListener("contextmenu", this.boundContextMenu);
  }

  private detachEvents(): void {
    const canvas = this.canvas;
    if (!canvas) return;

    if (this.boundMouseDown)
      canvas.removeEventListener("mousedown", this.boundMouseDown);
    if (this.boundMouseUp)
      canvas.removeEventListener("mouseup", this.boundMouseUp);
    if (this.boundMouseMove)
      canvas.removeEventListener("mousemove", this.boundMouseMove);
    if (this.boundWheel) canvas.removeEventListener("wheel", this.boundWheel);
    if (this.boundKeyDown)
      canvas.removeEventListener("keydown", this.boundKeyDown);
    if (this.boundKeyUp) canvas.removeEventListener("keyup", this.boundKeyUp);
    if (this.boundFocus) canvas.removeEventListener("focus", this.boundFocus);
    if (this.boundBlur) canvas.removeEventListener("blur", this.boundBlur);
    if (this.boundContextMenu)
      canvas.removeEventListener("contextmenu", this.boundContextMenu);
  }

  private handleMouse(e: MouseEvent, type: number): void {
    const conn = this.getConn();
    if (!conn || !this.canvas || !this.surface) return;
    if (type === SURFACE_POINTER_DOWN) {
      this.canvas.focus();
      this.pressedButtons.add(e.button);
    } else if (type === SURFACE_POINTER_UP) {
      this.pressedButtons.delete(e.button);
    }
    const rect = this.canvas.getBoundingClientRect();
    // Send logical (CSS-pixel) coordinates — the compositor and CLI both
    // operate in the Wayland surface's logical coordinate space.
    const x = Math.round(e.clientX - rect.left);
    const y = Math.round(e.clientY - rect.top);
    conn.sendSurfacePointer(
      this.surface.sessionId,
      this._surfaceId,
      type,
      e.button,
      x,
      y,
    );
  }

  /** Send synthetic pointer-up for any buttons still held.  Prevents the
   *  compositor's implicit pointer grab from outliving this canvas. */
  private releaseAllButtons(): void {
    if (this.pressedButtons.size === 0) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    for (const button of this.pressedButtons) {
      conn.sendSurfacePointer(
        this.surface.sessionId,
        this._surfaceId,
        SURFACE_POINTER_UP,
        button,
        0,
        0,
      );
    }
    this.pressedButtons.clear();
  }

  private handleWheel(e: WheelEvent): void {
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    e.preventDefault();
    const axis = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? 1 : 0;
    const value = axis === 0 ? e.deltaY : e.deltaX;
    conn.sendSurfaceAxis(
      this.surface.sessionId,
      this._surfaceId,
      axis,
      Math.round(value * 100),
    );
  }

  private handleKey(e: KeyboardEvent, pressed: boolean): void {
    // If a global shortcut (capture-phase) already handled this event,
    // don't forward it to the Wayland surface.
    if (e.defaultPrevented) return;
    // Always prevent default so keystrokes never leak to the terminal
    // textarea, even when the surface info hasn't arrived yet.
    e.preventDefault();
    const conn = this.getConn();
    if (!conn || !this.surface) return;

    // On keydown, sync CapsLock if the browser state drifted from the
    // compositor's (e.g. the user toggled CapsLock while a different
    // element or window was focused).
    if (pressed) {
      this.syncCapsLock(e, conn);
    }

    const keycode = domKeyToEvdev(e.code);
    if (keycode !== 0) {
      if (pressed) {
        this.pressedKeys.add(keycode);
      } else {
        this.pressedKeys.delete(keycode);
      }
      conn.sendSurfaceInput(
        this.surface.sessionId,
        this._surfaceId,
        keycode,
        pressed,
      );
    }
  }

  /** Send synthetic key-up for every key still held.  Prevents stuck
   *  modifiers and runaway key-repeat when focus leaves the canvas. */
  private releaseAllKeys(): void {
    if (this.pressedKeys.size === 0) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    for (const kc of this.pressedKeys) {
      conn.sendSurfaceInput(this.surface.sessionId, this._surfaceId, kc, false);
    }
    this.pressedKeys.clear();
  }

  private handleBlur(): void {
    this.releaseAllKeys();
  }

  /**
   * Ensure the compositor's CapsLock state matches the browser before the
   * current key event is forwarded.
   *
   * The browser's `getModifierState("CapsLock")` always reflects the OS
   * state, but the compositor only sees key events forwarded through
   * `handleKey`.  If CapsLock was toggled while the surface was unfocused,
   * the compositor's XKB state drifts.  We detect the mismatch and inject
   * a synthetic CapsLock press+release to bring it back in sync.
   *
   * For a regular key (not CapsLock itself) the rule is simple: if the
   * browser and compositor disagree, inject a toggle.
   *
   * When the key IS CapsLock, `getModifierState` already shows the
   * *post-toggle* value.  The compositor will also toggle when it receives
   * our forwarded keydown.  For the end state to match we need the
   * compositor's *pre-toggle* state to be the opposite of the browser's
   * post-toggle value, i.e. `compositorCaps === !browserCaps`.  If that
   * doesn't hold we inject an extra toggle first so the real key lands
   * correctly.
   */
  private syncCapsLock(e: KeyboardEvent, conn: BlitConnection): void {
    const browserCaps = e.getModifierState("CapsLock");
    const compositorCaps = _compositorCapsLock.get(this._connectionId) ?? false;

    let needsSync: boolean;
    if (e.code === "CapsLock") {
      // Browser shows post-toggle.  Compositor will toggle on our forwarded
      // keydown.  We need compositorCaps === !browserCaps for the toggle to
      // land at browserCaps.  If not, inject a corrective toggle first.
      needsSync = compositorCaps === browserCaps;
    } else {
      needsSync = compositorCaps !== browserCaps;
    }

    if (needsSync) {
      const kc = EVDEV_MAP.CapsLock; // 58
      conn.sendSurfaceInput(this.surface!.sessionId, this._surfaceId, kc, true);
      conn.sendSurfaceInput(
        this.surface!.sessionId,
        this._surfaceId,
        kc,
        false,
      );
    }

    // Update tracking to the expected compositor state after this event.
    if (e.code === "CapsLock") {
      // Compositor will toggle (possibly twice if synthetic was sent).
      // Either way it ends at browserCaps.
      _compositorCapsLock.set(this._connectionId, browserCaps);
    } else if (needsSync) {
      _compositorCapsLock.set(this._connectionId, !compositorCaps);
    }
  }

  private handleFocus(): void {
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    conn.sendSurfaceFocus(this.surface.sessionId, this._surfaceId);
  }
}
