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
   * When non-null the surface is in resizable mode: the framework binding's
   * ResizeObserver calls setDisplaySize with the container's physical pixel
   * size and a server-side resize is requested.  The canvas backing buffer
   * always mirrors the decoded frame; CSS (width:100%/height:100% +
   * object-fit: contain) scales it to fill the container.  Keeping the
   * canvas at the frame's native size avoids a blurry "jump" mid-drag
   * where an old, smaller frame would get drawImage-upscaled into a
   * prematurely enlarged canvas before the new keyframe arrives.
   */
  private _displaySize: { width: number; height: number } | null = null;

  // subscriptions
  private unsubFrame: (() => void) | null = null;
  private unsubCursor: (() => void) | null = null;
  private unsubChange: (() => void) | null = null;

  /** True after the first frame has been blitted.  Kept as a tripwire so
   *  resubscribe paths can restart the first-frame fast path. */
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
  /** The non-modifier key that Meta→Ctrl translated alongside (e.g. V for
   *  Cmd+V).  Used to keep Ctrl held on the Wayland side until this key
   *  is released, so releasing Cmd early doesn't leave a bare V press
   *  that the app interprets as plain 'v' via client-side keyrepeat. */
  private _metaToCtrlKey = 0;
  /** Ctrl release is waiting for the paste-chord key to be released. */
  private _ctrlReleaseDeferred = false;
  /** In-flight Ctrl+V/Cmd+V state.  We defer the V press until the
   *  clipboard read completes (readText resolve, paste event, or
   *  timeout) so the Wayland app sees `selection` before `key` — and
   *  defer the V release and Ctrl release that may fire physically
   *  during that window, otherwise V arrives at the compositor with
   *  Ctrl already released and the app types 'v' repeatedly. */
  private _pendingPaste: {
    keycode: number;
    released: boolean;
    deferredCtrlRelease: boolean;
  } | null = null;
  private _pendingPasteFlush: ((text: string | null) => void) | null = null;

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
  private boundPaste: ((e: ClipboardEvent) => void) | null = null;
  private boundDocumentPaste: ((e: ClipboardEvent) => void) | null = null;

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
    canvas.style.objectFit = "contain";
    canvas.width = this.surface?.width || 640;
    canvas.height = this.surface?.height || 480;
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
      return;
    }
    const w = Math.round(width);
    const h = Math.round(height!);
    if (w <= 0 || h <= 0) return;
    this._displaySize = { width: w, height: h };
    // Canvas backing buffer is intentionally NOT resized here.  It tracks
    // the decoded frame size (set in blitFromStore) so the last sharp
    // frame stays sharp while CSS (object-fit: contain) scales it to the
    // new container size.  Resizing the canvas pre-emptively would clear
    // the backing buffer and force a drawImage upscale of the stale
    // frame, producing a visible "blurry intermediate" step until the
    // server's keyframe at the requested size arrives.
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
        // Size the canvas backing buffer to the surface when info first
        // arrives so the canvas has sensible intrinsic dimensions before
        // any frame has been decoded.  blitFromStore will re-snap it to
        // the actual frame size on first paint.
        if (!prev && this.canvas) {
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
      // Paint synchronously: the SurfaceStore presenter already fires this
      // listener from inside its own rAF (at most once per vsync), so a
      // second rAF layer here just adds another vsync of visible latency
      // without any coalescing benefit.
      if (!this._hasBlitFirstFrame) this._hasBlitFirstFrame = true;
      this.blitFromStore(store);
    });
  }

  private unsubscribeAll(): void {
    this.unsubFrame?.();
    this.unsubChange?.();
    this.unsubCursor?.();
    this.unsubFrame = null;
    this.unsubChange = null;
    this.unsubCursor = null;
  }

  /** Copy the shared backing canvas onto our visible canvas. */
  private blitFromStore(store: import("./SurfaceStore").SurfaceStore): void {
    const src = store.getCanvas(this._surfaceId);
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!src || !canvas || !ctx) return;
    if (src.width === 0 || src.height === 0) return;

    // Canvas backing buffer mirrors the source frame exactly.  CSS
    // (width:100%/height:100% + object-fit: contain) scales to the
    // container and handles letterboxing, so no drawImage upscale.
    if (canvas.width !== src.width || canvas.height !== src.height) {
      canvas.width = src.width;
      canvas.height = src.height;
    }
    ctx.drawImage(src, 0, 0);
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
    this.boundPaste = (e) => this.handlePaste(e);
    // Some browsers don't dispatch `paste` to a focused non-editable
    // canvas; a document-level capture listener picks those up.  Only
    // act while we have a paste shortcut in flight so we don't
    // interfere with other elements.
    this.boundDocumentPaste = (e) => {
      if (this._pendingPasteFlush) this.handlePaste(e);
    };

    canvas.addEventListener("mousedown", this.boundMouseDown);
    canvas.addEventListener("mouseup", this.boundMouseUp);
    canvas.addEventListener("mousemove", this.boundMouseMove);
    canvas.addEventListener("wheel", this.boundWheel, { passive: false });
    canvas.addEventListener("keydown", this.boundKeyDown);
    canvas.addEventListener("keyup", this.boundKeyUp);
    canvas.addEventListener("focus", this.boundFocus);
    canvas.addEventListener("blur", this.boundBlur);
    canvas.addEventListener("contextmenu", this.boundContextMenu);
    canvas.addEventListener("paste", this.boundPaste);
    document.addEventListener("paste", this.boundDocumentPaste, true);

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
      // Paste into the textarea would otherwise insert text that the
      // `input` handler forwards as surface text — intercept it so the
      // content goes through the Wayland clipboard path instead.
      if (this.boundPaste) ta.addEventListener("paste", this.boundPaste);
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
    if (this.boundPaste) canvas.removeEventListener("paste", this.boundPaste);
    if (this.boundDocumentPaste)
      document.removeEventListener("paste", this.boundDocumentPaste, true);

    const ta = this.textInput;
    if (ta) {
      if (this.boundTextInput)
        ta.removeEventListener("input", this.boundTextInput);
      if (this.boundCompositionEnd)
        ta.removeEventListener("compositionend", this.boundCompositionEnd);
      if (this.boundKeyDown)
        ta.removeEventListener("keydown", this.boundKeyDown);
      if (this.boundKeyUp) ta.removeEventListener("keyup", this.boundKeyUp);
      if (this.boundPaste) ta.removeEventListener("paste", this.boundPaste);
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
    const cw = this.canvas.width;
    const ch = this.canvas.height;
    if (cw === 0 || ch === 0 || rect.width === 0 || rect.height === 0) return;
    // The canvas's CSS box fills the container; its intrinsic aspect
    // (canvas.width/height === src frame) is letterboxed within via
    // object-fit: contain.  Compute the drawn content region in CSS
    // coordinates, then map a click into surface coordinates.
    const srcAR = cw / ch;
    const dstAR = rect.width / rect.height;
    let dw: number, dh: number, dx: number, dy: number;
    if (srcAR > dstAR) {
      dw = rect.width;
      dh = rect.width / srcAR;
      dx = 0;
      dy = (rect.height - dh) / 2;
    } else {
      dh = rect.height;
      dw = rect.height * srcAR;
      dx = (rect.width - dw) / 2;
      dy = 0;
    }
    const px = e.clientX - rect.left - dx;
    const py = e.clientY - rect.top - dy;
    const x = Math.round((px / dw) * this.surface.width);
    const y = Math.round((py / dh) * this.surface.height);
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

  // Fallback clipboard-read path for browsers/contexts where
  // `navigator.clipboard.readText()` is denied (Brave without granted
  // permission, Firefox, insecure contexts, ...).  The `paste` event
  // delivers clipboard data synchronously without a permission prompt.
  private handlePaste(e: ClipboardEvent): void {
    e.preventDefault();
    if (!this._displaySize) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (this._pendingPasteFlush) {
      const flush = this._pendingPasteFlush;
      this._pendingPasteFlush = null;
      flush(text || null);
    } else if (text) {
      conn.sendClipboard(
        "text/plain;charset=utf-8",
        new TextEncoder().encode(text),
      );
    }
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

    // Paste shortcut: skip preventDefault so the browser fires a `paste`
    // event on the focused element.  Our paste handler uses it as a
    // fallback when `navigator.clipboard.readText()` is denied (e.g.
    // Brave without granted clipboard permission).  `!e.repeat` keeps
    // OS autorepeat from re-triggering paste — native apps treat Cmd+V
    // as a one-shot action regardless of how long it's held.
    const isPasteShortcut =
      pressed &&
      !e.repeat &&
      (e.key === "v" || e.key === "V") &&
      (e.ctrlKey || e.metaKey) &&
      !e.altKey;
    if (!isPasteShortcut) e.preventDefault();
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
    // place when the app processes the paste shortcut.  The V press,
    // V release, and Ctrl release are all deferred until the clipboard
    // has been sent — otherwise the app can see Ctrl release (or V
    // release) before V press and interpret it as plain 'v' typing.
    if (isPasteShortcut) {
      const keycode = domKeyToEvdev(e.code);
      // Do NOT add keycode to pressedKeys yet — the flush below does it.
      this._pendingPaste = {
        keycode,
        released: false,
        deferredCtrlRelease: false,
      };

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
          this._metaToCtrlKey = keycode;
        }
      }

      const surfaceId = this._surfaceId;
      const enc = new TextEncoder();
      const flush = (clipboardText: string | null) => {
        const p = this._pendingPaste;
        if (!p || p.keycode !== keycode) return;
        this._pendingPaste = null;
        this._pendingPasteFlush = null;
        if (clipboardText) {
          conn.sendClipboard(
            "text/plain;charset=utf-8",
            enc.encode(clipboardText),
          );
        }
        if (keycode !== 0) {
          this.pressedKeys.add(keycode);
          conn.sendSurfaceInput(surfaceId, keycode, true);
          if (p.released) {
            this.pressedKeys.delete(keycode);
            conn.sendSurfaceInput(surfaceId, keycode, false);
          }
        }
        if (p.deferredCtrlRelease) {
          if (keycode !== 0 && !p.released) {
            // V is still physically held — defer Ctrl release until the
            // keyup V event arrives.  Releasing Ctrl now would leave a
            // bare V press on the Wayland side which the app would
            // interpret as plain 'v' typing via client-side keyrepeat.
            this._ctrlReleaseDeferred = true;
          } else {
            this.pressedKeys.delete(29);
            conn.sendSurfaceInput(surfaceId, 29, false);
            this._metaToCtrlKey = 0;
          }
        }
        // Restore focus to the canvas after the paste event processed on
        // the hidden textarea (see focus shuffle below).
        if (this.canvas && document.activeElement === this.textInput) {
          this.canvas.focus();
        }
      };
      this._pendingPasteFlush = flush;

      // Chromium/Brave don't reliably dispatch `paste` to a focused
      // non-editable canvas, and `navigator.clipboard.readText()` is
      // often denied without an explicit user-granted permission.  Move
      // focus to the hidden (editable) textarea so the browser's native
      // paste handling targets it — the paste event fires reliably
      // there with populated clipboardData.  handleBlur ignores the
      // transient blur via the `_pendingPaste` check above.
      if (this.textInput) this.textInput.focus();

      navigator.clipboard.readText().then(
        (text) => {
          // Only flush when readText actually returned content.  Some
          // browsers (Brave with sanitization) resolve with `""` instead
          // of rejecting — if we flushed on empty here, we'd close out
          // the pending paste and dispatch V with no clipboard update,
          // causing the Wayland app to paste its previous selection.
          if (text) flush(text);
        },
        () => {
          /* paste event will flush */
        },
      );
      // Safety net — if neither readText nor the paste event ever
      // delivers (both paths blocked), clean up the pending state and
      // undo the Meta→Ctrl translation.  Don't force V through without
      // clipboard data; pasting stale content is worse than doing
      // nothing.
      setTimeout(() => {
        const p = this._pendingPaste;
        if (!p || p.keycode !== keycode) return;
        this._pendingPaste = null;
        this._pendingPasteFlush = null;
        if (p.deferredCtrlRelease) {
          this.pressedKeys.delete(29);
          conn.sendSurfaceInput(surfaceId, 29, false);
          this._metaToCtrlKey = 0;
        }
        if (this.canvas && document.activeElement === this.textInput) {
          this.canvas.focus();
        }
      }, 300);
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
      // If the key is already pressed on the Wayland side (e.g. dispatched
      // via a paste-shortcut flush), skip the text path.  Otherwise, after
      // the user releases Cmd mid-hold, OS autorepeat keydowns of V arrive
      // with no modifier flags and get typed as literal 'v' characters.
      const kc = domKeyToEvdev(e.code);
      if (kc !== 0 && this.pressedKeys.has(kc)) return;
      conn.sendSurfaceText(this._surfaceId, e.key);
      return;
    }

    // Everything else (modifiers, arrows, F-keys, Ctrl/Alt/Meta combos):
    // send raw evdev keycode.
    const keycode = domKeyToEvdev(e.code);
    if (keycode !== 0) {
      // Paste in flight: defer V release and Ctrl release until the
      // clipboard has been sent and the V press dispatched.
      if (!pressed && this._pendingPaste) {
        if (keycode === this._pendingPaste.keycode) {
          this._pendingPaste.released = true;
          return;
        }
        if (keycode === this._metaToCtrl) {
          this._pendingPaste.deferredCtrlRelease = true;
          this._metaToCtrl = 0;
          return;
        }
        if (keycode === 29) {
          this._pendingPaste.deferredCtrlRelease = true;
          return;
        }
      }
      // Finish Meta→Ctrl translation: when the physical Meta key is
      // released after a translated Cmd+V paste, release Ctrl instead —
      // unless the chord's V is still held, in which case defer until V
      // is released so the app doesn't see a bare V and keyrepeat 'v'.
      if (!pressed && keycode === this._metaToCtrl) {
        if (
          this._metaToCtrlKey !== 0 &&
          this.pressedKeys.has(this._metaToCtrlKey)
        ) {
          this._ctrlReleaseDeferred = true;
          this._metaToCtrl = 0;
          return;
        }
        this.pressedKeys.delete(29); // ControlLeft
        conn.sendSurfaceInput(this._surfaceId, 29, false);
        this._metaToCtrl = 0;
        this._metaToCtrlKey = 0;
        return;
      }
      if (pressed) {
        this.pressedKeys.add(keycode);
      } else {
        // If the keydown was handled via the text path (sendSurfaceText),
        // the compositor already synthesized a full press+release cycle.
        // Sending another release here would be an orphaned event that
        // confuses Chromium-based clients (e.g. Space in YouTube toggling
        // play/pause twice).
        if (!this.pressedKeys.has(keycode)) return;
        this.pressedKeys.delete(keycode);
      }
      conn.sendSurfaceInput(this._surfaceId, keycode, pressed);
      // If this was the paste-chord key being released, flush any
      // deferred Ctrl release that was held back while V was still down.
      if (!pressed && keycode === this._metaToCtrlKey) {
        if (this._ctrlReleaseDeferred) {
          this._ctrlReleaseDeferred = false;
          this.pressedKeys.delete(29);
          conn.sendSurfaceInput(this._surfaceId, 29, false);
        }
        this._metaToCtrlKey = 0;
      }
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
    this._pendingPaste = null;
    this._pendingPasteFlush = null;
    this._ctrlReleaseDeferred = false;
    this._metaToCtrlKey = 0;
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
    // During an in-flight paste shortcut we may have temporarily moved
    // focus to the hidden textarea (so the browser dispatches the paste
    // event to an editable element).  Don't tear down key state — the
    // paste flush will refocus the canvas and cleanup naturally.
    if (this._pendingPaste) return;
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
        // Don't release the synthetic Ctrl from Meta→Ctrl paste
        // translation — either while the original Cmd is still held
        // (_metaToCtrl set) or while V is held with Ctrl release pending.
        if ((this._metaToCtrl || this._ctrlReleaseDeferred) && kc === 29)
          continue;
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
