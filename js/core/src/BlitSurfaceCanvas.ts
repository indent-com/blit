import type { ConnectionId, BlitSurface } from "./types";
import {
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H264_444,
  CODEC_SUPPORT_AV1_444,
  AXIS_SOURCE_FINGER,
  AXIS_SOURCE_WHEEL,
} from "./types";
import type { BlitWorkspace } from "./BlitWorkspace";
import type { BlitConnection } from "./BlitConnection";
import {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_UP,
  SURFACE_POINTER_MOVE,
} from "./protocol";
import {
  devicePixelBox,
  drawHalved,
  halve,
  halvings,
  octaveCeil,
} from "./downscale";

/** Cached codec support bitmask.  Computed once, reused for all resize messages. */
let _codecSupport: number | null = null;

/**
 * Largest frame any supported codec decoded in the probe, as [w, h].
 * `[0, 0]` = nothing above 1080p was confirmed, which the server reads as
 * "undeclared" and holds to the H.264 ceiling.
 */
let _maxDecode: [number, number] = [0, 0];

/**
 * Frame sizes to probe, largest first.  These are the ceilings the server
 * will actually encode to, so probing anything between them would tell us
 * nothing it could act on: the AV1 hardware ceiling, the 5K/6K panels that
 * motivated raising it, and the H.264 ceiling below which the answer stops
 * mattering.
 */
const DECODE_PROBE_SIZES: [number, number][] = [
  [8192, 4352],
  [6144, 3456],
  [5120, 2880],
  [3840, 2160],
];

/**
 * AV1 `seq_level_idx` for a frame of this size at 60 fps, as the two-digit
 * string a codec parameter wants.  Mirrors `av1_level_for()` on the server,
 * which decides what the bitstream actually declares — probing at a level
 * below what we would be sent would pass here and fail later, and probing
 * above it under-reports on decoders that gate on level.
 */
function av1LevelString(width: number, height: number): string {
  const sps = width * height * 60;
  // [level, maxW, maxH, maxDecodeRate]
  const specs: [string, number, number, number][] = [
    ["00", 2048, 1152, 5529600],
    ["01", 2816, 1152, 10454400],
    ["04", 4352, 2448, 24969600],
    ["05", 5504, 3096, 39938400],
    ["08", 6144, 3456, 77856768],
    ["09", 6144, 3456, 155713536],
    ["12", 8192, 4352, 273715200],
    ["13", 8192, 4352, 547430400],
    ["16", 16384, 8704, 1176502272],
  ];
  for (const [level, maxW, maxH, maxRate] of specs) {
    if (width <= maxW && height <= maxH && sps <= maxRate) return level;
  }
  return "16";
}

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
  //
  // AV1_444_TEST_FRAME is a seq_profile 1 bitstream (its sequence header
  // payload opens 0x20 = 001b), and 8-bit 4:4:4 is Profile 1 ("High") — the
  // codec string has to say 1, not 2.  Profile 2 ("Professional") is 4:2:2
  // at 8/10-bit and only reaches 4:4:4 at 12-bit, so declaring 2 handed the
  // decoder a profile the frame contradicts.  This must stay in step with
  // `av1_profile_digit()` on the server, which picks what we actually send.
  const decode444Checks: [string, Uint8Array, number][] = [
    ["avc1.F4001f", H264_444_TEST_FRAME, CODEC_SUPPORT_H264_444],
    ["av01.1.01M.08", AV1_444_TEST_FRAME, CODEC_SUPPORT_AV1_444],
  ];
  await Promise.all(
    decode444Checks.map(async ([codec, frame, bit]) => {
      if (await tryDecode444(codec, frame, 64, 64)) {
        mask |= bit;
      }
    }),
  );

  // How large a frame can we actually decode?  The checks above only asked
  // at 1080p, which says nothing about 4K or 5K — and the server will not
  // composite a surface above the H.264 ceiling until we answer.  Probe
  // each supported codec largest-first and report the best result: the
  // server intersects it with the ceiling of whichever encoder it actually
  // uses, so the maximum across codecs is the right thing to send.
  //
  // Only AV1 can exceed 3840x2160 server-side, so H.264 is probed at that
  // ceiling and no further.
  const sizesFor = (bit: number) =>
    bit === CODEC_SUPPORT_AV1
      ? DECODE_PROBE_SIZES
      : DECODE_PROBE_SIZES.filter(([w, h]) => w <= 3840 && h <= 2160);
  const perCodec = await Promise.all(
    ([CODEC_SUPPORT_H264, CODEC_SUPPORT_AV1] as const)
      .filter((bit) => mask & bit)
      .map(async (bit): Promise<[number, number]> => {
        for (const [w, h] of sizesFor(bit)) {
          const codec =
            bit === CODEC_SUPPORT_AV1
              ? `av01.0.${av1LevelString(w, h)}M.08`
              : "avc1.640034"; // High@5.2 — covers everything up to 4K
          try {
            const r = await VideoDecoder.isConfigSupported({
              codec,
              codedWidth: w,
              codedHeight: h,
            });
            if (r.supported) return [w, h];
          } catch {
            // treat as unsupported at this size and try the next one down
          }
        }
        return [0, 0];
      }),
  );
  // Reduce after the fact rather than writing from each probe: the two run
  // concurrently, and a smaller result landing last would under-report.
  _maxDecode = perCodec.reduce<[number, number]>(
    (best, got) => (got[0] * got[1] > best[0] * best[1] ? got : best),
    [0, 0],
  );

  _codecSupport = mask;
  console.log(
    `[blit] codec support: 0x${mask.toString(16).padStart(2, "0")} ` +
      `(h264=${!!(mask & CODEC_SUPPORT_H264)} av1=${!!(mask & CODEC_SUPPORT_AV1)} ` +
      `h264-444=${!!(mask & CODEC_SUPPORT_H264_444)} av1-444=${!!(mask & CODEC_SUPPORT_AV1_444)}) ` +
      `max decode: ${_maxDecode[0]}x${_maxDecode[1]}`,
  );
  return mask;
}

/** Return the cached codec support, or 0 if not yet probed. */
export function getCodecSupport(): number {
  return _codecSupport ?? 0;
}

/**
 * Largest frame the probe confirmed this browser can decode, as [w, h].
 * `[0, 0]` before probing, or when nothing above 1080p was confirmed.
 */
export function getMaxDecodeSize(): [number, number] {
  return _maxDecode;
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

// -- Scroll ----------------------------------------------------------------

const WHEEL_MODE_LINE = 1;
const WHEEL_MODE_PAGE = 2;
/** CSS pixels per line when a browser reports a wheel in line mode
 *  (Firefox does, for notched mice). Matches the default line box. */
const WHEEL_LINE_PX = 16;
/** Lines a wheel notch conventionally travels, so line-mode deltas can be
 *  turned back into `axis_value120` detents. */
const WHEEL_LINES_PER_DETENT = 3;
/** CSS pixels per detent for browsers that report notched wheels in pixel
 *  mode (Chrome and Edge on Windows and Linux). */
const WHEEL_DETENT_PX = 120;
/**
 * Idle gap that ends a scroll sequence.
 *
 * Long enough to bridge the frame cadence of a macOS momentum tail so one
 * flick stays one gesture, short enough that the app settles promptly
 * once the tail decays.
 *
 * Also deliberately past Chromium's `kFlingStartTimeoutMs` of 200ms: it
 * turns `axis_stop` into a fling whose velocity it regresses from the
 * last few frames, unless the gap since the last of them exceeds that.
 * macOS has already appended its own momentum by the time we see these
 * events, so a fling on top of it is a second helping — the page sails
 * past where the tail left it, and stopping with fingers still down
 * flings at the speed you were going before you stopped. Touch drags
 * still want kinetic scrolling and are unaffected: they end their
 * sequence on `touchend` rather than waiting out this timer.
 */
const SCROLL_STOP_MS = 280;

/** One clipboard representation on its way to the Wayland selection. */
type ClipboardPayload = { mime: string; data: Uint8Array };

/** Marks a paste event as already handled.  The canvas, the hidden textarea
 *  and the document-level capture listener are all on the path of the same
 *  event, and each of them would otherwise forward the selection again —
 *  which for a screenshot means putting megabytes on the wire twice. */
const PASTE_CLAIMED = Symbol("blit.pasteClaimed");

/** Wrap plain text in the MIME type Wayland apps expect for a selection. */
function textPayload(text: string): ClipboardPayload {
  return {
    mime: "text/plain;charset=utf-8",
    data: new TextEncoder().encode(text),
  };
}

/**
 * Largest clipboard payload we will put on the wire.
 *
 * The protocol's frame ceiling is 16 MiB and a `C2S_CLIPBOARD_SET` over it is
 * not truncated, it is refused — the connection reads a bad length and drops.
 * Screenshots are the common case and land far below this; anything above it
 * is not something a paste should risk the session on.
 */
const MAX_CLIPBOARD_BYTES = 8 * 1024 * 1024;

/** Image types to prefer when a clipboard carries several, most portable
 *  first.  `image/png` is what every toolkit asks for. */
const IMAGE_MIME_PREFERENCE = ["image/png", "image/webp", "image/jpeg"];

/** How long a paste chord waits for the clipboard before giving up on it.
 *  Both reads it waits on — `readText()` and the `paste` event — are answered
 *  from memory the browser already holds, so this only has to cover an IPC
 *  round trip; the V keypress is stalled for the whole of it. */
const PASTE_READ_MS = 300;
/** The same deadline once an image is known to be on the clipboard: the
 *  bytes still have to be read out of the blob, and a screenshot is megabytes
 *  where text was bytes. */
const PASTE_IMAGE_MS = 3000;

/**
 * The image a clipboard payload carries, if the image is what the paste means.
 *
 * Rich sources put several representations on the clipboard at once — a
 * spreadsheet range arrives as text *and* as a picture of itself — and the
 * text is what pasting is expected to produce. So an image only wins when
 * there is no plain text at all, which is exactly the screenshot and
 * copied-image case this exists for.
 *
 * `getAsFile()` has to run while the event is being dispatched; the `File` it
 * returns stays readable afterwards.
 */
function clipboardImage(dt: DataTransfer | null): File | null {
  if (!dt || dt.getData("text/plain")) return null;
  const items = dt.items;
  if (!items) return null;
  const images: File[] = [];
  for (let i = 0; i < items.length; i++) {
    const it = items[i];
    if (it.kind !== "file" || !it.type.startsWith("image/")) continue;
    const file = it.getAsFile();
    if (file) images.push(file);
  }
  if (images.length === 0) return null;
  for (const mime of IMAGE_MIME_PREFERENCE) {
    const match = images.find((f) => f.type === mime);
    if (match) return match;
  }
  return images[0];
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

  /** Active single-finger gesture used to emulate mouse input on iPadOS. */
  private activeTouch: {
    identifier: number;
    startX: number;
    startY: number;
    lastX: number;
    lastY: number;
    mode: "pending" | "scroll" | "drag";
    longPressTimer: ReturnType<typeof setTimeout> | null;
    pointerId?: number;
  } | null = null;

  /**
   * When non-null the surface is in resizable mode: the framework binding's
   * ResizeObserver calls setDisplaySize with the container's physical pixel
   * size and a server-side resize is requested.  The canvas backing buffer
   * always mirrors the decoded frame; applyLayout() sizes the CSS box so
   * one canvas pixel is one device pixel — never upscaled — and centers it
   * in the container.  Keeping the canvas at the frame's native size avoids
   * a blurry "jump" mid-drag where an old, smaller frame would get
   * drawImage-upscaled into a prematurely enlarged canvas before the new
   * keyframe arrives.
   */
  private _displaySize: {
    width: number;
    height: number;
    scale120: number;
  } | null = null;
  /**
   * The container's size in device pixels, tracked for every view.
   *
   * A resizable view gets its size through setDisplaySize and sits at 1:1, so
   * this is only consulted for the views that never learn a size — dock
   * thumbnails and the React binding — which otherwise hand a full-resolution
   * frame to a card-sized box and get a point-sampled minification back.
   * Presentation only: it is never sent to the server, so a thumbnail cannot
   * shrink the surface for the co-viewers watching it full size.
   */
  private _presentBox: { width: number; height: number } | null = null;
  private _presentObserver: ResizeObserver | null = null;
  /** This view's surface-subscription token.  Allocated lazily and kept
   *  across resubscribes so the connection tracks one entry per view. */
  private _surfaceViewId: string | null = null;
  /** Halvings applied by the last blit, so the observer can tell a resize that
   *  crosses an octave from one that changes nothing on screen. */
  private _presentHalvings = 0;
  /** Source frame size of the last blit, so the observer can recompute the
   *  reduction without going back to the store. */
  private _lastFrameSize: { width: number; height: number } | null = null;
  /** Last layout applied by applyLayout(), to skip redundant style writes. */
  private _lastLayout: {
    left: number;
    top: number;
    w: number;
    h: number;
  } | null = null;
  /** True after this view has sent a nonzero surface resize that must be
   *  cleared when the view stops owning foreground/BSP sizing. */
  private _resizeConstraintActive = false;

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
  /** The exact subscription this canvas owns.  Kept separate from current
   *  props so prop changes can unsubscribe the old surface correctly. */
  private _subscribedSurface: {
    connectionId: ConnectionId;
    surfaceId: number;
  } | null = null;

  /** Hidden textarea used to capture IME composition.  Focus stays on
   *  the canvas for normal typing; the textarea only receives focus when
   *  an IME composition session is active. */
  private textInput: HTMLTextAreaElement | null = null;
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
  private _pendingPasteFlush:
    | ((payload: ClipboardPayload | null) => void)
    | null = null;
  /** Safety-net timer for the in-flight paste, and the cleanup it runs.
   *  Kept as fields so reading a clipboard image — which is asynchronous,
   *  unlike `getData()` — can push the deadline back instead of losing the
   *  paste to it. */
  private _pendingPasteTimer: ReturnType<typeof setTimeout> | null = null;
  private _pendingPasteAbandon: (() => void) | null = null;

  // scroll batching; see queueScroll()
  private scrollAccum: {
    dx: number;
    dy: number;
    v120x: number;
    v120y: number;
  } | null = null;
  private scrollFlushHandle: number | null = null;
  private scrollStopTimer: ReturnType<typeof setTimeout> | null = null;
  /** Whether the in-flight sequence came from a smooth device. Latched,
   *  so a momentum tail cannot be reclassified as a wheel mid-gesture. */
  private scrollSmoothLatch = false;
  /** Whether a stop still owes the client. */
  private scrollSequenceOpen = false;

  // bound event handlers
  private boundMouseDown: ((e: MouseEvent) => void) | null = null;
  private boundMouseUp: ((e: MouseEvent) => void) | null = null;
  private boundMouseMove: ((e: MouseEvent) => void) | null = null;
  private boundWheel: ((e: WheelEvent) => void) | null = null;
  private boundTouchStart: ((e: TouchEvent) => void) | null = null;
  private boundTouchMove: ((e: TouchEvent) => void) | null = null;
  private boundTouchEnd: ((e: TouchEvent) => void) | null = null;
  private boundTouchCancel: ((e: TouchEvent) => void) | null = null;
  private boundPointerDown: ((e: PointerEvent) => void) | null = null;
  private boundPointerMove: ((e: PointerEvent) => void) | null = null;
  private boundPointerUp: ((e: PointerEvent) => void) | null = null;
  private boundPointerCancel: ((e: PointerEvent) => void) | null = null;
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
    // Let Blit handle iPad touch gestures itself instead of Safari turning
    // them into page panning/zooming while interacting with a surface.
    canvas.style.touchAction = "none";
    canvas.style.webkitUserSelect = "none";
    (
      canvas.style as CSSStyleDeclaration & { webkitTouchCallout?: string }
    ).webkitTouchCallout = "none";
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

    this.observePresentBox(container);
    this.subscribe();
    this.attachEvents();
  }

  /**
   * Watch the container so blitFromStore knows how far the browser is about
   * to shrink the canvas.  See {@link _presentBox}.
   */
  private observePresentBox(container: HTMLElement): void {
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver((entries) => {
      const entry = entries[entries.length - 1];
      const box = entry && devicePixelBox(entry);
      if (!box) return;
      this._presentBox = box;
      // Ask the server for a stream sized to the new box.  Quantised, so a
      // drag re-asks only on an octave boundary — each change costs an
      // encoder rebuild and a keyframe.
      this.refreshScaledTarget();
      // Redraw only when the box crosses an octave.  The reduction is
      // quantised, so most of a dock-grip drag lands on the same chain and
      // there is nothing new to show.
      const src = this._lastFrameSize;
      if (!src || this._displaySize) return;
      if (
        halvings(src.width, src.height, box.width, box.height) ===
        this._presentHalvings
      )
        return;
      const store = this.getConn()?.surfaceStore ?? this._store;
      if (store) this.blitFromStore(store);
    });
    observer.observe(container);
    this._presentObserver = observer;
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    if (this._retryUnsub) {
      this._retryUnsub();
      this._retryUnsub = undefined;
    }
    this._presentObserver?.disconnect();
    this._presentObserver = null;
    this.releaseAllKeys();
    this.releaseAllButtons();
    this.endScrollSequence();
    this.setDisplaySize(null);
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
    this.clearResizeConstraint();
    this._connectionId = connectionId;
    this.resubscribe();
    this.resendDisplaySize();
  }

  setSurfaceId(surfaceId: number): void {
    if (this._surfaceId === surfaceId) return;
    this.clearResizeConstraint();
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
    // Only forget the request once it is actually on the wire.  The
    // transport can be mid-reconnect, in which case the send is a no-op —
    // clearing first left nothing to retry, and the binding's own
    // last-sent dedup means the same size is never offered again, so the
    // surface stayed at the pre-resize size indefinitely.
    if (!conn.sendSurfaceResize(this._surfaceId, w, h, scale120)) return;
    this._pendingResize = null;
    this._resizeConstraintActive = true;
  }

  private clearResizeConstraint(): void {
    this._pendingResize = null;
    if (!this._resizeConstraintActive) return;
    this._resizeConstraintActive = false;
    this.getConn()?.sendSurfaceResize(this._surfaceId, 0, 0, 0);
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
  setDisplaySize(
    width: number | null,
    height?: number,
    scale120?: number,
  ): void {
    if (width == null) {
      const wasSized = this._displaySize !== null;
      this._displaySize = null;
      this.clearResizeConstraint();
      // Back to watching at the mediated size, so this view offers a scaled
      // request again.  See the note below on why the pair matters.
      if (wasSized) this.refreshScaledTarget();
      this.applyLayout();
      return;
    }
    const w = Math.round(width);
    const h = Math.round(height!);
    if (w <= 0 || h <= 0) return;
    const s =
      scale120 ??
      (typeof devicePixelRatio === "number"
        ? Math.round(devicePixelRatio * 120)
        : 0);
    const wasSized = this._displaySize !== null;
    this._displaySize = { width: w, height: h, scale120: s };
    // A scaled subscriber is left out of the server's size mediation
    // entirely: it asked to be served a downscale of whatever the surface
    // happens to be, so it gets no say in how big that is.  Gaining a
    // display size is what turns this view from one of those into a live
    // pane, and {@link scaledTarget} reads `_displaySize` — so the request
    // has to be re-derived here, not only when the box changes.
    //
    // Without it, a pane that was still 0×0 when its binding first measured
    // (the box observer then wins the race and registers a thumbnail's
    // target) keeps that target forever: the server skips the client in
    // mediation, every resize it sends is ignored, and the surface stays at
    // the size it had in the sidebar until the pane's box next crosses an
    // octave and the observer happens to re-derive.
    if (!wasSized) this.refreshScaledTarget();
    // Canvas backing buffer is intentionally NOT resized here.  It tracks
    // the decoded frame size (set in blitFromStore) so the last sharp
    // frame stays sharp while applyLayout() places it in the new
    // container.  Resizing the canvas pre-emptively would clear the
    // backing buffer and force a drawImage upscale of the stale frame,
    // producing a visible "blurry intermediate" step until the server's
    // keyframe at the requested size arrives.
    this.applyLayout();
  }

  /**
   * Size and position the canvas's CSS box for the current frame.
   *
   * A frame is shown at exactly one device pixel per canvas pixel — never
   * upscaled.  The mediated surface size is the minimum across subscribed
   * clients, so a smaller co-viewer shrinks the frames this client
   * receives; those are shown at their native size, centered.  Only a
   * frame *larger* than the container (transiently, mid-resize) is scaled
   * down to fit, aspect-preserved.
   *
   * Non-resizable views (thumbnails, the React binding) keep the
   * fill-and-contain CSS from attach() and let the box drive the size.  They
   * do track the container (see {@link _presentBox}) but only to pick a
   * halving chain in blitFromStore, never to place the canvas.
   */
  private applyLayout(): void {
    const canvas = this.canvas;
    if (!canvas) return;
    const ds = this._displaySize;
    if (!ds || !ds.scale120) {
      if (this._lastLayout) {
        this._lastLayout = null;
        Object.assign(canvas.style, {
          position: "",
          left: "",
          top: "",
          width: "100%",
          height: "100%",
        });
      }
      return;
    }
    const fw = canvas.width;
    const fh = canvas.height;
    if (fw === 0 || fh === 0) return;
    const fit = Math.min(1, ds.width / fw, ds.height / fh);
    const w = Math.floor(fw * fit);
    const h = Math.floor(fh * fit);
    const left = Math.max(0, Math.floor((ds.width - w) / 2));
    const top = Math.max(0, Math.floor((ds.height - h) / 2));
    const last = this._lastLayout;
    if (
      last &&
      last.left === left &&
      last.top === top &&
      last.w === w &&
      last.h === h
    ) {
      return;
    }
    this._lastLayout = { left, top, w, h };
    // All values are integer device pixels converted to CSS pixels, so the
    // canvas lands on the device grid and the browser blits 1:1.
    const scale = ds.scale120 / 120;
    Object.assign(canvas.style, {
      position: "absolute",
      left: `${left / scale}px`,
      top: `${top / scale}px`,
      width: `${w / scale}px`,
      height: `${h / scale}px`,
    });
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
    const { width, height, scale120 } = this._displaySize;
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
      conn.sendSurfaceSubscribe(
        this._surfaceId,
        this.surfaceViewId(conn),
        this.scaledTarget(),
      );
      this._subscribedGeneration = store.generation;
      this._subscribedSurface = {
        connectionId: this._connectionId,
        surfaceId: this._surfaceId,
      };
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
            // The reconnect is a new client to the server, which keeps
            // view sizes per client — so this view no longer counts in the
            // surface's size mediation until it says so again.  The
            // ResizeObserver won't: the container never changed size.
            this.resendDisplaySize();
          }
        }
        // Size the canvas backing buffer to the surface when info first
        // arrives so the canvas has sensible intrinsic dimensions before
        // any frame has been decoded.  blitFromStore will re-snap it to
        // the actual frame size on first paint.
        if (!prev && this.canvas) {
          this.canvas.width = this.surface.width;
          this.canvas.height = this.surface.height;
          this.applyLayout();
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
    this._lastFrameSize = { width: src.width, height: src.height };

    // A view that sizes its own box is already 1:1 and has nothing to
    // prefilter: the backing buffer mirrors the source frame exactly and
    // applyLayout sizes the CSS box to match (or, transiently mid-resize,
    // scales a too-large frame down proportionally).
    //
    // A view that is *handed* a box — a dock thumbnail — is about to be
    // minified by the compositor instead, so bring the frame down to roughly
    // the box in whole halves first and leave CSS a scale it can filter.
    const box = this._displaySize ? null : this._presentBox;
    const n = box ? halvings(src.width, src.height, box.width, box.height) : 0;
    this._presentHalvings = n;
    const w = halve(src.width, n);
    const h = halve(src.height, n);
    if (canvas.width !== w || canvas.height !== h) {
      canvas.width = w;
      canvas.height = h;
    }
    this.applyLayout();
    drawHalved(ctx, src, src.width, src.height, n);
  }

  private resubscribe(): void {
    this.serverUnsubscribe();
    this.unsubscribeAll();
    this._hasBlitFirstFrame = false;
    if (!this.disposed) this.subscribe();
  }

  private serverUnsubscribe(): void {
    const sub = this._subscribedSurface;
    if (!sub) return;
    const conn =
      (this._workspace as any).getConnection(sub.connectionId) ?? null;
    if (conn && this._surfaceViewId) {
      conn.sendSurfaceUnsubscribe(sub.surfaceId, this._surfaceViewId);
    }
    this._subscribedSurface = null;
    this._subscribedGeneration = -1;
  }

  /** This view's subscription token, allocated on first use and kept for
   *  the life of the canvas so a resubscribe reclaims the same slot. */
  private surfaceViewId(conn: BlitConnection): string {
    if (!this._surfaceViewId) {
      this._surfaceViewId = conn.allocSurfaceViewId();
    }
    return this._surfaceViewId;
  }

  /**
   * The fixed encode size to ask the server for, or null to watch the
   * surface at its mediated size.
   *
   * Only a view that is handed a box asks for one: a resizable view already
   * drives the surface's size through setDisplaySize, and asking it to
   * bypass mediation would leave nobody sizing the surface at all.
   *
   * The request is this view's own box, octave-rounded — deliberately not
   * anything derived from the surface's current size.  A resubscribe costs
   * the server an encoder rebuild and this client a keyframe, and the
   * surface's size moves whenever any *other* viewer resizes its pane; a
   * request that tracked it would re-ask every time somebody else dragged
   * a split.  The box only moves when this card does.
   *
   * Overshooting to the next octave is the cheap side of that trade: the
   * server inscribes the surface's aspect inside whatever box it is given
   * and never upscales past native, and the ≤2:1 residual is exactly what
   * {@link drawHalved} and a single CSS tap already handle.
   */
  private scaledTarget(): { width: number; height: number } | null {
    if (this._displaySize) return null;
    const box = this._presentBox;
    if (!box) return null;
    const width = octaveCeil(box.width);
    const height = octaveCeil(box.height);
    return width > 0 && height > 0 ? { width, height } : null;
  }

  /** Re-derive the scaled request after the box or the display size
   *  changed.
   *
   *  Nothing to re-derive before the box has been measured — the request is
   *  the box — or once disposed: `dispose()` clears the display size on its
   *  way to unsubscribing, and re-deriving there would put a subscribe on
   *  the wire, costing the server an encoder rebuild, immediately before
   *  the unsubscribe that makes it moot. */
  private refreshScaledTarget(): void {
    const sub = this._subscribedSurface;
    if (this.disposed || !this._presentBox || !sub || !this._surfaceViewId) {
      return;
    }
    const conn =
      (this._workspace as any).getConnection(sub.connectionId) ?? null;
    conn?.setSurfaceViewTarget(
      sub.surfaceId,
      this._surfaceViewId,
      this.scaledTarget(),
    );
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
    this.boundTouchStart = (e) => this.handleTouchStart(e);
    this.boundTouchMove = (e) => this.handleTouchMove(e);
    this.boundTouchEnd = (e) => this.handleTouchEnd(e);
    this.boundTouchCancel = (e) => this.handleTouchCancel(e);
    this.boundPointerDown = (e) => this.handlePointerDown(e);
    this.boundPointerMove = (e) => this.handlePointerMove(e);
    this.boundPointerUp = (e) => this.handlePointerUp(e);
    this.boundPointerCancel = (e) => this.handlePointerCancel(e);
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
    canvas.addEventListener("pointerdown", this.boundPointerDown);
    canvas.addEventListener("pointermove", this.boundPointerMove);
    canvas.addEventListener("pointerup", this.boundPointerUp);
    canvas.addEventListener("pointercancel", this.boundPointerCancel);
    canvas.addEventListener("touchstart", this.boundTouchStart, {
      passive: false,
    });
    canvas.addEventListener("touchmove", this.boundTouchMove, {
      passive: false,
    });
    canvas.addEventListener("touchend", this.boundTouchEnd, {
      passive: false,
    });
    canvas.addEventListener("touchcancel", this.boundTouchCancel, {
      passive: false,
    });
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
    if (this.boundPointerDown)
      canvas.removeEventListener("pointerdown", this.boundPointerDown);
    if (this.boundPointerMove)
      canvas.removeEventListener("pointermove", this.boundPointerMove);
    if (this.boundPointerUp)
      canvas.removeEventListener("pointerup", this.boundPointerUp);
    if (this.boundPointerCancel)
      canvas.removeEventListener("pointercancel", this.boundPointerCancel);
    if (this.boundTouchStart)
      canvas.removeEventListener("touchstart", this.boundTouchStart);
    if (this.boundTouchMove)
      canvas.removeEventListener("touchmove", this.boundTouchMove);
    if (this.boundTouchEnd)
      canvas.removeEventListener("touchend", this.boundTouchEnd);
    if (this.boundTouchCancel)
      canvas.removeEventListener("touchcancel", this.boundTouchCancel);
    this.clearActiveTouch();
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
    this._pendingPaste = null;
    this._pendingPasteFlush = null;
    this.clearPasteDeadline();

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
    this.sendPointerAt(e.clientX, e.clientY, type, e.button);
  }

  private sendPointerAt(
    clientX: number,
    clientY: number,
    type: number,
    button: number,
  ): void {
    const conn = this.getConn();
    if (!conn || !this.canvas || !this.surface || !this._displaySize) return;
    if (type === SURFACE_POINTER_DOWN) {
      this.canvas.focus();
      this.pressedButtons.add(button);
    } else if (type === SURFACE_POINTER_UP) {
      this.pressedButtons.delete(button);
    }
    const point = this.surfacePointFromClient(clientX, clientY);
    if (!point) return;
    conn.sendSurfacePointer(this._surfaceId, type, button, point.x, point.y);
  }

  /**
   * Where the frame is actually drawn, in CSS pixels, plus the scale that
   * takes CSS pixels to surface coordinates.
   *
   * In resizable views applyLayout() sizes the CSS box to the drawn frame
   * exactly, so the letterbox degenerates to dx = dy ≈ 0; views still on
   * the fill-and-contain default (thumbnails) letterbox the intrinsic
   * aspect within the box via object-fit: contain.
   *
   * Pointer positions and scroll distances both go through this, so a
   * wheel and a drag move content by the same amount on a letterboxed or
   * downscaled surface.
   */
  private drawnGeometry(): {
    dx: number;
    dy: number;
    dw: number;
    dh: number;
    sx: number;
    sy: number;
    rect: DOMRect;
  } | null {
    if (!this.canvas || !this.surface) return null;
    const rect = this.canvas.getBoundingClientRect();
    const cw = this.canvas.width;
    const ch = this.canvas.height;
    if (cw === 0 || ch === 0 || rect.width === 0 || rect.height === 0)
      return null;
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
    if (dw === 0 || dh === 0) return null;
    return {
      dx,
      dy,
      dw,
      dh,
      sx: this.surface.width / dw,
      sy: this.surface.height / dh,
      rect,
    };
  }

  private surfacePointFromClient(
    clientX: number,
    clientY: number,
  ): { x: number; y: number } | null {
    const g = this.drawnGeometry();
    if (!g) return null;
    return {
      x: Math.round((clientX - g.rect.left - g.dx) * g.sx),
      y: Math.round((clientY - g.rect.top - g.dy) * g.sy),
    };
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

  private clearActiveTouch(): void {
    if (this.activeTouch?.longPressTimer) {
      clearTimeout(this.activeTouch.longPressTimer);
    }
    this.activeTouch = null;
  }

  private findActiveTouch(list: TouchList): Touch | null {
    const active = this.activeTouch;
    if (!active) return null;
    for (let i = 0; i < list.length; i++) {
      const touch = list.item(i);
      if (touch && touch.identifier === active.identifier) return touch;
    }
    return null;
  }

  private startTouchGesture(
    identifier: number,
    clientX: number,
    clientY: number,
    pointerId?: number,
  ): void {
    if (!this.canvas || !this.surface || !this._displaySize) return;
    this.canvas.focus();
    this.clearActiveTouch();
    this.activeTouch = {
      identifier,
      startX: clientX,
      startY: clientY,
      lastX: clientX,
      lastY: clientY,
      mode: "pending",
      pointerId,
      longPressTimer: setTimeout(() => {
        const active = this.activeTouch;
        if (!active || active.identifier !== identifier) return;
        active.longPressTimer = null;
        active.mode = "drag";
        this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_MOVE, 0);
        this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_DOWN, 0);
      }, 350),
    };
  }

  private moveTouchGesture(clientX: number, clientY: number): void {
    const active = this.activeTouch;
    if (!active) return;

    const dx = clientX - active.lastX;
    const dy = clientY - active.lastY;
    const totalDx = clientX - active.startX;
    const totalDy = clientY - active.startY;
    const moved = Math.hypot(totalDx, totalDy);

    if (active.mode === "pending" && moved > 8) {
      if (active.longPressTimer) clearTimeout(active.longPressTimer);
      active.longPressTimer = null;
      active.mode = "scroll";
    }

    active.lastX = clientX;
    active.lastY = clientY;

    if (active.mode === "drag") {
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_MOVE, 0);
    } else if (active.mode === "scroll") {
      const g = this.drawnGeometry();
      if (!g) return;
      // A finger dragging the content up scrolls down, hence the sign.
      // This genuinely is a finger, so it is never a wheel.
      if (dx !== 0 || dy !== 0) {
        this.queueScroll({
          dx: -dx * g.sx,
          dy: -dy * g.sy,
          v120x: 0,
          v120y: 0,
          smooth: true,
        });
      }
    }
  }

  private endTouchGesture(clientX: number, clientY: number): void {
    const active = this.activeTouch;
    if (!active) return;
    if (active.longPressTimer) clearTimeout(active.longPressTimer);

    if (active.mode === "drag") {
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_MOVE, 0);
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_UP, 0);
    } else if (active.mode === "pending") {
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_DOWN, 0);
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_UP, 0);
    } else if (active.mode === "scroll") {
      // The finger left the glass, so the gesture is over now — no need
      // to wait out the idle timeout the way a wheel has to.
      this.endScrollSequence();
    }
    this.activeTouch = null;
  }

  private handlePointerDown(e: PointerEvent): void {
    if (e.pointerType === "mouse") return;
    if (!this.canvas || !this.surface || !this._displaySize) return;
    e.preventDefault();
    this.canvas.setPointerCapture?.(e.pointerId);
    this.startTouchGesture(e.pointerId, e.clientX, e.clientY, e.pointerId);
  }

  private handlePointerMove(e: PointerEvent): void {
    const active = this.activeTouch;
    if (
      e.pointerType === "mouse" ||
      !active ||
      active.pointerId !== e.pointerId
    )
      return;
    e.preventDefault();
    this.moveTouchGesture(e.clientX, e.clientY);
  }

  private handlePointerUp(e: PointerEvent): void {
    const active = this.activeTouch;
    if (
      e.pointerType === "mouse" ||
      !active ||
      active.pointerId !== e.pointerId
    )
      return;
    e.preventDefault();
    this.canvas?.releasePointerCapture?.(e.pointerId);
    this.endTouchGesture(e.clientX, e.clientY);
  }

  private handlePointerCancel(e: PointerEvent): void {
    const active = this.activeTouch;
    if (
      e.pointerType === "mouse" ||
      !active ||
      active.pointerId !== e.pointerId
    )
      return;
    e.preventDefault();
    this.canvas?.releasePointerCapture?.(e.pointerId);
    if (active.mode === "drag") {
      this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_UP, 0);
    }
    this.clearActiveTouch();
  }

  private handleTouchStart(e: TouchEvent): void {
    if (!this.canvas || !this.surface || !this._displaySize) return;
    // Cancel the touch default before anything else can bail out, including
    // when the pointer-event path already owns this gesture.  Cancelling
    // `touchstart` is what stops the browser from replaying the tap as
    // compatibility mouse events, and on iPadOS `pointerdown` lands first
    // and claims the gesture, so the guard below used to skip this and let
    // a synthetic mousedown/mouseup through to handleMouse() — a second
    // click on top of the one the gesture itself sends.  The canvas carries
    // `touch-action: none` and owns every gesture on it, so there is no
    // default here worth keeping.
    e.preventDefault();
    if (this.activeTouch?.pointerId != null) return;
    if (e.touches.length !== 1) {
      this.handleTouchCancel(e);
      return;
    }
    const touch = e.touches.item(0);
    if (!touch) return;
    this.startTouchGesture(touch.identifier, touch.clientX, touch.clientY);
  }

  private handleTouchMove(e: TouchEvent): void {
    e.preventDefault();
    const active = this.activeTouch;
    if (!active || active.pointerId != null) return;
    const touch = this.findActiveTouch(e.touches);
    if (!touch) return;
    this.moveTouchGesture(touch.clientX, touch.clientY);
  }

  private handleTouchEnd(e: TouchEvent): void {
    // Same reasoning as handleTouchStart: the pointer path has usually
    // already ended the gesture and nulled activeTouch by the time this
    // runs, so cancel the default first or the guards below skip it.
    e.preventDefault();
    const active = this.activeTouch;
    if (!active) return;
    const touch = this.findActiveTouch(e.changedTouches);
    if (!touch) return;
    if (active.longPressTimer) clearTimeout(active.longPressTimer);

    if (active.mode === "drag") {
      this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_UP, 0);
    } else if (active.mode === "pending") {
      // A tap is a left click.  Use the release coordinate to match what the
      // user sees if their finger drifted slightly during the tap.
      this.sendPointerAt(touch.clientX, touch.clientY, SURFACE_POINTER_DOWN, 0);
      this.sendPointerAt(touch.clientX, touch.clientY, SURFACE_POINTER_UP, 0);
    }
    this.activeTouch = null;
  }

  private handleTouchCancel(e: TouchEvent): void {
    const active = this.activeTouch;
    if (!active) return;
    e.preventDefault();
    if (active.longPressTimer) clearTimeout(active.longPressTimer);
    if (active.mode === "drag") {
      this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_UP, 0);
    } else if (active.mode === "scroll") {
      this.endScrollSequence();
    }
    this.activeTouch = null;
  }

  /**
   * True when this event looks like it came from a smooth device rather
   * than a notched wheel.
   *
   * macOS is the reason this matters: it applies its own acceleration
   * curve and appends a momentum tail, and browsers report the result as
   * ordinary pixel-mode wheel events. Forwarding those unlabelled makes
   * the Wayland client read them as wheel detents — `axis_source`'s
   * default value is `wheel` — and scale them up by a lines-per-click
   * factor on top of the acceleration macOS already applied. That
   * double-multiply is what makes Mac scrolling feel violent on the
   * Linux side.
   */
  private wheelLooksSmooth(e: WheelEvent): boolean {
    // Line and page modes only ever describe a notched wheel.
    if (e.deltaMode !== 0) return false;
    if (!Number.isInteger(e.deltaX) || !Number.isInteger(e.deltaY)) return true;
    // A real wheel moves one axis at a time.
    if (e.deltaX !== 0 && e.deltaY !== 0) return true;
    const mag = Math.abs(e.deltaX || e.deltaY);
    return mag === 0 || mag % WHEEL_DETENT_PX !== 0;
  }

  private handleWheel(e: WheelEvent): void {
    // No display size means a thumbnail rather than a live view, and
    // those take no other input either. Claiming the wheel there would
    // scroll an app the user is only previewing, and the preventDefault
    // below would stop the page scrolling under the cursor.
    if (!this.getConn() || !this.surface || !this._displaySize) return;
    // Ctrl+wheel is how browsers report a pinch-zoom gesture, including
    // macOS trackpad pinches. It is a zoom request, not a scroll; sending
    // it on would scroll the surface while the user pinches.
    if (e.ctrlKey) return;
    const g = this.drawnGeometry();
    if (!g) return;
    e.preventDefault();

    // Once a sequence shows any sign of being smooth it stays smooth: a
    // trackpad flick's momentum tail can emit whole-number deltas that
    // would otherwise be misread as detents mid-gesture. The latch has to
    // win before the detent maths below, not just when labelling the
    // source, or a finger-sourced event ends up carrying wheel notches.
    const smooth = this.wheelLooksSmooth(e) || this.scrollSmoothLatch;
    let { deltaX, deltaY } = e;
    let v120x = 0;
    let v120y = 0;

    if (e.deltaMode === WHEEL_MODE_LINE) {
      v120x = (deltaX / WHEEL_LINES_PER_DETENT) * 120;
      v120y = (deltaY / WHEEL_LINES_PER_DETENT) * 120;
      deltaX *= WHEEL_LINE_PX;
      deltaY *= WHEEL_LINE_PX;
    } else if (e.deltaMode === WHEEL_MODE_PAGE) {
      v120x = deltaX * 120;
      v120y = deltaY * 120;
      deltaX *= g.dw;
      deltaY *= g.dh;
    } else if (!smooth) {
      // Pixel-mode wheel: browsers that report notches this way use
      // 120px per detent.
      v120x = (deltaX / WHEEL_DETENT_PX) * 120;
      v120y = (deltaY / WHEEL_DETENT_PX) * 120;
    }

    this.queueScroll({
      dx: deltaX * g.sx,
      dy: deltaY * g.sy,
      v120x,
      v120y,
      smooth,
    });
  }

  /**
   * Add to the pending scroll and arrange for it to be sent.
   *
   * Deltas are batched to one message per animation frame. macOS momentum
   * alone emits events at 60–120Hz for a second or more after the fingers
   * lift; one message each turns into one `wl_pointer.frame` each, and
   * network jitter then delivers them in bursts that read as stutter no
   * matter how correct the magnitudes are.
   */
  private queueScroll(part: {
    dx: number;
    dy: number;
    v120x: number;
    v120y: number;
    smooth: boolean;
  }): void {
    this.scrollSmoothLatch = part.smooth;
    this.scrollSequenceOpen = true;
    const a = (this.scrollAccum ??= { dx: 0, dy: 0, v120x: 0, v120y: 0 });
    a.dx += part.dx;
    a.dy += part.dy;
    a.v120x += part.v120x;
    a.v120y += part.v120y;

    if (this.scrollFlushHandle === null) {
      this.scrollFlushHandle = requestAnimationFrame(() => {
        this.scrollFlushHandle = null;
        this.flushScroll();
      });
    }
    if (this.scrollStopTimer !== null) clearTimeout(this.scrollStopTimer);
    this.scrollStopTimer = setTimeout(
      () => this.endScrollSequence(),
      SCROLL_STOP_MS,
    );
  }

  private flushScroll(): void {
    const a = this.scrollAccum;
    this.scrollAccum = null;
    if (!a) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    if (a.dx === 0 && a.dy === 0 && a.v120x === 0 && a.v120y === 0) return;
    conn.sendSurfaceAxis2(this._surfaceId, {
      dx: a.dx,
      dy: a.dy,
      v120x: a.v120x,
      v120y: a.v120y,
      source: this.scrollSmoothLatch ? AXIS_SOURCE_FINGER : AXIS_SOURCE_WHEEL,
      stop: false,
    });
  }

  /**
   * Tell the client the gesture is over.
   *
   * Without this the app never learns a scroll ended, so toolkits that
   * gate kinetic scrolling on a stop event either keep flinging or never
   * settle. Claiming a `finger` source obliges us to send it.
   *
   * A notched wheel gets no stop: the protocol says a `wheel` sequence
   * may or may not be terminated and that clients must not rely on it,
   * and a wheel has no finger-lift moment to report anyway. Sending one
   * only gives toolkits a reason to invent momentum — Chromium starts a
   * fling off any `axis_stop`, without checking the source when it has a
   * single frame of history to regress.
   */
  private endScrollSequence(): void {
    if (this.scrollStopTimer !== null) {
      clearTimeout(this.scrollStopTimer);
      this.scrollStopTimer = null;
    }
    if (this.scrollFlushHandle !== null) {
      cancelAnimationFrame(this.scrollFlushHandle);
      this.scrollFlushHandle = null;
      this.flushScroll();
    }
    const wasSmooth = this.scrollSmoothLatch;
    this.scrollSmoothLatch = false;
    if (!this.scrollSequenceOpen) return;
    this.scrollSequenceOpen = false;
    if (!wasSmooth) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    conn.sendSurfaceAxis2(this._surfaceId, {
      dx: 0,
      dy: 0,
      v120x: 0,
      v120y: 0,
      source: AXIS_SOURCE_FINGER,
      stop: true,
    });
  }

  // Fallback clipboard-read path for browsers/contexts where
  // `navigator.clipboard.readText()` is denied (Brave without granted
  // permission, Firefox, insecure contexts, ...).  The `paste` event
  // delivers clipboard data synchronously without a permission prompt.
  private handlePaste(e: ClipboardEvent): void {
    const claimed = e as ClipboardEvent & { [PASTE_CLAIMED]?: true };
    if (claimed[PASTE_CLAIMED]) return;
    claimed[PASTE_CLAIMED] = true;
    e.preventDefault();
    if (!this._displaySize) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;

    // Claim the pending paste up front.  Reading an image blob is
    // asynchronous, and a `readText()` resolving in the meantime must not
    // paste the text representation out from under the image.  `abandon` is
    // the chord's own cleanup, captured before it can be cleared: an image we
    // decline to forward has to stand the chord down, not press V behind it.
    const flush = this._pendingPasteFlush;
    const abandon = this._pendingPasteAbandon;
    this._pendingPasteFlush = null;

    const image = clipboardImage(e.clipboardData);
    if (image) {
      this.extendPasteDeadline(PASTE_IMAGE_MS);
      void image
        .arrayBuffer()
        .then((buf) => {
          if (buf.byteLength > MAX_CLIPBOARD_BYTES) {
            console.warn(
              `blit: clipboard image is ${buf.byteLength} bytes, over the ` +
                `${MAX_CLIPBOARD_BYTES}-byte paste limit — not pasted`,
            );
            abandon?.();
            return;
          }
          const payload = {
            mime: image.type || "image/png",
            data: new Uint8Array(buf),
          };
          if (flush) flush(payload);
          else conn.sendClipboard(payload.mime, payload.data);
        })
        // Same for a blob we could not read: we know an image was there, so
        // pressing V would paste something the user did not copy.
        .catch(() => abandon?.());
      return;
    }

    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (flush) {
      // An empty clipboard still presses V, and that is not the stale paste
      // the image paths above refuse.  Nothing was withheld here, so the
      // selection the app goes on to read is whichever *Wayland* client owns
      // it — copy in one surface and paste into another, with the browser
      // never in the middle.  Standing the chord down would break that.
      flush(text ? textPayload(text) : null);
    } else if (text) {
      const payload = textPayload(text);
      conn.sendClipboard(payload.mime, payload.data);
    }
  }

  /** Push the in-flight paste's safety net back, for a clipboard read that
   *  needs longer than the synchronous paths do. */
  private extendPasteDeadline(ms: number): void {
    if (!this._pendingPasteAbandon) return;
    if (this._pendingPasteTimer !== null) {
      clearTimeout(this._pendingPasteTimer);
    }
    this._pendingPasteTimer = setTimeout(this._pendingPasteAbandon, ms);
  }

  private clearPasteDeadline(): void {
    if (this._pendingPasteTimer !== null) {
      clearTimeout(this._pendingPasteTimer);
      this._pendingPasteTimer = null;
    }
    this._pendingPasteAbandon = null;
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
      const flush = (payload: ClipboardPayload | null) => {
        const p = this._pendingPaste;
        if (!p || p.keycode !== keycode) return;
        this._pendingPaste = null;
        this._pendingPasteFlush = null;
        this.clearPasteDeadline();
        if (payload) {
          conn.sendClipboard(payload.mime, payload.data);
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

      // Safety net — if neither readText nor the paste event ever
      // delivers (both paths blocked), clean up the pending state and
      // undo the Meta→Ctrl translation.  Don't force V through without
      // clipboard data; pasting stale content is worse than doing
      // nothing.  Armed before either read starts so that a paste event
      // carrying an image has a deadline to push back.
      this._pendingPasteAbandon = () => {
        const p = this._pendingPaste;
        if (!p || p.keycode !== keycode) return;
        this._pendingPaste = null;
        this._pendingPasteFlush = null;
        this.clearPasteDeadline();
        if (p.deferredCtrlRelease) {
          this.pressedKeys.delete(29);
          conn.sendSurfaceInput(surfaceId, 29, false);
          this._metaToCtrlKey = 0;
        }
        if (this.canvas && document.activeElement === this.textInput) {
          this.canvas.focus();
        }
      };
      this._pendingPasteTimer = setTimeout(
        this._pendingPasteAbandon,
        PASTE_READ_MS,
      );

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
          // `_pendingPasteFlush` being cleared means a paste event already
          // claimed this chord: an image is on its way and its text
          // representation, if any, must not pre-empt it.
          if (text && this._pendingPasteFlush) flush(textPayload(text));
        },
        () => {
          /* paste event will flush */
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
