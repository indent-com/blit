import type { ConnectionId, BlitSurface } from "./types";
import {
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H264_444,
  CODEC_SUPPORT_AV1_444,
  AXIS_SOURCE_CONTINUOUS,
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
  SCROLL_STOP_MS,
  WHEEL_DETENT_PX,
  WHEEL_LINE_PX,
  WHEEL_LINES_PER_DETENT,
  WHEEL_MODE_LINE,
  WHEEL_MODE_PAGE,
} from "./wheel";
import {
  devicePixelBox,
  drawHalved,
  halve,
  halvings,
  octaveCeil,
} from "./downscale";

/** Cached codec support bitmask.  Computed once, reused for all resize messages. */
let _codecSupport: number | null = null;

/** What the probe found, before any demotion narrowed it.  Restoring a
 *  demoted codec may only ever re-offer bits this browser did probe as
 *  working — never invent support the probe never saw. */
let _probedCodecSupport = 0;

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
export function av1LevelString(width: number, height: number): string {
  const pic = width * height;
  const rate = pic * 60;
  // [seq_level_idx, maxPicSize, maxHSize, maxVSize, maxDisplayRate] —
  // spec Table A.3.  Levels whose limits duplicate the previous row for
  // these fields (5.3, 6.3) can never be picked and are folded into the
  // fallthrough, exactly like the server table.
  const specs: [number, number, number, number, number][] = [
    [0, 147456, 2048, 1152, 4423680],
    [1, 278784, 2816, 1584, 8363520],
    [4, 665856, 4352, 2448, 19975680],
    [5, 1065024, 5504, 3096, 31950720],
    [8, 2359296, 6144, 3456, 70778880],
    [9, 2359296, 6144, 3456, 141557760],
    [12, 8912896, 8192, 4352, 267386880],
    [13, 8912896, 8192, 4352, 534773760],
    [14, 8912896, 8192, 4352, 1069547520],
    [16, 35651584, 16384, 8704, 1069547520],
    [17, 35651584, 16384, 8704, 2139095040],
    [18, 35651584, 16384, 8704, 4278190080],
  ];
  for (const [idx, maxPic, maxW, maxH, maxRate] of specs) {
    if (pic <= maxPic && width <= maxW && height <= maxH && rate <= maxRate)
      return String(idx).padStart(2, "0");
  }
  return "19";
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
  _probedCodecSupport = mask;
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
 * Drop codec-support bits after the stream they selected proved
 * undecodable in practice — the probe's tiny test frames pass on decoders
 * that then reject the real stream.  Returns the new mask, or null when
 * nothing changed: the probe hasn't finished (nothing to demote), the bits
 * were already clear, or clearing them would zero the mask — which the
 * wire protocol reads as "accept anything" and would undo the demotion.
 */
export function demoteCodecSupport(bits: number): number | null {
  if (_codecSupport === null) return null;
  const next = _codecSupport & ~bits;
  if (next === _codecSupport || next === 0) return null;
  _codecSupport = next;
  return next;
}

/**
 * Re-offer bits a previous {@link demoteCodecSupport} withdrew, once the
 * failures that triggered it are far enough behind to have been a transient
 * fault (a GPU reset, a decoder the browser had briefly wedged) rather than
 * a codec this platform cannot handle.  Returns the new mask, or null when
 * nothing changed — the probe never confirmed those bits, or they are
 * already offered.
 */
export function restoreCodecSupport(bits: number): number | null {
  if (_codecSupport === null) return null;
  const next = _codecSupport | (bits & _probedCodecSupport);
  if (next === _codecSupport) return null;
  _codecSupport = next;
  return next;
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

/**
 * True when the browser is on macOS/iPadOS, where the Alt key doubles as
 * the Option character modifier: Option+E is a dead key, Option+F types
 * "ƒ".  Only there is the Alt press held back pending dead-key detection;
 * elsewhere Alt means the modifier alone and is forwarded immediately.
 * (`navigator.platform` is deprecated but is the only source Firefox and
 * Safari implement; iPadOS reports "MacIntel", which is the right answer
 * here since its keyboards do dead keys too.)
 */
function detectMacOptionChars(): boolean {
  const nav = navigator as Navigator & {
    userAgentData?: { platform?: string };
  };
  const platform = (
    nav.userAgentData?.platform ??
    nav.platform ??
    ""
  ).toLowerCase();
  if (platform) return platform.startsWith("mac") || platform.startsWith("ip");
  return /mac|ipad|iphone/.test((nav.userAgent ?? "").toLowerCase());
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
//
// Wheel units live in ./wheel, shared with the terminal surface: the same
// events reach both, and only one of them should be deciding what a notch
// is.

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

/**
 * The page's text selection as a payload, or null when there is none.
 *
 * Null deliberately means "say nothing rather than nothing-in-particular":
 * the middle click still reaches the app, which then pastes whatever
 * *Wayland* client owns PRIMARY — select in one surface, middle-click in
 * another, with the browser never in the middle. Offering an empty
 * selection instead would take ownership away from that client and paste
 * zero bytes.
 */
function selectedPayload(): ClipboardPayload | null {
  const text = document.getSelection()?.toString() ?? "";
  if (!text) return null;
  const payload = textPayload(text);
  if (payload.data.length > MAX_CLIPBOARD_BYTES) {
    console.warn(
      `blit: selection is ${payload.data.length} bytes, over the ` +
        `${MAX_CLIPBOARD_BYTES}-byte limit — not offered as PRIMARY`,
    );
    return null;
  }
  return payload;
}

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

  /** Alt presses held back pending dead-key detection (evdev keycodes).
   *  A macOS Option keydown may turn out to be the start of a dead-key
   *  composition (Option+E → é), in which case the Alt press must never
   *  reach the app: Electron apps (Slack) react to a bare Alt press by
   *  activating their menu bar, which then swallows the composed text. */
  private pendingAlt = new Set<number>();

  /** Alt presses that a dead-key composition consumed: never forwarded,
   *  so their physical key-up must be ignored too. */
  private swallowedAlt = new Set<number>();

  /** Whether the browser's Alt key doubles as the macOS Option character
   *  modifier.  Only then is the Alt press held back (pendingAlt above);
   *  on other platforms it is forwarded immediately, keeping Alt-tap and
   *  Alt-hold semantics for apps that react to them. */
  private macOptionChars = detectMacOptionChars();

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
  /** `axis_source` of the in-flight sequence, null between sequences.
   *  Latched by {@link latchScrollSource} so a momentum tail cannot be
   *  reclassified as a wheel mid-gesture. */
  private scrollSource: number | null = null;
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
  private boundFocus: ((e: FocusEvent) => void) | null = null;
  private boundBlur: ((e: FocusEvent) => void) | null = null;
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
    // The label is the UI's handle on this element: the mobile keyboard
    // toggle focuses it (the canvas is not editable, so an IME will not
    // stay up for it) and the inputmode stamping covers it.
    ta.setAttribute("aria-label", "Surface input");
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
    // Only forget the request once the connection has it on the wire.
    // The transport can be mid-reconnect, in which case the offer is a
    // no-op — clearing first left nothing to retry, and the binding's own
    // last-sent dedup means the same size is never offered again, so the
    // surface stayed at the pre-resize size indefinitely.
    if (
      !conn.offerSurfaceViewSize(
        this._surfaceId,
        this.surfaceViewId(conn),
        w,
        h,
        scale120,
      )
    ) {
      return;
    }
    this._pendingResize = null;
    this._resizeConstraintActive = true;
  }

  private clearResizeConstraint(): void {
    this._pendingResize = null;
    if (!this._resizeConstraintActive) return;
    this._resizeConstraintActive = false;
    if (this._surfaceViewId) {
      this.getConn()?.withdrawSurfaceViewSize(
        this._surfaceId,
        this._surfaceViewId,
      );
    }
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
   * The box comes from the view's own display size, not from the frame:
   * the stream is only ever an approximation of what was asked for — the
   * server mediates across subscribed clients, rounds to the even 4:2:0
   * grid, and may serve a downscale of the surface — and a box derived
   * from it would move by a pixel or two every time any of those changed,
   * with the picture never quite reaching the edge of its pane.  The frame
   * is instead fitted to the box, aspect-preserved, so a genuinely
   * different aspect ratio still letterboxes and nothing shifts when the
   * stream size does.
   *
   * Non-resizable views (thumbnails, the React binding) keep the
   * fill-and-contain CSS from attach() and let the box drive the size.  They
   * do track the container (see {@link _presentBox}) but only to pick a
   * halving chain in blitFromStore, never to place the canvas.
   */
  /**
   * The box, in this view's device pixels, the surface may be drawn into:
   * the pane, but never larger than the surface's own logical size at this
   * view's DPR.  Null for views that don't size their own box.
   *
   * The server mediates one surface across all its viewers at the
   * *highest* DPR any of them asked for (see `mediated_size_for_surface`),
   * so a small 3x pane and a large 1x pane settle on a small window
   * composited at 3x.  Filling the 1x pane with that frame would show the
   * window at 3x zoom — the same window drawn three times too big on the
   * client that never asked for a high-DPI anything.  Capping at
   * `logical × own DPR` draws it at the size it actually is and lets the
   * rest of the pane letterbox.
   *
   * The cap only bites when it clears the pane by more than rounding
   * noise.  The viewer that *is* sizing the surface gets a cap within a
   * pixel or two of its own pane — mediation rounds the logical size onto
   * the even 4:2:0 grid — and snapping those back to the pane keeps its
   * stream landing on the pane exactly rather than a hairline inside it.
   */
  private presentationBox(): { width: number; height: number } | null {
    const ds = this._displaySize;
    if (!ds || !ds.scale120) return null;
    const lw = this.surface?.logicalWidth ?? 0;
    const lh = this.surface?.logicalHeight ?? 0;
    // Unknown logical size (old server, or no resize reported yet): the
    // pane is the only answer we have.
    if (lw <= 0 || lh <= 0) return { width: ds.width, height: ds.height };
    const SNAP = 3;
    const capW = Math.round((lw * ds.scale120) / 120);
    const capH = Math.round((lh * ds.scale120) / 120);
    return {
      width: capW < ds.width - SNAP ? capW : ds.width,
      height: capH < ds.height - SNAP ? capH : ds.height,
    };
  }

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
    const box = this.presentationBox() ?? ds;
    // Rounding, not flooring, and clamped to the box: a stream that is the
    // box's aspect to within the grid it was rounded onto has to land on
    // the box exactly, not a pixel inside it.
    const fit = Math.min(box.width / fw, box.height / fh);
    const w = Math.min(box.width, Math.round(fw * fit));
    const h = Math.min(box.height, Math.round(fh * fit));
    // Centred in the *pane*, not the box: when the box is the smaller of
    // the two the difference is the letterbox, and it belongs on both
    // sides.
    const left = Math.max(0, Math.round((ds.width - w) / 2));
    const top = Math.max(0, Math.round((ds.height - h) / 2));
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
    // canvas lands on the device grid — a stream served at the size that
    // was asked for is then blitted 1:1.
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

    // A view that sizes its own box usually has nothing to prefilter: the
    // backing buffer mirrors the source frame exactly and applyLayout fits
    // it to the pane, which is the size the stream was asked for and so is
    // at or near 1:1 — halvings() returns 0 for that and this is a no-op.
    // It is not always 1:1 though: a 1x viewer watching a surface a
    // high-DPI viewer sized draws it capped to its logical size, which can
    // be a whole multiple down.
    //
    // A view that is *handed* a box — a dock thumbnail — is about to be
    // minified by the compositor instead, so bring the frame down to roughly
    // the box in whole halves first and leave CSS a scale it can filter.
    const box = this._displaySize ? this.presentationBox() : this._presentBox;
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
    this.boundFocus = (e) => this.handleFocus(e);
    this.boundBlur = (e) => this.handleBlur(e);
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
      // Focus *rests* on the textarea, so it carries the same
      // compositor-focus and key-release bookkeeping as the canvas.
      ta.addEventListener("focus", this.boundFocus);
      ta.addEventListener("blur", this.boundBlur);
      // Paste into the textarea would otherwise insert text that the
      // `input` handler forwards as surface text — intercept it so the
      // content goes through the Wayland clipboard path instead.
      if (this.boundPaste) ta.addEventListener("paste", this.boundPaste);
    }

    // Belt and braces for a browser that starts a composition on the canvas
    // anyway.  Chromium does not — it fires nothing at all while a canvas
    // holds focus, which is why the handoff cannot wait for this event and
    // happens on focus instead.
    this.boundCompositionStart = () => {
      if (this.textInput) this.textInput.focus({ preventScroll: true });
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
      if (this.boundFocus) ta.removeEventListener("focus", this.boundFocus);
      if (this.boundBlur) ta.removeEventListener("blur", this.boundBlur);
      if (this.boundPaste) ta.removeEventListener("paste", this.boundPaste);
    }
  }

  private handleMouse(e: MouseEvent, type: number): void {
    // Read the selection first: focusing the canvas below collapses it, so
    // by the time the button is on the wire there is nothing left to send.
    const primary =
      e.button === 1 && type === SURFACE_POINTER_DOWN
        ? selectedPayload()
        : null;
    // Back and forward navigate the page — out of the session entirely —
    // and middle click starts an autoscroll, all while the same press is
    // on its way to the app. Claim them; the surface still gets the
    // button. Left and right keep their defaults: the canvas wants the
    // focus that a left press brings, and `contextmenu` is cancelled
    // separately so a right press is already harmless.
    if (e.button === 1 || e.button >= 3) e.preventDefault();
    // Hand PRIMARY over on the press that pastes it, the way the clipboard
    // is pushed on paste rather than on copy. The compositor serves these
    // bytes itself, so owning the selection continuously would displace
    // whichever Wayland client the user last selected text in — including
    // when they middle-click with nothing selected here, which has to keep
    // pasting that client's selection. Ordering holds because both
    // messages ride the same connection, and the compositor advertises the
    // offer before it delivers the button.
    if (primary) this.getConn()?.sendPrimary(primary.mime, primary.data);
    this.sendPointerAt(e.clientX, e.clientY, type, e.button);
  }

  /** Focus where keystrokes should land: the editable textarea, so an input
   *  method has something to attach to.  The canvas routes the same key
   *  handlers, so it stands in only while the textarea does not exist. */
  private focusKeyboardTarget(): void {
    const target = this.textInput ?? this.canvas;
    target?.focus({ preventScroll: true });
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
      this.focusKeyboardTarget();
      this.pressedButtons.add(button);
      // Alt+click is a real chord: any Alt press still pending dead-key
      // detection belongs ahead of this button.
      this.flushPendingAlt(conn);
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
   * In resizable views applyLayout() gives the CSS box the frame's own
   * aspect, so the letterbox degenerates to dx = dy ≈ 0; views still on
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
    this.focusKeyboardTarget();
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
          source: AXIS_SOURCE_FINGER,
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
    } else if (active.mode === "scroll") {
      // As in endTouchGesture(): the finger left the glass, so say so now
      // rather than letting the idle timer say it 280ms late. A flick is
      // supposed to coast, and Chromium reads no velocity into a stop
      // that arrives more than 200ms after the frames it would regress
      // one from — a late stop lands the gesture dead.
      this.endScrollSequence();
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
   * The `wl_pointer.axis_source` a wheel event deserves.
   *
   * Two answers, and neither is `finger`. A DOM wheel event never proves
   * a finger is on anything: macOS delivers a trackpad and a notched
   * wheel through the same pixel deltas, having already applied its own
   * acceleration curve to both. `finger` is the one source that invites
   * a toolkit to append momentum of its own — it obliges us to send an
   * `axis_stop`, and Chromium turns that into a fling — so claiming it
   * off a guess is how one notch of a real wheel ends up gliding.
   * `continuous` describes the same smooth stream without licensing that
   * second helping. Real fingers arrive through the touch handlers,
   * which don't have to guess.
   *
   * That leaves only the unmistakable wheels to spot: a `deltaMode`
   * coarser than pixels, or a whole number of 120px detents. Everything
   * else takes the harmless path, which costs a misread trackpad
   * nothing and a misread wheel only its detents.
   */
  private wheelAxisSource(e: WheelEvent): number {
    // Line and page modes only ever describe a notched wheel.
    if (e.deltaMode !== 0) return AXIS_SOURCE_WHEEL;
    if (!Number.isInteger(e.deltaX) || !Number.isInteger(e.deltaY))
      return AXIS_SOURCE_CONTINUOUS;
    // A real wheel moves one axis at a time.
    if (e.deltaX !== 0 && e.deltaY !== 0) return AXIS_SOURCE_CONTINUOUS;
    const mag = Math.abs(e.deltaX || e.deltaY);
    return mag !== 0 && mag % WHEEL_DETENT_PX === 0
      ? AXIS_SOURCE_WHEEL
      : AXIS_SOURCE_CONTINUOUS;
  }

  /**
   * Fold a source into the open sequence and answer with what the
   * sequence now is.
   *
   * A sequence only ever gets smoother. A trackpad's momentum tail can
   * land on a round 120px mid-flick, and calling that a wheel would hand
   * the client a detent it scales up by its own lines-per-click factor.
   * A finger overrides either, since the touch handlers know what they
   * are holding rather than inferring it from arithmetic.
   */
  private latchScrollSource(source: number): number {
    const open = this.scrollSource;
    if (
      open === null ||
      open === AXIS_SOURCE_WHEEL ||
      source === AXIS_SOURCE_FINGER
    ) {
      this.scrollSource = source;
      return source;
    }
    return open;
  }

  private handleWheel(e: WheelEvent): void {
    // No display size means a thumbnail rather than a live view, and
    // those take no other input either. Claiming the wheel there would
    // scroll an app the user is only previewing, and the preventDefault
    // below would stop the page scrolling under the cursor.
    const conn = this.getConn();
    if (!conn || !this.surface || !this._displaySize) return;
    // Ctrl+wheel is how browsers report a pinch-zoom gesture, including
    // macOS trackpad pinches. It is a zoom request, not a scroll; sending
    // it on would scroll the surface while the user pinches.
    if (e.ctrlKey) return;
    const g = this.drawnGeometry();
    if (!g) return;
    e.preventDefault();
    // Alt+scroll is a real chord (horizontal scroll, zoom in some apps):
    // a held-back Alt press belongs ahead of the axis events.  No-op when
    // no Alt press is pending dead-key detection.
    this.flushPendingAlt(conn);

    // The latch has to win before the detent maths below, not just when
    // labelling the source, or a smooth event ends up carrying notches.
    const source = this.latchScrollSource(this.wheelAxisSource(e));
    const notched = source === AXIS_SOURCE_WHEEL;
    let { deltaX, deltaY } = e;
    let v120x = 0;
    let v120y = 0;

    if (e.deltaMode === WHEEL_MODE_LINE) {
      if (notched) {
        v120x = (deltaX / WHEEL_LINES_PER_DETENT) * 120;
        v120y = (deltaY / WHEEL_LINES_PER_DETENT) * 120;
      }
      deltaX *= WHEEL_LINE_PX;
      deltaY *= WHEEL_LINE_PX;
    } else if (e.deltaMode === WHEEL_MODE_PAGE) {
      if (notched) {
        v120x = deltaX * 120;
        v120y = deltaY * 120;
      }
      deltaX *= g.dw;
      deltaY *= g.dh;
    } else if (notched) {
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
      source,
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
    source: number;
  }): void {
    this.latchScrollSource(part.source);
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
      source: this.scrollSource ?? AXIS_SOURCE_CONTINUOUS,
      stop: false,
    });
  }

  /**
   * Close the sequence, and tell the client the gesture is over if a
   * finger was what drove it.
   *
   * A lifted finger is a real event with a real moment, and the toolkits
   * that fling do it off this: a flick on a touchscreen should coast,
   * and without a stop it never would.
   *
   * Nothing else gets one. `axis_stop` is what a toolkit regresses a
   * fling velocity from — Chromium starts one off any stop it can find
   * recent frames behind — and every other sequence we send arrived as
   * browser wheel events, which already carry whatever momentum the
   * platform decided they deserved. A stop there would be asking for a
   * second helping of it, which is exactly what a mouse wheel gliding to
   * a halt looks like. The protocol agrees for `wheel` at least: the
   * sequence may or may not be terminated and clients must not rely on
   * it.
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
    const source = this.scrollSource;
    this.scrollSource = null;
    if (!this.scrollSequenceOpen) return;
    this.scrollSequenceOpen = false;
    if (source !== AXIS_SOURCE_FINGER) return;
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
      // A macOS dead key (Option+E → ´) means the Alt press held back below
      // is part of a character composition, not a modifier chord — drop it
      // so the app never sees it (and ignore its key-up later).
      for (const kc of this.pendingAlt) this.swallowedAlt.add(kc);
      this.pendingAlt.clear();
      if (this.textInput) {
        this.textInput.focus();
      }
      return;
    }

    // Soft-keyboard synthesized keydowns (keyCode 229) name neither key nor
    // code — the text arrives as an input event on the hidden textarea
    // instead.  The evdev path below would send nothing for them anyway, and
    // its preventDefault can cancel that input event, so step aside.
    if (
      (e.key === "Unidentified" || e.key === "Process") &&
      domKeyToEvdev(e.code) === 0
    )
      return;

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

    // macOS Option as a character modifier, no dead key involved: the
    // browser resolves Option+F to "ƒ", Option+G to "©", and reports a
    // single printable (non-ASCII) key with altKey set.  That is text,
    // not an Alt chord — and the Alt press held back below belongs to the
    // character the way a dead key's does.  Gated to macOS: on other
    // platforms Alt is a pure modifier, and on national layouts where a
    // base key is non-ASCII (e.g. Alt+ä on a German layout) this same
    // event shape is a real Meta chord that must reach the app as keys.
    if (
      pressed &&
      this.macOptionChars &&
      e.altKey &&
      !e.ctrlKey &&
      !e.metaKey &&
      e.key.length === 1 &&
      e.key.charCodeAt(0) > 127
    ) {
      for (const kc of this.pendingAlt) this.swallowedAlt.add(kc);
      this.pendingAlt.clear();
      conn.sendSurfaceText(this._surfaceId, e.key);
      return;
    }

    // Hold back the Alt press until the next event shows whether it starts
    // a dead-key composition (handled above) or a real modifier chord.
    // Only on macOS, where Option is a character modifier — elsewhere Alt
    // is forwarded immediately so apps see Alt-hold and Alt-tap as usual.
    const altKeycode = domKeyToEvdev(e.code);
    if (this.macOptionChars && (altKeycode === 56 || altKeycode === 100)) {
      if (pressed) {
        this.pendingAlt.add(altKeycode);
      } else if (this.pendingAlt.delete(altKeycode)) {
        // Bare Alt tap: deliver press+release together, as a native
        // compositor would.
        conn.sendSurfaceInput(this._surfaceId, altKeycode, true);
        conn.sendSurfaceInput(this._surfaceId, altKeycode, false);
      } else if (this.swallowedAlt.delete(altKeycode)) {
        // Consumed by a dead-key composition: never pressed, never released.
      } else if (this.pressedKeys.delete(altKeycode)) {
        // Forwarded as part of a chord — release it.
        conn.sendSurfaceInput(this._surfaceId, altKeycode, false);
      }
      return;
    }
    this.flushPendingAlt(conn);
    if (pressed && e.altKey && this.swallowedAlt.size !== 0) {
      // A dead-key composition was abandoned while Option is still held
      // (and this keydown is no composition): put Alt back so the app sees
      // a consistent modifier for this chord.
      for (const kc of this.swallowedAlt) {
        this.pressedKeys.add(kc);
        conn.sendSurfaceInput(this._surfaceId, kc, true);
      }
      this.swallowedAlt.clear();
    }

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
      };
      this._pendingPasteTimer = setTimeout(
        this._pendingPasteAbandon,
        PASTE_READ_MS,
      );

      // `navigator.clipboard.readText()` is often denied without an
      // explicit user-granted permission, and the `paste` event that backs
      // it up only fires reliably on an editable element — Chromium/Brave
      // do not dispatch it to a focused canvas.  Focus normally rests on
      // the textarea already; this is the belt for a view that somehow
      // left it on the canvas.  handleBlur ignores the transient blur via
      // the `_pendingPaste` check above.
      if (this.textInput) this.textInput.focus({ preventScroll: true });

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

  /** Forward any Alt presses held back for dead-key detection, ahead of
   *  the event that proves they are a real modifier chord. */
  private flushPendingAlt(conn: BlitConnection): void {
    if (this.pendingAlt.size === 0) return;
    for (const kc of this.pendingAlt) {
      this.pressedKeys.add(kc);
      conn.sendSurfaceInput(this._surfaceId, kc, true);
    }
    this.pendingAlt.clear();
  }

  /** Handle text input from the hidden textarea. */
  private handleTextInput(e: InputEvent): void {
    const ta = this.textInput;
    // A composition in progress goes out as a preedit, so the app can draw
    // it: the textarea capturing it is 1px and transparent, so this is the
    // only place the pending text becomes legible.  Reported from `input`
    // rather than `compositionupdate` because that one fires *before* the
    // DOM is updated — the caret read there is the previous one, which put
    // the app's cursor at 0 for every composition.
    if (e.isComposing) {
      const conn = this.getConn();
      if (conn && this.surface && this._displaySize && ta) {
        conn.sendSurfacePreedit(this._surfaceId, ta.value, ta.selectionStart);
      }
      return;
    }
    // Any keydown handleKey processed was preventDefault'ed, which cancels
    // its input event — so what reaches here is text the keyboard delivered
    // *without* a usable keydown: soft-keyboard commits (keyCode 229),
    // suggestion taps, autocorrect, and IMEs that delete or break lines via
    // input events alone.  Everything else (insertFromPaste, and Firefox's
    // post-compositionend insertCompositionText, which handleCompositionEnd
    // already sent) stays ignored.
    const conn = this.getConn();
    if (conn && this.surface && this._displaySize) {
      if (e.inputType === "insertText" && e.data) {
        conn.sendSurfaceText(this._surfaceId, e.data);
      } else if (e.inputType === "insertLineBreak") {
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, false);
      } else if (e.inputType === "deleteContentBackward") {
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Backspace, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Backspace, false);
      }
    }
    if (ta) ta.value = "";
  }

  /** Handle IME composition end — send the composed text. */
  private handleCompositionEnd(e: CompositionEvent): void {
    const ta = this.textInput;
    if (!ta) return;
    const conn = this.getConn();
    if (e.data) {
      if (conn && this.surface) {
        conn.sendSurfaceText(this._surfaceId, e.data);
      }
    } else if (conn && this.surface && this._displaySize) {
      // Cancelled: nothing to commit, so nothing else will take back the
      // preedit still on screen.
      conn.sendSurfacePreedit(this._surfaceId, "", 0);
    }
    ta.value = "";
    // Focus stays here.  Handing it back to the canvas would end the next
    // composition before it started, and the keydown/keyup handlers the
    // canvas would take back are already attached to this element.
  }

  /** Send synthetic key-up for every key still held.  Prevents stuck
   *  modifiers and runaway key-repeat when focus leaves the canvas. */
  private releaseAllKeys(): void {
    this._pendingPaste = null;
    this._pendingPasteFlush = null;
    this._ctrlReleaseDeferred = false;
    this._metaToCtrlKey = 0;
    // Held-back and swallowed Alt presses never reached the compositor, so
    // they need no release — only forgetting.
    this.pendingAlt.clear();
    this.swallowedAlt.clear();
    if (this.pressedKeys.size === 0) return;
    const conn = this.getConn();
    if (!conn || !this.surface) return;
    for (const kc of this.pressedKeys) {
      conn.sendSurfaceInput(this._surfaceId, kc, false);
    }
    this.pressedKeys.clear();
    this._metaToCtrl = 0;
  }

  private handleBlur(e: FocusEvent): void {
    // Focus shuffling between the canvas and its own IME textarea (paste,
    // composition, the mobile keyboard parking on the textarea) never
    // means the user left the surface — releasing held keys there sends
    // phantom key-ups, e.g. a V-up while the paste chord's V is still
    // physically down.
    const to = e.relatedTarget;
    if (to && (to === this.canvas || to === this.textInput)) return;
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

  private handleFocus(e: FocusEvent): void {
    // Focus that lands on the canvas is handed straight to the textarea.
    // An input method only engages for an editable element, and a canvas is
    // not one: while focus rests there the browser fires no composition
    // events at all, so a composition never starts and everything an IME
    // exists to produce is never typed.  Focus arrives here from outside
    // this component too (a pane taking focus, Tab), which is why the
    // handoff lives on the event rather than only at our own call sites.
    if (e.target === this.canvas && this.textInput && this._displaySize) {
      // The textarea is a 1px box in the corner of the container; scrolling
      // the pane to it would be a visible jump for an invisible element.
      this.textInput.focus({ preventScroll: true });
      // Its own focus event sends the surface focus — one message, not two.
      return;
    }
    const conn = this.getConn();
    if (!conn || !this.surface || !this._displaySize) return;
    conn.sendSurfaceFocus(this._surfaceId);
  }
}
