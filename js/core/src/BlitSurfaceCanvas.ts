import type { ConnectionId, BlitSurface } from "./types";
import {
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H264_444,
  CODEC_SUPPORT_AV1_444,
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

// Minimal 64×64 4:4:4 test frames for real-decode probing.
// isConfigSupported() is unreliable for 4:4:4 — e.g. Chromium reports AV1
// Professional Profile as supported but dav1d chokes on actual 4:4:4 OBUs.
// prettier-ignore
const AV1_444_TEST_FRAME = new Uint8Array([
  0x12, 0x00, 0x0a, 0x0d, 0x20, 0x00, 0x00, 0xf9, 0x57, 0xff, 0xc4, 0x21,
  0x52, 0x04, 0x04, 0x04, 0xa0, 0x32, 0x29, 0x10, 0x02, 0x89, 0x1d, 0xa9,
  0x9d, 0x8f, 0x81, 0x60, 0x00, 0x10, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00,
  0x00, 0x00, 0x00, 0x30, 0xc3, 0x0c, 0x10, 0x41, 0x10, 0xbb, 0x11, 0x0e,
  0xc2, 0xb1, 0x4f, 0x18, 0x9e, 0x95, 0x58, 0xe7, 0x95, 0xb8, 0x14, 0x93,
]);
// prettier-ignore
const H264_444_TEST_FRAME = new Uint8Array([
  0x00, 0x00, 0x00, 0x01, 0x67, 0xf4, 0x00, 0x1f, 0x91, 0x9b, 0x28, 0x84,
  0xd8, 0x08, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x19, 0x07,
  0x8c, 0x18, 0xcb, 0x00, 0x00, 0x00, 0x01, 0x68, 0xeb, 0xe3, 0xc4, 0x48,
  0x44, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, 0x2b, 0xff, 0xfe, 0xf5,
  0xdb, 0xf3, 0x2c, 0x93, 0x97, 0x37, 0xc0, 0xa5, 0x92, 0x31, 0xf0, 0x29,
  0xa0, 0xb6, 0xbf, 0xff, 0xc1, 0xed, 0x94, 0x6c, 0x08, 0x03, 0x84, 0x16,
  0xdf, 0x31,
]);

/**
 * Try to actually decode a 4:4:4 test frame.  Returns true only if the
 * decoder produces a frame without error.
 */
async function tryDecode444(
  codec: string,
  testFrame: Uint8Array,
  codedWidth: number,
  codedHeight: number,
): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    let settled = false;
    const settle = (v: boolean) => {
      if (!settled) {
        settled = true;
        resolve(v);
      }
    };
    try {
      const decoder = new VideoDecoder({
        output: (frame) => {
          frame.close();
          decoder.close();
          settle(true);
        },
        error: () => {
          try {
            decoder.close();
          } catch {
            /* already closed */
          }
          settle(false);
        },
      });
      decoder.configure({ codec, codedWidth, codedHeight });
      decoder.decode(
        new EncodedVideoChunk({
          type: "key",
          timestamp: 0,
          data: testFrame,
        }),
      );
      decoder.flush().then(
        () => {
          try {
            decoder.close();
          } catch {
            /* already closed */
          }
          settle(settled ? true : false);
        },
        () => settle(false),
      );
      setTimeout(() => settle(false), 2000);
    } catch {
      settle(false);
    }
  });
}

/**
 * Probe which video codecs the browser can decode via WebCodecs and return
 * a bitmask of CODEC_SUPPORT_* flags.  Result is cached after first call.
 *
 * Basic codec support (H.264, AV1) is checked via isConfigSupported().
 * 4:4:4 chroma variants are verified by actually decoding a small test
 * frame, since isConfigSupported() is unreliable for subsampling modes.
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

  // 4:4:4 probes: actually decode a test frame (isConfigSupported lies).
  const decode444Checks: [string, Uint8Array, number][] = [
    ["avc1.F4001f", H264_444_TEST_FRAME, CODEC_SUPPORT_H264_444],
    ["av01.2.01M.08", AV1_444_TEST_FRAME, CODEC_SUPPORT_AV1_444],
  ];
  await Promise.all(
    decode444Checks.map(async ([codec, frame, bit]) => {
      if (await tryDecode444(codec, frame, 64, 64)) {
        mask |= bit;
      }
    }),
  );

  _codecSupport = mask;
  console.log(
    `[blit] codec support: 0x${mask.toString(16).padStart(2, "0")} ` +
      `(h264=${!!(mask & CODEC_SUPPORT_H264)} av1=${!!(mask & CODEC_SUPPORT_AV1)} ` +
      `h264-444=${!!(mask & CODEC_SUPPORT_H264_444)} av1-444=${!!(mask & CODEC_SUPPORT_AV1_444)})`,
  );
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
  private unsubCursor: (() => void) | null = null;
  private unsubChange: (() => void) | null = null;

  /** Dirty flag for rAF-coalesced blits — avoids redundant drawImage calls
   *  when multiple frames decode between display refreshes. */
  private _blitDirty = false;
  private _blitRafId: number | null = null;
  /** True after the first frame has been blitted.  The very first decoded
   *  frame is painted synchronously (bypassing rAF) to minimise
   *  time-to-first-paint on remote connections. */
  private _hasBlitFirstFrame = false;
  /** Cached store reference so we can keep the frame listener alive
   *  even when the connection is temporarily unavailable. */
  private _store: import("./SurfaceStore").SurfaceStore | null = null;
  private _retryUnsub: (() => void) | undefined;

  /** The SurfaceStore generation at the time we last sent a subscribe.
   *  Used to detect reconnects (generation bumps on disconnect) so we
   *  re-subscribe even when the surfaceId hasn't changed. */
  private _subscribedGeneration = -1;

  /** Hidden textarea used to capture IME composition.  Focus stays on
   *  the canvas for normal typing; the textarea only receives focus when
   *  an IME composition session is active. */
  private textInput: HTMLTextAreaElement | null = null;
  /** True while an IME composition session is active (focus is on textarea). */
  private _isComposing = false;
  /** Non-zero when a Meta→Ctrl translation is in flight (stores the Meta
   *  evdev keycode that was swapped so the release can be translated back). */
  private _metaToCtrl = 0;

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
  private boundTextInput: ((e: Event) => void) | null = null;
  private boundCompositionStart: ((e: Event) => void) | null = null;
  private boundCompositionEnd: ((e: CompositionEvent) => void) | null = null;

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
    // Hidden textarea for capturing IME composition and properly-shifted
    // characters.  Positioned behind the canvas so it doesn't interfere
    // with rendering but still receives focus and keyboard events.
    const ta = document.createElement("textarea");
    ta.autocomplete = "off";
    ta.setAttribute("autocorrect", "off");
    ta.setAttribute("autocapitalize", "off");
    ta.setAttribute("spellcheck", "false");
    ta.tabIndex = -1;
    ta.style.position = "absolute";
    ta.style.left = "0";
    ta.style.top = "0";
    ta.style.width = "1px";
    ta.style.height = "1px";
    ta.style.opacity = "0";
    ta.style.padding = "0";
    ta.style.border = "none";
    ta.style.outline = "none";
    ta.style.resize = "none";
    ta.style.overflow = "hidden";
    ta.style.pointerEvents = "none";
    ta.style.zIndex = "-1";
    // Ensure the container is a positioning context for the textarea.
    if (getComputedStyle(container).position === "static") {
      container.style.position = "relative";
    }
    container.appendChild(ta);
    this.textInput = ta;

    container.appendChild(canvas);

    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");

    this.subscribe();
    this.attachEvents();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this._retryUnsub) {
      this._retryUnsub();
      this._retryUnsub = undefined;
    }
    this.releaseAllKeys();
    this.releaseAllButtons();
    this.serverUnsubscribe();
    this.detachEvents();
    this.unsubscribeAll();
    if (this.textInput && this.container) {
      this.container.removeChild(this.textInput);
    }
    this.textInput = null;
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
    this.resendDisplaySize();
  }

  setSurfaceId(surfaceId: number): void {
    if (this._surfaceId === surfaceId) return;
    this._surfaceId = surfaceId;
    this.resubscribe();
    this.resendDisplaySize();
  }

  /**
   * Request the server to resize the surface to the given pixel dimensions.
   * The server will respond with a SURFACE_RESIZED message that updates the
   * surface metadata and canvas size via the normal onChange path.
   */
  requestResize(width: number, height: number, scale120: number = 0): void {
    const w = Math.round(width);
    const h = Math.round(height);
    if (w <= 0 || h <= 0) return;
    // Stash the pending resize so it can be sent when the surface info
    // arrives (the ResizeObserver may fire before the surface is known).
    this._pendingResize = { w, h, scale120 };
    this.flushPendingResize();
  }

  private _pendingResize: {
    w: number;
    h: number;
    scale120: number;
  } | null = null;

  private flushPendingResize(): void {
    if (!this._pendingResize) return;
    const conn = this.getConn();
    if (!conn || !this.surface) {
      return;
    }
    const { w, h, scale120 } = this._pendingResize;
    this._pendingResize = null;
    conn.sendSurfaceResize(this._surfaceId, w, h, scale120);
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

  /**
   * Re-queue the current display size as a pending resize so it is sent to
   * the server for the (possibly new) surface.  Analogous to how
   * {@link BlitTerminalSurface} re-sends dimensions in
   * `setupResizeObserver()` after a session change — the ResizeObserver
   * only fires when the container's pixel dimensions change, but after a
   * surfaceId/connectionId swap the server needs to learn the size for the
   * new surface even if the container stayed the same size.
   */
  private resendDisplaySize(): void {
    if (!this._displaySize) return;
    const { width, height } = this._displaySize;
    const scale120 =
      typeof devicePixelRatio === "number"
        ? Math.round(devicePixelRatio * 120)
        : 0;
    this._pendingResize = { w: width, h: height, scale120 };
    this.flushPendingResize();
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
    const store = conn?.surfaceStore ?? this._store;

    if (!store) {
      // Connection not ready yet — retry when workspace state changes.
      if (this._workspace && !this._retryUnsub) {
        this._retryUnsub = (this._workspace as any).subscribe(() => {
          if (this.disposed) {
            this._retryUnsub?.();
            this._retryUnsub = undefined;
            return;
          }
          const c = this.getConn();
          if (c) {
            this._retryUnsub?.();
            this._retryUnsub = undefined;
            this.subscribe();
          }
        });
      }
      return;
    }
    // Clear retry listener if it was set.
    if (this._retryUnsub) {
      this._retryUnsub();
      this._retryUnsub = undefined;
    }
    this._store = store;

    this.surface = store.getSurface(this._surfaceId);

    // Tell the server we want frames for this surface.  Subscribe eagerly
    // even when the surface metadata hasn't arrived yet (this.surface may
    // be undefined) — the server already knows the surface and can start
    // encoding as soon as it sees our subscribe.  Waiting for
    // S2C_SURFACE_CREATED to be processed before subscribing adds a
    // needless round-trip to time-to-first-frame.
    //
    // Only gate on canDecodeVideo: subscribing when WebCodecs is
    // unavailable (non-secure context) drives the server encoder for
    // nothing and can crash it.
    if (conn && store.canDecodeVideo) {
      conn.sendSurfaceSubscribe(this._surfaceId);
      this._subscribedGeneration = store.generation;
    }

    // Flush any pending resize and paint the latest frame immediately
    // so newly-mounted views aren't blank.
    this.flushPendingResize();
    this.blitFromStore(store);

    this.unsubChange = store.onChange(() => {
      const prev = this.surface;
      this.surface = store.getSurface(this._surfaceId);
      // Re-subscribe when the store generation changed (reconnect — the
      // server dropped all subscriptions but the surface reappeared with
      // the same IDs).  We no longer need to handle the "surface info
      // just arrived" case here because subscribe() above sends the
      // subscribe eagerly before the surface metadata is available.
      if (this.surface && store.canDecodeVideo) {
        if (this._subscribedGeneration !== store.generation) {
          const c = this.getConn();
          if (c) {
            // Refresh on reconnect — don't bump the ref-count, we
            // already own a ref from the initial subscribe() call.
            c.refreshSurfaceSubscribe(this._surfaceId);
            this._subscribedGeneration = store.generation;
          }
        }
        // Update canvas size when surface info first arrives,
        // unless the display size is pinned by a ResizeObserver.
        if (!prev && this.canvas && !this._displaySize) {
          this.canvas.width = this.surface.width;
          this.canvas.height = this.surface.height;
        }
      }
      // Flush any pending resize now that we have the surface info.
      this.flushPendingResize();
      // Repaint on any surface change (e.g. resize, new frame decoded
      // while listener was briefly detached).
      this.blitFromStore(store);
    });

    // Frame listener — must always be registered so decoded frames are
    // painted to the visible canvas regardless of connection state.
    // Apply cursor changes from the compositor.
    this.unsubCursor = store.onCursor((sid, shape) => {
      if (sid !== this._surfaceId || !this.canvas) return;
      this.canvas.style.cursor = shape;
    });
    // Apply initial cursor.
    if (this.canvas) {
      this.canvas.style.cursor = store.getCursor(this._surfaceId);
    }

    this.unsubFrame = store.onFrame((sid) => {
      if (sid !== this._surfaceId) return;
      // Paint the very first decoded frame synchronously to minimise
      // time-to-first-paint.  Subsequent frames are coalesced via rAF
      // so we don't drawImage more than once per display refresh.
      if (!this._hasBlitFirstFrame) {
        this._hasBlitFirstFrame = true;
        this.blitFromStore(store);
        return;
      }
      if (!this._blitDirty) {
        this._blitDirty = true;
        this._blitRafId = requestAnimationFrame(() => {
          this._blitRafId = null;
          this._blitDirty = false;
          this.blitFromStore(store);
        });
      }
    });
  }

  private unsubscribeAll(): void {
    this.unsubFrame?.();
    this.unsubChange?.();
    this.unsubCursor?.();
    this.unsubFrame = null;
    this.unsubChange = null;
    this.unsubCursor = null;
    if (this._blitRafId !== null) {
      cancelAnimationFrame(this._blitRafId);
      this._blitRafId = null;
    }
    this._blitDirty = false;
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
      // physical pixel size.  Draw the source frame scaled to fit,
      // preserving aspect ratio (letterbox/pillarbox).
      const srcAR = src.width / src.height;
      const dstAR = canvas.width / canvas.height;
      let dw: number, dh: number, dx: number, dy: number;
      if (srcAR > dstAR) {
        // Source is wider — pillarbox (bars top/bottom)
        dw = canvas.width;
        dh = canvas.width / srcAR;
        dx = 0;
        dy = (canvas.height - dh) / 2;
      } else {
        // Source is taller — letterbox (bars left/right)
        dh = canvas.height;
        dw = canvas.height * srcAR;
        dx = (canvas.width - dw) / 2;
        dy = 0;
      }
      ctx.clearRect(0, 0, canvas.width, canvas.height);
      ctx.drawImage(src, dx, dy, dw, dh);
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
    this.unsubscribeAll();
    this._hasBlitFirstFrame = false;
    if (!this.disposed) this.subscribe();
  }

  private serverUnsubscribe(): void {
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    conn.sendSurfaceUnsubscribe(this._surfaceId);
  }

  // -----------------------------------------------------------------------
  // Event handling
  // -----------------------------------------------------------------------

  private attachEvents(): void {
    const canvas = this.canvas;
    const ta = this.textInput;
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

    // Hidden textarea is only used for IME composition.  Focus stays on
    // the canvas during normal typing; we redirect to the textarea when
    // a composition session starts (detected via compositionstart on the
    // canvas) and return focus to the canvas when it ends.
    if (ta) {
      this.boundTextInput = (e) => this.handleTextInput(e as InputEvent);
      this.boundCompositionEnd = (e) => this.handleCompositionEnd(e);

      ta.addEventListener("input", this.boundTextInput);
      ta.addEventListener("compositionend", this.boundCompositionEnd);
      // Also listen for keydown on textarea so keys during IME composition
      // (e.g. Enter to confirm, Escape to cancel) still get routed.
      ta.addEventListener("keydown", this.boundKeyDown);
      ta.addEventListener("keyup", this.boundKeyUp);
    }

    // Detect IME composition start on the canvas and redirect focus
    // to the textarea so the browser's IME UI can work.
    this.boundCompositionStart = () => {
      this._isComposing = true;
      if (this.textInput) this.textInput.focus();
    };
    canvas.addEventListener("compositionstart", this.boundCompositionStart);
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
    if (this.boundCompositionStart)
      canvas.removeEventListener(
        "compositionstart",
        this.boundCompositionStart,
      );

    const ta = this.textInput;
    if (ta) {
      if (this.boundTextInput)
        ta.removeEventListener("input", this.boundTextInput);
      if (this.boundCompositionEnd)
        ta.removeEventListener("compositionend", this.boundCompositionEnd);
      if (this.boundKeyDown)
        ta.removeEventListener("keydown", this.boundKeyDown);
      if (this.boundKeyUp) ta.removeEventListener("keyup", this.boundKeyUp);
    }
  }

  private handleMouse(e: MouseEvent, type: number): void {
    const conn = this.getConn();
    if (!conn || !this.canvas || !this.surface || !this._displaySize) return;
    if (type === SURFACE_POINTER_DOWN) {
      this.canvas.focus();
      this.pressedButtons.add(e.button);
    } else if (type === SURFACE_POINTER_UP) {
      this.pressedButtons.delete(e.button);
    }
    const rect = this.canvas.getBoundingClientRect();
    // Convert CSS-pixel coordinates to canvas (physical) pixel coordinates.
    const canvasX = (e.clientX - rect.left) * (this.canvas.width / rect.width);
    const canvasY = (e.clientY - rect.top) * (this.canvas.height / rect.height);
    // Compute the letterbox/pillarbox geometry (same as blitFromStore) so
    // we map through the actual drawn region, not the full canvas.
    const srcAR = this.surface.width / this.surface.height;
    const dstAR = this.canvas.width / this.canvas.height;
    let dw: number, dh: number, dx: number, dy: number;
    if (srcAR > dstAR) {
      dw = this.canvas.width;
      dh = this.canvas.width / srcAR;
      dx = 0;
      dy = (this.canvas.height - dh) / 2;
    } else {
      dh = this.canvas.height;
      dw = this.canvas.height * srcAR;
      dx = (this.canvas.width - dw) / 2;
      dy = 0;
    }
    const x = Math.round(((canvasX - dx) / dw) * this.surface.width);
    const y = Math.round(((canvasY - dy) / dh) * this.surface.height);
    conn.sendSurfacePointer(this._surfaceId, type, e.button, x, y);
  }

  /** Send synthetic pointer-up for any buttons still held.  Prevents the
   *  compositor's implicit pointer grab from outliving this canvas. */
  private releaseAllButtons(): void {
    if (this.pressedButtons.size === 0) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    for (const button of this.pressedButtons) {
      conn.sendSurfacePointer(
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
    if (!conn || !this.surface || !this._displaySize) return;
    e.preventDefault();
    const axis = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? 1 : 0;
    const value = axis === 0 ? e.deltaY : e.deltaX;
    conn.sendSurfaceAxis(this._surfaceId, axis, Math.round(value * 100));
  }

  private handleKey(e: KeyboardEvent, pressed: boolean): void {
    // If a global shortcut (capture-phase) already handled this event,
    // don't forward it to the Wayland surface.
    if (e.defaultPrevented) return;
    // Only forward input when interactive (resizable/focused mode).
    // Sidebar previews should not intercept keyboard or send events.
    if (!this._displaySize) return;

    // Dead keys / ongoing IME composition: redirect focus to the hidden
    // textarea so the browser's composition UI can work.  The textarea's
    // compositionend handler sends the result and returns focus here.
    if (pressed && (e.key === "Dead" || e.isComposing)) {
      if (this.textInput) {
        this._isComposing = true;
        this.textInput.focus();
      }
      return;
    }

    e.preventDefault();
    const conn = this.getConn();
    if (!conn || !this.surface) return;

    // On keydown, reconcile modifier state with the browser before
    // forwarding the key.  Window managers may intercept modifier keys
    // (especially Super/Meta) without delivering the key-up to the
    // browser, leaving pressedKeys and the compositor's mods_depressed
    // out of sync.
    if (pressed) {
      this.syncModifiers(e, conn);
      this.syncCapsLock(e, conn);
    }

    // Paste: read the browser clipboard and offer it to the Wayland
    // compositor *before* forwarding the key, so the data offer is in
    // place when the app processes the paste shortcut.
    if (
      pressed &&
      (e.key === "v" || e.key === "V") &&
      (e.ctrlKey || e.metaKey) &&
      !e.altKey
    ) {
      const keycode = domKeyToEvdev(e.code);
      if (keycode !== 0) this.pressedKeys.add(keycode);

      // On macOS, Cmd+V arrives with metaKey set.  Wayland apps expect
      // Ctrl+V, so swap the already-pressed Meta → Ctrl before forwarding
      // the key.  The reverse swap happens on Meta key-up (see below).
      if (e.metaKey && !e.ctrlKey) {
        const metaCode = this.pressedKeys.has(125)
          ? 125
          : this.pressedKeys.has(126)
            ? 126
            : 0;
        if (metaCode !== 0) {
          this.pressedKeys.delete(metaCode);
          conn.sendSurfaceInput(this._surfaceId, metaCode, false);
          this.pressedKeys.add(29); // ControlLeft
          conn.sendSurfaceInput(this._surfaceId, 29, true);
          this._metaToCtrl = metaCode;
        }
      }

      const surfaceId = this._surfaceId;
      navigator.clipboard.readText().then(
        (text) => {
          if (text) {
            const enc = new TextEncoder();
            conn.sendClipboard("text/plain;charset=utf-8", enc.encode(text));
          }
          if (keycode !== 0) conn.sendSurfaceInput(surfaceId, keycode, true);
        },
        () => {
          // Clipboard read failed (no permission, empty, etc.) —
          // forward the key anyway so the shortcut still reaches the app.
          if (keycode !== 0) conn.sendSurfaceInput(surfaceId, keycode, true);
        },
      );
      return;
    }

    // Printable character (no Ctrl/Alt/Meta): send the browser-resolved
    // character via the text path.  This handles keyboard layout
    // differences (e.g. Shift+2 → @ on US, " on UK) without depending
    // on the compositor's US-QWERTY keymap.
    if (
      pressed &&
      !e.ctrlKey &&
      !e.altKey &&
      !e.metaKey &&
      e.key.length === 1
    ) {
      conn.sendSurfaceText(this._surfaceId, e.key);
      return;
    }

    // Everything else (modifiers, arrows, F-keys, Ctrl/Alt/Meta combos):
    // send raw evdev keycode.
    const keycode = domKeyToEvdev(e.code);
    if (keycode !== 0) {
      // Finish Meta→Ctrl translation: when the physical Meta key is
      // released after a translated Cmd+V paste, release Ctrl instead.
      if (!pressed && keycode === this._metaToCtrl) {
        this.pressedKeys.delete(29); // ControlLeft
        conn.sendSurfaceInput(this._surfaceId, 29, false);
        this._metaToCtrl = 0;
        return;
      }
      if (pressed) {
        this.pressedKeys.add(keycode);
      } else {
        this.pressedKeys.delete(keycode);
      }
      conn.sendSurfaceInput(this._surfaceId, keycode, pressed);
    }
  }

  /** Handle text input from the hidden textarea (IME only). */
  private handleTextInput(e: InputEvent): void {
    // During IME composition, wait for compositionend.
    if (e.isComposing) return;
    // Non-composition input events on the textarea can be ignored —
    // normal typing is handled via e.key in handleKey directly.
    const ta = this.textInput;
    if (ta) ta.value = "";
  }

  /** Handle IME composition end — send the composed text and return
   *  focus to the canvas. */
  private handleCompositionEnd(e: CompositionEvent): void {
    this._isComposing = false;
    const ta = this.textInput;
    if (!ta) return;
    if (e.data) {
      const conn = this.getConn();
      if (conn && this.surface) {
        conn.sendSurfaceText(this._surfaceId, e.data);
      }
    }
    ta.value = "";
    // Return focus to the canvas so subsequent keystrokes go through
    // the normal evdev / e.key path.
    if (this.canvas) this.canvas.focus();
  }

  /** Send synthetic key-up for every key still held.  Prevents stuck
   *  modifiers and runaway key-repeat when focus leaves the canvas. */
  private releaseAllKeys(): void {
    if (this.pressedKeys.size === 0) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    for (const kc of this.pressedKeys) {
      conn.sendSurfaceInput(this._surfaceId, kc, false);
    }
    this.pressedKeys.clear();
    this._metaToCtrl = 0;
  }

  private handleBlur(): void {
    this.releaseAllKeys();
  }

  /**
   * Release any modifier keys that the browser says are no longer held.
   *
   * Window managers (especially on Linux) may grab modifier keys like
   * Super/Meta without forwarding the key-up event to the browser.  When
   * that happens our `pressedKeys` set and the compositor's modifier
   * state drift from reality.  On every key-down we compare the browser's
   * authoritative modifier flags against `pressedKeys` and inject
   * synthetic releases for anything that should no longer be held.
   */
  private syncModifiers(e: KeyboardEvent, conn: BlitConnection): void {
    const checks: [boolean, number[]][] = [
      [e.shiftKey, [42, 54]], // ShiftLeft, ShiftRight
      [e.ctrlKey, [29, 97]], // ControlLeft, ControlRight
      [e.altKey, [56, 100]], // AltLeft, AltRight
      [e.metaKey, [125, 126]], // MetaLeft, MetaRight
    ];
    for (const [held, keycodes] of checks) {
      if (held) continue;
      for (const kc of keycodes) {
        if (!this.pressedKeys.has(kc)) continue;
        // Don't release the synthetic Ctrl from Meta→Ctrl paste translation.
        if (this._metaToCtrl && kc === 29) continue;
        this.pressedKeys.delete(kc);
        conn.sendSurfaceInput(this._surfaceId, kc, false);
      }
    }
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
      conn.sendSurfaceInput(this._surfaceId, kc, true);
      conn.sendSurfaceInput(this._surfaceId, kc, false);
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
    if (!conn || !this.surface || !this._displaySize) return;
    conn.sendSurfaceFocus(this._surfaceId);
  }
}
