import type { ConnectionId, BlitSurface } from "./types";
import type { FsSyncHandle } from "./fs";
import {
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H264_444,
  CODEC_SUPPORT_AV1_444,
  AXIS_SOURCE_CONTINUOUS,
  AXIS_SOURCE_FINGER,
  AXIS_SOURCE_WHEEL,
  SURFACE_TOUCH_DOWN,
  SURFACE_TOUCH_UP,
  SURFACE_TOUCH_MOTION,
  SURFACE_TOUCH_CANCEL,
} from "./types";
import type { BlitWorkspace } from "./BlitWorkspace";
import type { BlitConnection } from "./BlitConnection";
import type {
  RemoteSurfaceInput,
  SurfaceCursorImage,
  SurfaceTextInputEvent,
} from "./SurfaceStore";
import {
  SURFACE_POINTER_DOWN,
  SURFACE_POINTER_UP,
  SURFACE_POINTER_MOVE,
  SURFACE_POINTER_LEAVE,
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

/** Radius of a mirrored touch contact, in logical pixels — about a fingertip. */
const REMOTE_CONTACT_RADIUS = 14;

const REMOTE_CURSOR_ARROW =
  "M 0.75 0.75 L 0.75 20 L 6 14.75 L 10.5 23 L 14 21 L 9.5 13 L 17 13 Z";
const REMOTE_CURSOR_HAND =
  "M 0 0 C -1 -3 1 -5 3 -5 C 5 -5 6 -3 6 -1 L 6 8 L 8 8 L 8 3 C 8 1 11 1 11 3 L 11 8 L 13 8 L 13 4 C 13 2 16 2 16 4 L 16 9 L 18 9 L 18 6 C 18 4 21 4 21 6 L 21 14 C 21 21 17 25 10 25 C 6 25 3 22 1 19 L -3 13 C -4 11 -3 9 -1 8 C 1 8 3 11 3 11 L 3 0 Z";
const REMOTE_CURSOR_GRAB =
  "M -10 -2 C -10 -5 -6 -6 -5 -3 L -5 -8 C -5 -11 -1 -11 0 -8 C 1 -12 5 -11 5 -8 C 7 -10 10 -8 10 -5 L 10 5 C 10 11 6 14 0 14 C -5 14 -9 10 -11 6 L -14 1 C -15 -2 -12 -4 -10 -2 Z";
const REMOTE_CURSOR_TEXT =
  "M -7 -12 H 7 V -9 H 2 V 9 H 7 V 12 H -7 V 9 H -2 V -9 H -7 Z";
const REMOTE_CURSOR_CROSSHAIR =
  "M -1 -12 H 1 V -2 H 12 V 2 H 1 V 12 H -1 V 2 H -12 V -2 H -1 Z";
const REMOTE_CURSOR_EW =
  "M -12 0 L -6 -6 V -2 H 6 V -6 L 12 0 L 6 6 V 2 H -6 V 6 Z";
const REMOTE_CURSOR_NS =
  "M 0 -12 L 6 -6 H 2 V 6 H 6 L 0 12 L -6 6 H -2 V -6 H -6 Z";
const REMOTE_CURSOR_MOVE =
  "M 0 -13 L 5 -8 H 2 V -2 H 8 V -5 L 13 0 L 8 5 V 2 H 2 V 8 H 5 L 0 13 L -5 8 H -2 V 2 H -8 V 5 L -13 0 L -8 -5 V -2 H -2 V -8 H -5 Z";
const REMOTE_CURSOR_PROHIBITED =
  "M 0 -12 A 12 12 0 1 1 0 12 A 12 12 0 0 1 0 -12 Z M -6 -8 L 8 6 L 6 8 L -8 -6 Z";
const REMOTE_CURSOR_WAIT =
  "M 0 -12 A 12 12 0 1 1 -12 0 H -8 A 8 8 0 1 0 0 -8 Z";
const REMOTE_CURSOR_ZOOM =
  "M -3 -11 A 8 8 0 1 1 -3 5 A 8 8 0 0 1 -3 -11 Z M -3 -7 A 4 4 0 1 0 -3 1 A 4 4 0 0 0 -3 -7 Z M 3 3 L 12 12 L 9 15 L 0 6 Z";
const REMOTE_CURSOR_CONTEXT_MENU = `${REMOTE_CURSOR_ARROW} M 12 16 H 22 V 18 H 12 Z M 12 20 H 22 V 22 H 12 Z`;
const REMOTE_CURSOR_HELP = `${REMOTE_CURSOR_ARROW} M 13 15 C 13 11 21 11 21 16 C 21 19 18 19 18 21 H 15 C 15 17 18 17 18 15 C 18 13 16 13 16 15 Z M 15 23 H 18 V 26 H 15 Z`;
const REMOTE_CURSOR_COPY = `${REMOTE_CURSOR_ARROW} M 13 16 H 17 V 12 H 20 V 16 H 24 V 19 H 20 V 23 H 17 V 19 H 13 Z`;
const REMOTE_CURSOR_ALIAS = `${REMOTE_CURSOR_ARROW} M 13 19 H 18 V 16 L 24 21 L 18 26 V 23 H 13 Z`;
const REMOTE_CURSOR_PROGRESS = `${REMOTE_CURSOR_ARROW} M 18 12 A 6 6 0 1 1 12 18 H 15 A 3 3 0 1 0 18 15 Z`;
const REMOTE_CURSOR_ZOOM_IN = `${REMOTE_CURSOR_ZOOM} M -6 -4 H -4 V -6 H -2 V -4 H 0 V -2 H -2 V 0 H -4 V -2 H -6 Z`;
const REMOTE_CURSOR_ZOOM_OUT = `${REMOTE_CURSOR_ZOOM} M -6 -4 H 0 V -2 H -6 Z`;

interface RemoteCursorGlyph {
  path: string;
  rotation?: number;
}

/** A compact, high-contrast approximation of the platform cursor shape. */
function remoteCursorGlyph(name: string): RemoteCursorGlyph {
  switch (name) {
    case "context-menu":
      return { path: REMOTE_CURSOR_CONTEXT_MENU };
    case "help":
      return { path: REMOTE_CURSOR_HELP };
    case "pointer":
      return { path: REMOTE_CURSOR_HAND };
    case "alias":
      return { path: REMOTE_CURSOR_ALIAS };
    case "copy":
      return { path: REMOTE_CURSOR_COPY };
    case "grab":
    case "grabbing":
      return { path: REMOTE_CURSOR_GRAB };
    case "text":
      return { path: REMOTE_CURSOR_TEXT };
    case "vertical-text":
      return { path: REMOTE_CURSOR_TEXT, rotation: 90 };
    case "cell":
    case "crosshair":
      return { path: REMOTE_CURSOR_CROSSHAIR };
    case "e-resize":
    case "w-resize":
    case "ew-resize":
    case "col-resize":
      return { path: REMOTE_CURSOR_EW };
    case "n-resize":
    case "s-resize":
    case "ns-resize":
    case "row-resize":
      return { path: REMOTE_CURSOR_NS };
    case "ne-resize":
    case "sw-resize":
    case "nesw-resize":
      return { path: REMOTE_CURSOR_EW, rotation: -45 };
    case "nw-resize":
    case "se-resize":
    case "nwse-resize":
      return { path: REMOTE_CURSOR_EW, rotation: 45 };
    case "move":
    case "all-scroll":
      return { path: REMOTE_CURSOR_MOVE };
    case "no-drop":
    case "not-allowed":
      return { path: REMOTE_CURSOR_PROHIBITED };
    case "wait":
      return { path: REMOTE_CURSOR_WAIT };
    case "progress":
      return { path: REMOTE_CURSOR_PROGRESS };
    case "zoom-in":
      return { path: REMOTE_CURSOR_ZOOM_IN };
    case "zoom-out":
      return { path: REMOTE_CURSOR_ZOOM_OUT };
    default:
      return { path: REMOTE_CURSOR_ARROW };
  }
}

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

/** Android soft keyboards can identify Return without supplying a physical
 *  `code`.  It is an action key in that case, so its logical `key` is enough. */
function isEnterKeyEvent(e: KeyboardEvent): boolean {
  return (
    e.key === "Enter" ||
    e.key === "Return" ||
    e.code === "Enter" ||
    e.code === "NumpadEnter" ||
    e.keyCode === 13
  );
}

/** Chromium-family keyboards use several equivalent shapes for Return. */
function isEnterInputEvent(e: InputEvent): boolean {
  return (
    e.inputType === "insertLineBreak" ||
    e.inputType === "insertParagraph" ||
    e.data === "\n" ||
    e.data === "\r"
  );
}

/** Recover a physical DOM code when a soft keyboard supplied only text. */
function domCodeForCharacter(key: string): string {
  if (/^[a-z]$/i.test(key)) return `Key${key.toUpperCase()}`;
  if (/^[0-9]$/.test(key)) return `Digit${key}`;
  return (
    (
      {
        " ": "Space",
        "!": "Digit1",
        "@": "Digit2",
        "#": "Digit3",
        $: "Digit4",
        "%": "Digit5",
        "^": "Digit6",
        "&": "Digit7",
        "*": "Digit8",
        "(": "Digit9",
        ")": "Digit0",
        "-": "Minus",
        _: "Minus",
        "=": "Equal",
        "+": "Equal",
        "[": "BracketLeft",
        "{": "BracketLeft",
        "]": "BracketRight",
        "}": "BracketRight",
        ";": "Semicolon",
        ":": "Semicolon",
        "'": "Quote",
        '"': "Quote",
        "`": "Backquote",
        "~": "Backquote",
        "\\": "Backslash",
        "|": "Backslash",
        ",": "Comma",
        "<": "Comma",
        ".": "Period",
        ">": "Period",
        "/": "Slash",
        "?": "Slash",
      } as Record<string, string>
    )[key] ?? ""
  );
}

/** Recover a DOM code from the logical key when a virtual key has no physical
 *  code. Named action keys share their names with EVDEV_MAP entries. */
function domCodeForLogicalKey(key: string): string {
  if (key === "Return") return "Enter";
  if (domKeyToEvdev(key) !== 0) return key;
  return domCodeForCharacter(key);
}

function characterNeedsShift(key: string): boolean {
  return /^[A-Z]$/.test(key) || '~!@#$%^&*()_+{}|:"<>?'.includes(key);
}

/** Modifier keys must stay held across a chord; every other Cmd-chord key can
 *  be released with its press on macOS, where the browser may eat its keyup. */
const EVDEV_MODIFIERS = new Set([29, 42, 54, 56, 97, 100, 125, 126]);

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

/** iOS/iPadOS only auto-repeats a soft-keyboard Backspace while the focused
 * editable element still contains text it can delete. */
function detectIOS(): boolean {
  if (typeof navigator === "undefined") return false;
  return (
    /iPad|iPhone|iPod/.test(navigator.platform) ||
    (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1)
  );
}

const IOS_INPUT_PAD_CODE = 0x00a0;
// Replacing the field while Backspace is held resets WebKit's native repeat
// cadence.  Keep enough filler that the idle repad is the only refill a human
// hold can reach; 4096 characters lasts for minutes at iOS repeat rates.
const IOS_INPUT_PAD = String.fromCharCode(IOS_INPUT_PAD_CODE).repeat(4096);

function stripIOSInputPad(value: string): string {
  let i = 0;
  while (i < value.length && value.charCodeAt(i) === IOS_INPUT_PAD_CODE) i++;
  return value.slice(i);
}

// ---------------------------------------------------------------------------
// BlitSurfaceCanvas
// ---------------------------------------------------------------------------

/** Mounted live canvases, used to hand an in-progress physical mouse grab
 * across DOM elements.  Browser mouse capture is not consistent across a
 * canvas boundary while a remote app is drawing its own drag icon; routing
 * the window event by hit-test keeps the compositor's drag over the surface
 * actually under the pointer. */
const mountedSurfaceCanvases = new Map<HTMLCanvasElement, BlitSurfaceCanvas>();
const surfaceCanvasByInput = new WeakMap<
  HTMLTextAreaElement,
  BlitSurfaceCanvas
>();

/** Bubbling DOM event emitted by a mounted surface when its Wayland client
 * commits text-input state. The app shell uses fresh `requested` events to
 * raise a mobile virtual keyboard; embedders can provide their own policy. */
export const BLIT_SURFACE_TEXT_INPUT_EVENT = "blit-surface-text-input";

export type BlitSurfaceTextInputEvent = CustomEvent<SurfaceTextInputEvent>;

function inputModeForContentPurpose(purpose: number): string {
  switch (purpose) {
    case 2: // digits
      return "numeric";
    case 3: // number
      return "decimal";
    case 4: // phone
      return "tel";
    case 5: // url
      return "url";
    case 6: // email
      return "email";
    case 9: // pin
      return "numeric";
    default:
      return "text";
  }
}

/** Resolve the live Wayland surface view owning a hidden IME textarea. */
export function surfaceCanvasForInput(
  input: Element | null,
): BlitSurfaceCanvas | null {
  return input instanceof HTMLTextAreaElement
    ? (surfaceCanvasByInput.get(input) ?? null)
    : null;
}

const routedGrabMouseEvents = new WeakSet<Event>();
let activeSurfaceMouseGrab: {
  owner: BlitSurfaceCanvas;
  buttons: Set<number>;
} | null = null;

export interface BlitSurfaceCanvasOptions {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  surfaceId: number;
  /**
   * Whether this mount is expected to drive the surface size.  This lets an
   * already-laid-out passive view put its scaled target on the very first
   * subscribe, while preserving the eager unscaled subscribe for a pane
   * whose framework binding is about to call `setDisplaySize`.
   */
  resizable?: boolean;
  /**
   * Whether this mount owns a server-side video subscription.  Cached-only
   * mounts still paint frames produced by another live view from the shared
   * SurfaceStore, but never create an encoder themselves.  Defaults to true.
   */
  live?: boolean;
  /** `direct` (the default) forwards every touchscreen contact to the Wayland
   * client's `wl_touch`. `pointer` opts into Blit's single-finger
   * click/scroll emulation. */
  touchMode?: SurfaceTouchMode;
}

export type SurfaceTouchMode = "pointer" | "direct";

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
 * Largest inline drop payload we will put on the wire.
 *
 * `C2S_SURFACE_DRAG_DROP` rides a single transport frame — there is no
 * fragmentation on the C2S path — and the frame ceiling is 16 MiB; a frame
 * over it is refused, not truncated.  Stay a full 1 MiB under the ceiling
 * to cover the MIME/name/framing overhead around the payload bytes.  Only
 * name-less items (dragged text) still ride the frame inline: dropped
 * FILES are staged through the chunked FS upload pump and the DROP names
 * them, so they have no size cap here.
 */
const MAX_DND_BYTES = 15 * 1024 * 1024;

/** The most recently entered canvas for a connection.  A WebKit drop can
 *  be retargeted from the canvas to the document, while a stale mount may
 *  still think it is active because DOM enter precedes leave. */
const activeBrowserDragCanvas = new WeakMap<
  BlitConnection,
  BlitSurfaceCanvas
>();

/** One DROP is observed by both the window fallback and canvas. */
const DROP_CLAIMED = Symbol("blit-surface-drop-claimed");

/** One known image MIME per file-kind drag item, in item order — readable
 *  during hover, unlike the files themselves.  The plan is all-or-nothing:
 *  a typeless/unknown WebKit item would commit its visible URI to `.bin`, so
 *  omit the trailer and derive a useful name once the File materializes.
 *
 *  iPad screenshots are the exception: WebKit exposes only the `Files`
 *  marker during hover, then materializes the real representation at DROP.
 *  Without an item plan the compositor must park Chromium's eager URI read,
 *  so the remote page does not receive dragenter until after release.  Plan
 *  that screenshot as PNG: HEIC/HEIF is converted at DROP, and announcing
 *  the final path during hover gives Chromium time to deliver a file-shaped
 *  drag to the remote page before the release arrives. */
function dragFileItemMimes(dt: DataTransfer | null): string[] | undefined {
  if (!dt) return undefined;
  const items = Array.from(dt.items ?? []).filter(
    (item) => item.kind === "file",
  );
  const mimes: string[] = [];
  for (const item of items) {
    const mime = normalizedMime(item.type);
    if (!plannedDropExtension(mime)) {
      return isIPadOS() && items.length === 1 ? ["image/png"] : undefined;
    }
    mimes.push(mime);
  }
  if (mimes.length > 0) return mimes;
  return isIPadOS() && Array.from(dt.types ?? []).includes("Files")
    ? ["image/png"]
    : undefined;
}

/** Modern iPadOS identifies Safari as MacIntel; touch-point count separates
 *  it from desktop Safari without relying on the mutable user-agent string. */
function isIPadOS(): boolean {
  const platform = navigator.platform ?? "";
  return (
    /^iPad$/i.test(platform) ||
    (platform === "MacIntel" && navigator.maxTouchPoints > 1)
  );
}

/**
 * The MIME list to offer the compositor for a drag, or null when the drag
 * is none of ours.  OS file drags usually list "Files" among the types —
 * but macOS file promises (the screenshot's floating thumbnail) can arrive
 * with only a file-kind item and no "Files" type, so items count too.  File
 * drags are offered as a URI list with a raw-bytes fallback; a text drag as
 * plain text.  Anything else — pane/tile moves carry only custom MIMEs — is
 * an internal UI drag and must pass through untouched.
 */
function dragOfferMimes(dt: DataTransfer | null): string[] | null {
  if (!dt) return null;
  const types = Array.from(dt.types ?? []);
  if (dragHasFiles(dt)) return ["text/uri-list", "application/octet-stream"];
  if (types.includes("text/plain"))
    return ["text/plain;charset=utf-8", "text/plain"];
  return null;
}

/** True when the transfer exposes a file through any browser API.  WebKit
 *  can keep `items` empty during hover and expose only `files` at DROP, so
 *  no one collection is authoritative for the whole gesture. */
function dragHasFiles(dt: DataTransfer): boolean {
  return (
    Array.from(dt.types ?? []).includes("Files") ||
    (dt.files?.length ?? 0) > 0 ||
    dragHasFileItem(dt)
  );
}

/** True when any drag item is a file (available even when types omit
 *  "Files", as with macOS file-promise drags). */
function dragHasFileItem(dt: DataTransfer): boolean {
  for (const item of Array.from(dt.items ?? [])) {
    if (item.kind === "file") return true;
  }
  return false;
}

/** The files a drop carries, from `files` or — macOS file promises again —
 *  from file-kind items when `files` is empty. */
function droppedFiles(dt: DataTransfer): File[] {
  if (dt.files.length > 0) return Array.from(dt.files);
  const files: File[] = [];
  for (const item of Array.from(dt.items ?? [])) {
    if (item.kind !== "file") continue;
    const file = item.getAsFile();
    if (file) files.push(file);
  }
  return files;
}

function normalizedMime(mime: string): string {
  return mime.split(";", 1)[0].trim().toLowerCase();
}

/** Extensions both protocol peers can derive from hover-time MIME alone. */
function plannedDropExtension(mime: string): string | null {
  switch (normalizedMime(mime)) {
    case "image/png":
      return "png";
    case "image/jpeg":
      return "jpg";
    case "image/webp":
      return "webp";
    case "image/gif":
      return "gif";
    case "image/avif":
      return "avif";
    case "image/heic":
      return "heic";
    case "image/heif":
      return "heif";
    case "image/tiff":
      return "tiff";
    case "image/bmp":
      return "bmp";
    default:
      return null;
  }
}

function mimeFromDropName(name: string): string | null {
  const dot = name.lastIndexOf(".");
  const ext = dot >= 0 ? name.slice(dot + 1).toLowerCase() : "";
  switch (ext) {
    case "png":
      return "image/png";
    case "jpg":
    case "jpeg":
      return "image/jpeg";
    case "webp":
      return "image/webp";
    case "gif":
      return "image/gif";
    case "avif":
      return "image/avif";
    case "heic":
      return "image/heic";
    case "heif":
      return "image/heif";
    case "tif":
    case "tiff":
      return "image/tiff";
    case "bmp":
      return "image/bmp";
    default:
      return null;
  }
}

/** Determine the representation WebKit finally materialized.  A promised
 *  iPad image can have neither a useful MIME nor a name, so fall back to the
 *  file signature instead of assigning an extension by assumption. */
function materializedDropMimeFromMetadata(file: File): string | null {
  const declared = normalizedMime(file.type);
  if (plannedDropExtension(declared)) return declared;
  const named = mimeFromDropName(file.name);
  if (named) return named;
  if (declared && !declared.startsWith("image/")) return declared;
  const dot = file.name.lastIndexOf(".");
  if (dot >= 0 && /^[a-z0-9]{1,10}$/i.test(file.name.slice(dot + 1)))
    return declared || "application/octet-stream";
  return null;
}

async function materializedDropMime(file: File): Promise<string> {
  const declared = normalizedMime(file.type);
  const known = materializedDropMimeFromMetadata(file);
  if (known) return known;

  const bytes = new Uint8Array(await file.slice(0, 64).arrayBuffer());
  if (
    bytes.length >= 8 &&
    bytes[0] === 0x89 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x4e &&
    bytes[3] === 0x47 &&
    bytes[4] === 0x0d &&
    bytes[5] === 0x0a &&
    bytes[6] === 0x1a &&
    bytes[7] === 0x0a
  )
    return "image/png";
  if (
    bytes.length >= 3 &&
    bytes[0] === 0xff &&
    bytes[1] === 0xd8 &&
    bytes[2] === 0xff
  )
    return "image/jpeg";
  const ascii = (start: number, end: number) =>
    String.fromCharCode(...bytes.subarray(start, end));
  if (
    bytes.length >= 6 &&
    (ascii(0, 6) === "GIF87a" || ascii(0, 6) === "GIF89a")
  )
    return "image/gif";
  if (bytes.length >= 12 && ascii(0, 4) === "RIFF" && ascii(8, 12) === "WEBP")
    return "image/webp";
  if (
    bytes.length >= 4 &&
    ((bytes[0] === 0x49 &&
      bytes[1] === 0x49 &&
      bytes[2] === 0x2a &&
      bytes[3] === 0) ||
      (bytes[0] === 0x4d &&
        bytes[1] === 0x4d &&
        bytes[2] === 0 &&
        bytes[3] === 0x2a))
  )
    return "image/tiff";
  if (bytes.length >= 2 && ascii(0, 2) === "BM") return "image/bmp";
  if (bytes.length >= 12 && ascii(4, 8) === "ftyp") {
    const brands: string[] = [];
    for (let offset = 8; offset + 4 <= bytes.length; offset += 4)
      brands.push(ascii(offset, offset + 4));
    if (brands.some((brand) => brand === "avif" || brand === "avis"))
      return "image/avif";
    if (
      brands.some((brand) =>
        ["heic", "heix", "hevc", "hevx", "heim", "heis"].includes(brand),
      )
    )
      return "image/heic";
    if (brands.some((brand) => brand === "mif1" || brand === "msf1"))
      return "image/heif";
  }
  return declared || "application/octet-stream";
}

/** Convert the HEIC/HEIF representation iPadOS supplies for a screenshot to
 *  PNG before it crosses the Wayland boundary.  Chromium-backed destinations
 *  commonly leave those image types unclaimed and navigate to the staged file
 *  instead of treating it as an upload.  The source browser can decode the
 *  representation, so make the compatibility conversion there.  A failed
 *  conversion is non-fatal: preserving the original file is preferable to
 *  cancelling the drop. */
async function compatibleIPadDropFile(
  file: File,
  mime: string,
): Promise<{ file: File; mime: string }> {
  if (
    !isIPadOS() ||
    (normalizedMime(mime) !== "image/heic" &&
      normalizedMime(mime) !== "image/heif")
  )
    return { file, mime };

  try {
    const bitmap = await createImageBitmap(file);
    try {
      if (bitmap.width < 1 || bitmap.height < 1)
        throw new Error("decoded image has no pixels");
      const canvas = document.createElement("canvas");
      canvas.width = bitmap.width;
      canvas.height = bitmap.height;
      const context = canvas.getContext("2d");
      if (!context) throw new Error("2D canvas is unavailable");
      context.drawImage(bitmap, 0, 0);
      const png = await new Promise<Blob>((resolve, reject) => {
        canvas.toBlob(
          (blob) =>
            blob
              ? resolve(blob)
              : reject(new Error("canvas returned no PNG data")),
          "image/png",
        );
      });
      const stem = file.name.replace(/\.(?:heic|heif)$/i, "") || "screenshot";
      return {
        file: new File([png], `${stem}.png`, {
          type: "image/png",
          lastModified: file.lastModified,
        }),
        mime: "image/png",
      };
    } finally {
      bitmap.close();
    }
  } catch (err) {
    console.warn(
      "blit: could not convert an iPad HEIC/HEIF drop to PNG; dropping the original",
      err,
    );
    return { file, mime };
  }
}

/** The staging path for a hover plan, derived identically on the server. */
function plannedDropName(mime: string, index: number): string {
  const ext = plannedDropExtension(mime) ?? "bin";
  return `${index}.${ext}`;
}

/** Name an item that had no hover plan.  At DROP the browser may finally
 *  reveal a MIME and filename; prefer a known image MIME, then retain a safe
 *  filename extension (the iPad screenshot path), then use a simple image
 *  subtype before conceding `.bin`. */
function materializedDropName(file: File, index: number): string {
  let ext = plannedDropExtension(file.type);
  if (!ext) {
    const dot = file.name.lastIndexOf(".");
    const candidate = dot >= 0 ? file.name.slice(dot + 1).toLowerCase() : "";
    if (/^[a-z0-9]{1,10}$/.test(candidate)) ext = candidate;
  }
  if (!ext) {
    const mime = normalizedMime(file.type);
    if (mime.startsWith("image/")) {
      const subtype = mime.slice("image/".length);
      const candidate =
        subtype === "svg+xml"
          ? "svg"
          : subtype === "x-icon" || subtype === "vnd.microsoft.icon"
            ? "ico"
            : subtype;
      if (/^[a-z0-9]{1,10}$/.test(candidate)) ext = candidate;
    }
  }
  return `${index}.${ext ?? "bin"}`;
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
  /** Live previews do not need to drive applications at monitor refresh. */
  private static readonly THUMBNAIL_MAX_FPS = 15;
  private _workspace: BlitWorkspace;
  private _connectionId: ConnectionId;
  private _surfaceId: number;
  private _live: boolean;
  private _expectsDisplaySize: boolean;
  private _touchMode: SurfaceTouchMode;
  private touchCapabilityAcquired = false;
  /**
   * A passive view with a working ResizeObserver must learn its box before
   * it opens a stream. A newly-created sidebar card is mounted before its
   * first layout and therefore measures 0x0 synchronously; subscribing at
   * that point asks for native pixels, only to replace the request with an
   * octave-rounded thumbnail target in the observer callback.
   */
  private _waitForPresentBox = false;

  private container: HTMLElement | null = null;
  private canvas: HTMLCanvasElement | null = null;
  private ctx: CanvasRenderingContext2D | null = null;
  /** Pointer overlay for the client currently driving this shared surface.
   *  The originating client is told to hide it and keeps its native cursor. */
  private remotePointerSvg: SVGSVGElement | null = null;
  private remotePointerGlyph: SVGPathElement | null = null;
  private remotePointerImage: SVGImageElement | null = null;
  private remoteInput: RemoteSurfaceInput | null = null;
  /** Reused rings for mirrored touch contacts. */
  private remoteContacts: SVGCircleElement[] = [];
  private remoteCursor: SurfaceCursorImage = {
    kind: "named",
    name: "default",
  };

  private surface: BlitSurface | undefined;
  private disposed = false;

  /** Track which mouse buttons are currently pressed so we can send synthetic
   *  pointer-up events on dispose — preventing a dangling compositor grab. */
  private pressedButtons = new Set<number>();

  /** Track which keyboard keys are currently pressed (evdev keycodes) so we
   *  can release them when focus leaves or the canvas is disposed — preventing
   *  stuck modifiers and runaway key-repeat in the compositor. */
  private pressedKeys = new Set<number>();

  /** One-shot modifiers armed by the mobile extra-keys row. */
  private _ctrlModifier = false;
  private _ctrlModifierListeners = new Set<(active: boolean) => void>();
  private _altModifier = false;
  private _altModifierListeners = new Set<(active: boolean) => void>();

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

  /** True from compositionstart through compositionend. Some engines report
   *  `KeyboardEvent.isComposing=false` on the keystroke that completes a
   *  dead-key composition; the explicit lifecycle keeps that key on the IME
   *  path instead of forwarding it as ordinary input and cancelling commit. */
  private compositionActive = false;

  /** Active single-finger gesture used to emulate mouse input on iPadOS. */
  private activeTouch: {
    identifier: number;
    startX: number;
    startY: number;
    lastX: number;
    lastY: number;
    mode: "pending" | "held" | "scroll" | "drag";
    longPressTimer: ReturnType<typeof setTimeout> | null;
    pointerId?: number;
  } | null = null;
  /** Browser contact identifiers currently held by direct-touch mode. */
  private directTouchIds = new Set<number>();

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
    /**
     * The container's own device-pixel ratio, in 1/120ths — the ratio that
     * converts this view's device pixels back to CSS pixels.
     *
     * Equal to `scale120` unless the binding selected a relative or exact
     * surface scale that differs from the display DPI. The pane's device↔CSS
     * ratio is unchanged, so the two must not be conflated.
     */
    cssScale120: number;
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
  /** Whether this mount intersects the document viewport.  Hidden BSP
   *  leaves and inactive tabs can remain mounted; keeping their server
   *  subscription alive would make the compositor render and encode a
   *  stream nobody can see. */
  private _isIntersecting = true;
  private _intersectionObserver: IntersectionObserver | null = null;
  /** This view's surface-subscription token.  Allocated lazily and kept
   *  across resubscribes so the connection tracks one entry per view. */
  private _surfaceViewId: string | null = null;
  /** Halvings applied by the last blit, so the observer can tell a resize that
   *  crosses an octave from one that changes nothing on screen. */
  private _presentHalvings = 0;
  /** Source frame size of the last blit, so the observer can recompute the
   *  reduction without going back to the store. */
  private _lastFrameSize: { width: number; height: number } | null = null;
  /** Reused result for presentationBox(); callers consume it synchronously. */
  private readonly _presentationBox = { width: 0, height: 0 };
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
  private unsubRemotePointer: (() => void) | null = null;
  private unsubTextInput: (() => void) | null = null;
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

  /** Hidden textarea used as the editable keyboard and IME target. */
  private textInput: HTMLTextAreaElement | null = null;
  /** Keep the iOS capture field non-empty so a held soft-keyboard Backspace
   *  continues producing deleteContentBackward events. */
  private _iosInputPad = detectIOS();
  /** Refill the iOS pad only after a repeat burst goes idle; changing the
   *  textarea value during the burst can stop WebKit's native repeat. */
  private _iosInputRepadTimer: ReturnType<typeof setTimeout> | null = null;
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
   *  clipboard read completes — readText resolve/reject, clipboard.read,
   *  or the paste event — so the Wayland app sees `selection` before
   *  `key`, and defer the V release and Ctrl release that may fire
   *  physically during that window, otherwise V arrives at the compositor
   *  with Ctrl already released and the app types 'v' repeatedly. */
  private _pendingPaste: {
    keycode: number;
    released: boolean;
    deferredCtrlRelease: boolean;
    /** The chord used Cmd (macOS paste).  Chrome on macOS eats the
     *  key-up of a key that triggered a menu command (Cmd+V → Paste),
     *  so its V release never arrives and cannot be waited for: the
     *  flush sends press and release together.  Cmd chords don't
     *  autorepeat on macOS, so holding changes nothing. */
    metaChord: boolean;
  } | null = null;
  private _pendingPasteFlush:
    | ((payload: ClipboardPayload | null) => void)
    | null = null;
  /** Stand the in-flight chord down without pressing V, releasing anything
   *  the deferral held back.  Runs when the clipboard is known to hold
   *  nothing pastable, when an image we declined is all it held, or when
   *  focus leaves mid-chord.  No timer: every clipboard read is a promise
   *  that settles, and the paste event is dispatched with the keydown, so
   *  the chord's outcome is always decided by an event, never guessed. */
  private _pendingPasteAbandon: (() => void) | null = null;

  // scroll batching; see queueScroll()
  private scrollAccum: {
    dx: number;
    dy: number;
    v120x: number;
    v120y: number;
    /** `timeStamp` of the newest event folded in. Deltas are rAF-batched, so the
     *  flush reports when the travel actually happened rather than when it was
     *  sent — kinetic scrolling integrates against this. */
    timeMs: number;
  } | null = null;
  private scrollFlushHandle: number | null = null;
  private scrollStopTimer: ReturnType<typeof setTimeout> | null = null;
  /** `axis_source` of the in-flight sequence, null between sequences.
   *  Latched by {@link latchScrollSource} so a momentum tail cannot be
   *  reclassified as a wheel mid-gesture. */
  private scrollSource: number | null = null;
  /** Whether a stop still owes the client. */
  private scrollSequenceOpen = false;
  /** Reasons this pane has already reported for swallowing a wheel event.
   *  See {@link reportWheelIgnored}. */
  private wheelIgnoredReported = new Set<string>();

  // bound event handlers
  private boundMouseDown: ((e: MouseEvent) => void) | null = null;
  private boundMouseUp: ((e: MouseEvent) => void) | null = null;
  private boundMouseMove: ((e: MouseEvent) => void) | null = null;
  private boundWindowMouseUp: ((e: MouseEvent) => void) | null = null;
  private boundWindowMouseMove: ((e: MouseEvent) => void) | null = null;
  private boundWheel: ((e: WheelEvent) => void) | null = null;
  private boundTouchStart: ((e: TouchEvent) => void) | null = null;
  private boundTouchMove: ((e: TouchEvent) => void) | null = null;
  private boundTouchEnd: ((e: TouchEvent) => void) | null = null;
  private boundTouchCancel: ((e: TouchEvent) => void) | null = null;
  private boundPointerDown: ((e: PointerEvent) => void) | null = null;
  private boundPointerMove: ((e: PointerEvent) => void) | null = null;
  private boundPointerUp: ((e: PointerEvent) => void) | null = null;
  private boundPointerCancel: ((e: PointerEvent) => void) | null = null;
  private boundMouseLeave: (() => void) | null = null;
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
  private boundWindowBlur: (() => void) | null = null;
  private boundBrowserClipboardChange: (() => void) | null = null;
  private boundDragEnter: ((e: DragEvent) => void) | null = null;
  private boundDragOver: ((e: DragEvent) => void) | null = null;
  private boundDragLeave: ((e: DragEvent) => void) | null = null;
  private boundDrop: ((e: DragEvent) => void) | null = null;
  private boundDragEnd: (() => void) | null = null;
  /** True between a sent DRAG_ENTER and its LEAVE / DROP / CANCEL — the
   *  compositor has a live wl_data_device drag session we are driving. */
  private dragActive = false;
  /** The active ENTER was a file drag.  Keep this independently of the
   *  current event's DataTransfer: WebKit may expose Files at ENTER, an
   *  empty protected store at DRAGOVER, and concrete files only at DROP. */
  private dragFilesActive = false;
  /** Last valid surface coordinates seen during this drag.  iPad WebKit can
   *  retarget DROP to the document and report its client position as 0,0. */
  private dragLastPoint: { x: number; y: number } | null = null;
  /** Staging names announced by the most recent file ENTER, in item order.
   *  These names are authoritative at DROP: `DataTransferItem.type` is
   *  visible during hover, while `File.type` is read after release and can
   *  differ (notably for file promises). */
  private dragPlannedNames: string[] | null = null;

  /** Per-canvas staging sync for file drops (FS_SYNC_STAGING), opened
   *  lazily on the first file drop and reused across drops: the staging
   *  dir is per-connection and lives until the connection closes, so one
   *  sync serves every drop on this canvas.  Stopped on dispose. */
  private dragStaging: FsSyncHandle | null = null;
  /** An in-flight open of `dragStaging` — concurrent drops share it. */
  private dragStagingOpening: Promise<FsSyncHandle> | null = null;

  constructor(options: BlitSurfaceCanvasOptions) {
    this._workspace = options.workspace;
    this._connectionId = options.connectionId;
    this._surfaceId = options.surfaceId;
    this._live = options.live !== false;
    this._expectsDisplaySize = options.resizable === true;
    this._touchMode = options.touchMode ?? "direct";
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

  setCtrlModifier(active: boolean): void {
    if (this._ctrlModifier === active) return;
    this._ctrlModifier = active;
    for (const listener of this._ctrlModifierListeners) listener(active);
  }

  get ctrlModifier(): boolean {
    return this._ctrlModifier;
  }

  onCtrlModifierChange(listener: (active: boolean) => void): () => void {
    this._ctrlModifierListeners.add(listener);
    return () => this._ctrlModifierListeners.delete(listener);
  }

  setAltModifier(active: boolean): void {
    if (this._altModifier === active) return;
    this._altModifier = active;
    for (const listener of this._altModifierListeners) listener(active);
  }

  get altModifier(): boolean {
    return this._altModifier;
  }

  onAltModifierChange(listener: (active: boolean) => void): () => void {
    this._altModifierListeners.add(listener);
    return () => this._altModifierListeners.delete(listener);
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
    // characters.  1px and transparent at the screen's top-left corner
    // (z-index -1 keeps it out of hit-testing).  Receives focus and
    // keyboard events.
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
    // Fixed at the top of the screen, for the same reason as the terminal's
    // textarea (see BlitTerminalSurface): an assist target iPadOS can
    // always keep clear of the keyboard, whatever the pane's position.
    ta.style.position = "fixed";
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
    ta.style.zIndex = "-1";
    // Ensure the container is a positioning context: applyLayout() places
    // the canvas absolutely within it.
    if (getComputedStyle(container).position === "static") {
      container.style.position = "relative";
    }
    container.appendChild(ta);
    this.textInput = ta;
    surfaceCanvasByInput.set(ta, this);

    container.appendChild(canvas);

    const svgNs = "http://www.w3.org/2000/svg";
    const remotePointerSvg = document.createElementNS(svgNs, "svg");
    remotePointerSvg.setAttribute("aria-hidden", "true");
    remotePointerSvg.setAttribute("preserveAspectRatio", "xMidYMid meet");
    remotePointerSvg.setAttribute("data-blit-remote-pointer", "");
    Object.assign(remotePointerSvg.style, {
      position: "absolute",
      left: "0",
      top: "0",
      width: "100%",
      height: "100%",
      overflow: "visible",
      pointerEvents: "none",
      visibility: "hidden",
      zIndex: "1",
    });
    const remotePointerGlyph = document.createElementNS(svgNs, "path");
    remotePointerGlyph.setAttribute("d", REMOTE_CURSOR_ARROW);
    remotePointerGlyph.setAttribute("fill", "#38bdf8");
    remotePointerGlyph.setAttribute("stroke", "#0b1020");
    remotePointerGlyph.setAttribute("stroke-width", "1.5");
    remotePointerGlyph.setAttribute("stroke-linejoin", "round");
    remotePointerGlyph.setAttribute("fill-rule", "evenodd");
    remotePointerGlyph.setAttribute("vector-effect", "non-scaling-stroke");
    remotePointerSvg.appendChild(remotePointerGlyph);
    const remotePointerImage = document.createElementNS(svgNs, "image");
    remotePointerImage.style.display = "none";
    remotePointerSvg.appendChild(remotePointerImage);
    container.appendChild(remotePointerSvg);
    this.remotePointerSvg = remotePointerSvg;
    this.remotePointerGlyph = remotePointerGlyph;
    this.remotePointerImage = remotePointerImage;

    this.canvas = canvas;
    this.ctx = canvas.getContext("2d");
    mountedSurfaceCanvases.set(canvas, this);

    this.observePresentBox(container);
    this.observeIntersection(container);
    this.subscribe();
    this.attachEvents();
    this.syncTouchCapability();
  }

  /**
   * Watch the container so blitFromStore knows how far the browser is about
   * to shrink the canvas.  See {@link _presentBox}.
   */
  private observePresentBox(container: HTMLElement): void {
    if (typeof ResizeObserver === "undefined") return;
    // Resizable panes deliberately keep their eager unscaled subscribe: the
    // framework binding is about to hand them a display size.  Passive views
    // need a box to derive their fixed encode target.  If layout has not run
    // yet, serverSubscribe() waits for the observer below instead of briefly
    // opening a native stream.
    this._waitForPresentBox = !this._expectsDisplaySize;
    // ResizeObserver runs after layout, but attach() subscribes immediately.
    // When the card is already laid out, seed its box synchronously and make
    // the first subscribe scaled.  Newly inserted cards can still be 0x0;
    // the guard above leaves those deferred until their first observer box.
    // Without this every card briefly asked for a native stream, then
    // replaced it with a ~512 px stream in the observer callback: two encoder
    // builds and a visible native↔thumbnail resolution flip on every load.
    //
    // Do not do this for a resizable pane.  Its binding calls setDisplaySize
    // immediately after attach; keeping the initial request unscaled avoids
    // creating the inverse thumbnail→native churn there.
    if (!this._expectsDisplaySize) {
      const rect = container.getBoundingClientRect();
      const dpr = (globalThis.devicePixelRatio ?? 1) || 1;
      const width = Math.round(rect.width * dpr);
      const height = Math.round(rect.height * dpr);
      if (width > 0 && height > 0) this._presentBox = { width, height };
    }
    const observer = new ResizeObserver((entries) => {
      const entry = entries[entries.length - 1];
      const box = entry && devicePixelBox(entry);
      if (!box) return;
      const firstBox = this._presentBox === null;
      this._presentBox = box;
      // subscribe() has already installed the store listeners.  A passive
      // mount whose synchronous box was 0x0 stopped at serverSubscribe();
      // now that the request can be scaled correctly, open it exactly once.
      if (firstBox && !this._subscribedSurface) {
        this.serverSubscribe();
      } else {
        // Ask the server for a stream sized to the new box.  Quantised, so a
        // drag re-asks only on an octave boundary — each change costs an
        // encoder rebuild and a keyframe.
        this.refreshScaledTarget();
      }
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

  /** Drop this view's server subscription while its mount is off-screen.
   *
   * Store listeners stay attached so metadata/cursor state remains current,
   * and the last decoded frame stays on the canvas.  On re-entry we reclaim
   * the same view token and immediately paint the store's newest frame.
   */
  private observeIntersection(container: HTMLElement): void {
    if (typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver((entries) => {
      const entry = entries[entries.length - 1];
      if (
        !entry ||
        this.disposed ||
        entry.isIntersecting === this._isIntersecting
      )
        return;
      this._isIntersecting = entry.isIntersecting;
      if (!this._isIntersecting) {
        this.serverUnsubscribe();
        return;
      }

      this.serverSubscribe();
      // A reconnect while hidden loses the server-side view size along with
      // its subscription.  The container did not resize, so its observer
      // will not send the size again for us.
      this.resendDisplaySize();
      const store = this.getConn()?.surfaceStore ?? this._store;
      if (store) this.blitFromStore(store);
    });
    observer.observe(container);
    this._intersectionObserver = observer;
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
    this._intersectionObserver?.disconnect();
    this._intersectionObserver = null;
    this.releaseAllKeys();
    this.releaseAllButtons();
    this.endScrollSequence();
    this.cancelDirectTouches();
    this.releaseTouchCapability();
    this.setDisplaySize(null);
    this.dragStaging?.stop();
    this.dragStaging = null;
    this.serverUnsubscribe();
    this.detachEvents();
    this.unsubscribeAll();
    if (this.textInput) {
      surfaceCanvasByInput.delete(this.textInput);
      if (this.container) this.container.removeChild(this.textInput);
    }
    this.textInput = null;
    if (this.canvas) mountedSurfaceCanvases.delete(this.canvas);
    if (this.canvas && this.container) {
      this.container.removeChild(this.canvas);
    }
    if (this.remotePointerSvg && this.container) {
      this.container.removeChild(this.remotePointerSvg);
    }
    this.canvas = null;
    this.ctx = null;
    this.remotePointerSvg = null;
    this.remotePointerGlyph = null;
    this.remotePointerImage = null;
    this.remoteInput = null;
    this.remoteContacts.length = 0;
    this.container = null;
  }

  setConnectionId(connectionId: ConnectionId): void {
    if (this._connectionId === connectionId) return;
    this.cancelDirectTouches();
    this.releaseTouchCapability();
    this.clearResizeConstraint();
    this._connectionId = connectionId;
    this.syncTouchCapability();
    this.resubscribe();
    this.resendDisplaySize();
  }

  setSurfaceId(surfaceId: number): void {
    if (this._surfaceId === surfaceId) return;
    this.cancelDirectTouches();
    this.cancelPointerTouchGesture();
    this.clearResizeConstraint();
    this._surfaceId = surfaceId;
    this.resubscribe();
    this.resendDisplaySize();
  }

  /** Toggle ownership of the server-side stream without dropping the shared
   * store listeners that keep a cached preview current. */
  setLive(live: boolean): void {
    if (this._live === live) return;
    this._live = live;
    if (!live) {
      this.serverUnsubscribe();
      return;
    }
    this.serverSubscribe();
    this.resendDisplaySize();
    const store = this.getConn()?.surfaceStore ?? this._store;
    if (store) this.blitFromStore(store);
  }

  setTouchMode(mode: SurfaceTouchMode): void {
    if (this._touchMode === mode) return;
    if (this._touchMode === "direct") {
      this.cancelDirectTouches();
      this.releaseTouchCapability();
    } else {
      this.cancelPointerTouchGesture();
    }
    this._touchMode = mode;
    this.syncTouchCapability();
  }

  get touchMode(): SurfaceTouchMode {
    return this._touchMode;
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
   *
   * `scale120` is the scale the *surface* is asked to render at, in 1/120ths
   * (Wayland convention): the app is handed `width * 120 / scale120` logical
   * pixels. `cssScale120` is the container's device-pixel ratio and defaults
   * to `scale120`; a binding applying relative zoom or an exact scale passes
   * the two separately, since the control moves the surface scale only.
   */
  setDisplaySize(
    width: number | null,
    height?: number,
    scale120?: number,
    cssScale120?: number,
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
    this._displaySize = {
      width: w,
      height: h,
      scale120: s,
      // A binding that applies no zoom passes one scale for both.
      cssScale120: cssScale120 && cssScale120 > 0 ? cssScale120 : s,
    };
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
   * view's requested scale. Null for views that don't size their own box.
   *
   * The server mediates one surface across all its viewers at the
   * *highest* scale any of them asked for (see `mediated_size_for_surface`),
   * so a small 3x pane and a large 1x pane settle on a small window
   * composited at 3x.  Filling the 1x pane with that frame would show the
   * window at 3x zoom — the same window drawn three times too big on the
   * client that never asked for a high-DPI anything.  Capping at
   * `logical × own scale` draws it at the requested size and lets the
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
    const box = this._presentationBox;
    // Unknown logical size (old server, or no resize reported yet): the
    // pane is the only answer we have.
    if (lw <= 0 || lh <= 0) {
      box.width = ds.width;
      box.height = ds.height;
      return box;
    }
    const SNAP = 3;
    const capW = Math.round((lw * ds.scale120) / 120);
    const capH = Math.round((lh * ds.scale120) / 120);
    box.width = capW < ds.width - SNAP ? capW : ds.width;
    box.height = capH < ds.height - SNAP ? capH : ds.height;
    return box;
  }

  private applyLayout(): void {
    const canvas = this.canvas;
    if (!canvas) return;
    const remotePointerSvg = this.remotePointerSvg;
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
      if (remotePointerSvg) {
        Object.assign(remotePointerSvg.style, {
          position: "absolute",
          left: "0",
          top: "0",
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
    // was asked for is then blitted 1:1.  The container's own ratio, not the
    // surface scale: under a zoom factor the two differ, and dividing by the
    // zoomed scale would draw the canvas that many times too small.
    const scale = ds.cssScale120 / 120;
    Object.assign(canvas.style, {
      position: "absolute",
      left: `${left / scale}px`,
      top: `${top / scale}px`,
      width: `${w / scale}px`,
      height: `${h / scale}px`,
    });
    if (remotePointerSvg) {
      Object.assign(remotePointerSvg.style, {
        position: "absolute",
        left: `${left / scale}px`,
        top: `${top / scale}px`,
        width: `${w / scale}px`,
        height: `${h / scale}px`,
      });
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
    if (conn && store.canDecodeVideo) this.serverSubscribe(conn, store);

    // Flush any pending resize and paint the latest frame immediately
    // so newly-mounted views aren't blank.
    this.flushPendingResize();
    this.blitFromStore(store);

    this.unsubChange = store.onChange(() => {
      const prev = this.surface;
      this.surface = store.getSurface(this._surfaceId);
      this.updateRemotePointerOverlay();
      // Re-subscribe when the store generation changed (reconnect — the
      // server dropped all subscriptions but the surface reappeared with
      // the same IDs).  We no longer need to handle the "surface info
      // just arrived" case here because subscribe() above sends the
      // subscribe eagerly before the surface metadata is available.
      if (this.surface && store.canDecodeVideo) {
        if (this._isIntersecting && !this._subscribedSurface) {
          this.serverSubscribe(this.getConn(), store);
          this.resendDisplaySize();
        } else if (
          this._isIntersecting &&
          this._subscribedGeneration !== store.generation
        ) {
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
      this.remoteCursor =
        store.getCursorImage?.(sid) ??
        (shape === "none"
          ? { kind: "hidden" }
          : { kind: "named", name: shape });
      this.updateRemotePointerOverlay();
    });
    // Apply initial cursor.
    if (this.canvas) {
      this.canvas.style.cursor = store.getCursor(this._surfaceId);
    }
    this.remoteCursor = store.getCursorImage?.(this._surfaceId) ?? {
      kind: "named",
      name: "default",
    };

    // Some embedders provide a narrow SurfaceStore-shaped test/cache facade;
    // keep the new overlay optional for those older facades.
    this.unsubRemotePointer = store.onRemoteInput?.((sid, input) => {
      if (sid !== this._surfaceId) return;
      this.remoteInput = input;
      this.updateRemotePointerOverlay();
    });
    // Narrow facades answer unknown members with a stub, so validate the shape
    // rather than trusting that this is a RemoteSurfaceInput at all.
    const initial = store.getRemoteInput?.(this._surfaceId) ?? null;
    this.remoteInput =
      initial &&
      Array.isArray(initial.pointer) &&
      Array.isArray(initial.touch) &&
      [...initial.pointer, ...initial.touch].every(
        (point) => Number.isFinite(point?.x) && Number.isFinite(point?.y),
      )
        ? initial
        : null;
    this.updateRemotePointerOverlay();

    this.unsubTextInput = store.onTextInput?.((sid, state) => {
      if (sid !== this._surfaceId) return;
      this.applyTextInputState(state);
    });
    const textInput = store.getTextInput?.(this._surfaceId) ?? null;
    if (textInput) {
      this.applyTextInputState({ ...textInput, requested: false });
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

  /** Register this visible mount with the connection exactly once. */
  private serverSubscribe(
    conn: BlitConnection | null = this.getConn(),
    store: import("./SurfaceStore").SurfaceStore | null = conn?.surfaceStore ??
      this._store,
  ): void {
    if (
      !this._live ||
      !this._isIntersecting ||
      !conn ||
      !store?.canDecodeVideo ||
      (this._waitForPresentBox && !this._presentBox) ||
      this._subscribedSurface
    ) {
      return;
    }
    const target = this.scaledTarget();
    conn.sendSurfaceSubscribe(
      this._surfaceId,
      this.surfaceViewId(conn),
      target,
      target ? BlitSurfaceCanvas.THUMBNAIL_MAX_FPS : 0,
    );
    this._subscribedGeneration = store.generation;
    this._subscribedSurface = {
      connectionId: this._connectionId,
      surfaceId: this._surfaceId,
    };
  }

  private unsubscribeAll(): void {
    this.unsubFrame?.();
    this.unsubChange?.();
    this.unsubCursor?.();
    this.unsubRemotePointer?.();
    this.unsubTextInput?.();
    this.unsubFrame = null;
    this.unsubChange = null;
    this.unsubCursor = null;
    this.unsubRemotePointer = null;
    this.unsubTextInput = null;
  }

  private updateRemotePointerOverlay(): void {
    const svg = this.remotePointerSvg;
    const glyph = this.remotePointerGlyph;
    const image = this.remotePointerImage;
    const input = this.remoteInput;
    const surface = this.surface;
    const cursor = this.remoteCursor;
    // A hidden cursor only hides the *pointer* mark. Fingers are the remote
    // user's, not the app's, so an app that hides the cursor cannot hide them.
    const showPointer =
      !!input && input.pointer.length > 0 && cursor.kind !== "hidden";
    const showTouch = !!input && input.touch.length > 0;
    if (!svg || !glyph || !image || !surface || (!showPointer && !showTouch)) {
      if (svg) svg.style.visibility = "hidden";
      this.layoutRemoteContacts(0, 1);
      return;
    }
    const width = Math.max(1, surface.width);
    const height = Math.max(1, surface.height);
    // Pointer artwork is sized in logical pixels. Scale it into the physical
    // composite so it remains a normal cursor size on high-DPI surfaces.
    const cursorScale =
      surface.logicalWidth > 0 ? width / surface.logicalWidth : 1;
    svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    const clampX = (v: number) => Math.max(0, Math.min(width, v));
    const clampY = (v: number) => Math.max(0, Math.min(height, v));

    // Fingers have no cursor artwork: one ring per contact, sized like a
    // fingertip. Drawn alongside the cursor, since one viewer can be doing both.
    const circles = this.layoutRemoteContacts(
      showTouch ? input.touch.length : 0,
      cursorScale,
    );
    if (showTouch) {
      input.touch.forEach((point, i) => {
        const circle = circles[i]!;
        circle.setAttribute("cx", String(clampX(point.x)));
        circle.setAttribute("cy", String(clampY(point.y)));
      });
    }

    if (!showPointer) {
      glyph.style.display = "none";
      image.style.display = "none";
      svg.style.visibility = "visible";
      return;
    }
    const x = clampX(input.pointer[0]!.x);
    const y = clampY(input.pointer[0]!.y);
    if (cursor.kind === "custom") {
      glyph.style.display = "none";
      image.style.display = "";
      image.setAttribute("href", cursor.url);
      // Hotspot and extent are both logical (the server divides the buffer's
      // pixel size by the cursor's own `buffer_scale`), so one factor takes
      // both into the physical-pixel viewBox. Scaling only the offset put the
      // hotspot a full cursor-width off the artwork on any HiDPI surface.
      image.setAttribute("x", String(x - cursor.hotspotX * cursorScale));
      image.setAttribute("y", String(y - cursor.hotspotY * cursorScale));
      image.setAttribute("width", String(cursor.width * cursorScale));
      image.setAttribute("height", String(cursor.height * cursorScale));
    } else {
      image.style.display = "none";
      glyph.style.display = "";
      // `hidden` was excluded by `showPointer`; TS cannot see that, so name the
      // fallback rather than assert.
      const artwork = remoteCursorGlyph(
        cursor.kind === "named" ? cursor.name : "default",
      );
      glyph.setAttribute("d", artwork.path);
      const rotation = artwork.rotation ? ` rotate(${artwork.rotation})` : "";
      glyph.setAttribute(
        "transform",
        `translate(${x} ${y}) scale(${cursorScale})${rotation}`,
      );
    }
    svg.style.visibility = "visible";
  }

  /**
   * Grow/shrink the contact-ring pool to `count` and return it.
   *
   * Elements are reused rather than recreated: this runs at the remote user's
   * touch-move rate, and churning DOM nodes per frame is exactly the cost the
   * store's dedup exists to avoid.
   */
  private layoutRemoteContacts(
    count: number,
    cursorScale: number,
  ): SVGCircleElement[] {
    const svg = this.remotePointerSvg;
    if (!svg) return [];
    while (this.remoteContacts.length < count) {
      const circle = document.createElementNS(
        "http://www.w3.org/2000/svg",
        "circle",
      );
      circle.setAttribute("fill", "rgba(56, 189, 248, 0.35)");
      circle.setAttribute("stroke", "#0b1020");
      circle.setAttribute("stroke-width", "1.5");
      circle.setAttribute("vector-effect", "non-scaling-stroke");
      svg.appendChild(circle);
      this.remoteContacts.push(circle);
    }
    for (let i = this.remoteContacts.length - 1; i >= count; i--) {
      this.remoteContacts[i]!.remove();
      this.remoteContacts.length = i;
    }
    // A fingertip is about this wide in logical pixels; scale like the cursor so
    // it stays the same physical size on a HiDPI surface.
    const r = String(REMOTE_CONTACT_RADIUS * cursorScale);
    for (const circle of this.remoteContacts) circle.setAttribute("r", r);
    return this.remoteContacts;
  }

  /** Copy the shared backing canvas onto our visible canvas. */
  private blitFromStore(store: import("./SurfaceStore").SurfaceStore): void {
    const src = store.getCanvas(this._surfaceId);
    const canvas = this.canvas;
    const ctx = this.ctx;
    if (!src || !canvas || !ctx) return;
    if (src.width === 0 || src.height === 0) return;
    if (this._lastFrameSize) {
      this._lastFrameSize.width = src.width;
      this._lastFrameSize.height = src.height;
    } else {
      this._lastFrameSize = { width: src.width, height: src.height };
    }

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
    this.remoteInput = null;
    this.remoteCursor = { kind: "named", name: "default" };
    this.updateRemotePointerOverlay();
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
    const target = this.scaledTarget();
    conn?.setSurfaceViewTarget(
      sub.surfaceId,
      this._surfaceViewId,
      target,
      target ? BlitSurfaceCanvas.THUMBNAIL_MAX_FPS : 0,
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
    this.boundWindowMouseMove = (e) =>
      this.handleWindowMouseGrab(e, SURFACE_POINTER_MOVE);
    this.boundWindowMouseUp = (e) =>
      this.handleWindowMouseGrab(e, SURFACE_POINTER_UP);
    this.boundWheel = (e) => this.handleWheel(e);
    this.boundTouchStart = (e) => this.handleTouchStart(e);
    this.boundTouchMove = (e) => this.handleTouchMove(e);
    this.boundTouchEnd = (e) => this.handleTouchEnd(e);
    this.boundTouchCancel = (e) => this.handleTouchCancel(e);
    this.boundPointerDown = (e) => this.handlePointerDown(e);
    this.boundPointerMove = (e) => this.handlePointerMove(e);
    this.boundPointerUp = (e) => this.handlePointerUp(e);
    this.boundPointerCancel = (e) => this.handlePointerCancel(e);
    // Nothing else tells the server the pointer stopped being over this
    // surface, and without it every peer keeps drawing our ghost cursor frozen
    // where we left it. `mouseleave` rather than `pointerleave`: a touch
    // contact's implicit capture fires `pointerleave` on lift, which is a
    // sequence end, not the pointer leaving the pane.
    this.boundMouseLeave = () => this.sendPointerLeave();
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
    // A Wayland selection stays authoritative while focus moves between
    // streamed surfaces.  Only leaving the browser context, or a genuine
    // DOM copy/cut, means the host clipboard may now be newer.
    this.boundWindowBlur = () =>
      this.getConn()?.noteBrowserClipboardMayHaveChanged();
    this.boundBrowserClipboardChange = () =>
      this.getConn()?.noteBrowserClipboardMayHaveChanged();

    canvas.addEventListener("mousedown", this.boundMouseDown);
    canvas.addEventListener("mouseup", this.boundMouseUp);
    canvas.addEventListener("mousemove", this.boundMouseMove);
    window.addEventListener("mousemove", this.boundWindowMouseMove, true);
    window.addEventListener("mouseup", this.boundWindowMouseUp, true);
    canvas.addEventListener("wheel", this.boundWheel, { passive: false });
    canvas.addEventListener("pointerdown", this.boundPointerDown);
    canvas.addEventListener("pointermove", this.boundPointerMove);
    canvas.addEventListener("pointerup", this.boundPointerUp);
    canvas.addEventListener("pointercancel", this.boundPointerCancel);
    canvas.addEventListener("mouseleave", this.boundMouseLeave);
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
    window.addEventListener("blur", this.boundWindowBlur);
    document.addEventListener("copy", this.boundBrowserClipboardChange, true);
    document.addEventListener("cut", this.boundBrowserClipboardChange, true);

    // OS drag-and-drop onto the surface.  Drags that carry only custom
    // MIMEs (pane/tile moves inside the page) are not ours: the handlers
    // return before touching them, so the page's own DnD is unaffected.
    this.boundDragEnter = (e) => this.handleDragEnter(e);
    this.boundDragOver = (e) => this.handleDragOver(e);
    this.boundDragLeave = (e) => this.handleDragLeave(e);
    this.boundDrop = (e) => this.handleDrop(e);
    canvas.addEventListener("dragenter", this.boundDragEnter);
    canvas.addEventListener("dragover", this.boundDragOver);
    canvas.addEventListener("dragleave", this.boundDragLeave);
    canvas.addEventListener("drop", this.boundDrop);
    // iPad WebKit can deliver the terminal DROP above the canvas (usually
    // document/body).  Capture it at the window while DataTransfer is still
    // readable; DROP_CLAIMED keeps the canvas listener from handling it twice.
    window.addEventListener("drop", this.boundDrop, true);
    // Belt and braces: an OS-source drag usually never fires dragend (it
    // belongs to the drag source), but if one does while our session is
    // still open, it means the drag was abandoned without a drop.
    this.boundDragEnd = () => {
      if (!this.dragActive) return;
      const conn = this.getConn();
      const current = conn ? activeBrowserDragCanvas.get(conn) : undefined;
      this.dragActive = false;
      this.dragFilesActive = false;
      this.dragLastPoint = null;
      this.dragPlannedNames = null;
      // Every mount observes window.dragend.  A stale previous mount must
      // clear only its local flag, not cancel the newer mount's session.
      if (conn && current === this) {
        activeBrowserDragCanvas.delete(conn);
        conn.sendSurfaceDragCancel();
      } else if (conn && !current) {
        conn.sendSurfaceDragCancel();
      }
    };
    window.addEventListener("dragend", this.boundDragEnd);

    this.boundCompositionStart = () => {
      this.compositionActive = true;
      for (const kc of this.pendingAlt) this.swallowedAlt.add(kc);
      this.pendingAlt.clear();
      if (this.textInput) this.textInput.focus({ preventScroll: true });
    };

    // The textarea is the normal keyboard target and owns the composition
    // lifecycle. Keep the canvas listener below as a fallback for embedders
    // that move focus there themselves.
    if (ta) {
      this.boundTextInput = (e) => this.handleTextInput(e as InputEvent);
      this.boundCompositionEnd = (e) => this.handleCompositionEnd(e);

      ta.addEventListener("input", this.boundTextInput);
      ta.addEventListener("compositionstart", this.boundCompositionStart);
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
    canvas.addEventListener("compositionstart", this.boundCompositionStart);
    this.seedIOSInputPad();
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
    if (this.boundWindowMouseMove)
      window.removeEventListener("mousemove", this.boundWindowMouseMove, true);
    if (this.boundWindowMouseUp)
      window.removeEventListener("mouseup", this.boundWindowMouseUp, true);
    if (this.boundWheel) canvas.removeEventListener("wheel", this.boundWheel);
    if (this.boundPointerDown)
      canvas.removeEventListener("pointerdown", this.boundPointerDown);
    if (this.boundPointerMove)
      canvas.removeEventListener("pointermove", this.boundPointerMove);
    if (this.boundPointerUp)
      canvas.removeEventListener("pointerup", this.boundPointerUp);
    if (this.boundPointerCancel)
      canvas.removeEventListener("pointercancel", this.boundPointerCancel);
    if (this.boundMouseLeave)
      canvas.removeEventListener("mouseleave", this.boundMouseLeave);
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
    if (this.boundWindowBlur)
      window.removeEventListener("blur", this.boundWindowBlur);
    if (this.boundBrowserClipboardChange) {
      document.removeEventListener(
        "copy",
        this.boundBrowserClipboardChange,
        true,
      );
      document.removeEventListener(
        "cut",
        this.boundBrowserClipboardChange,
        true,
      );
    }
    if (this.boundDragEnter)
      canvas.removeEventListener("dragenter", this.boundDragEnter);
    if (this.boundDragOver)
      canvas.removeEventListener("dragover", this.boundDragOver);
    if (this.boundDragLeave)
      canvas.removeEventListener("dragleave", this.boundDragLeave);
    if (this.boundDrop) canvas.removeEventListener("drop", this.boundDrop);
    if (this.boundDrop)
      window.removeEventListener("drop", this.boundDrop, true);
    if (this.boundDragEnd)
      window.removeEventListener("dragend", this.boundDragEnd);
    // Disposing mid-drag must not leave the compositor session dangling.
    if (this.dragActive) {
      this.dragActive = false;
      this.dragFilesActive = false;
      this.dragLastPoint = null;
      this.dragPlannedNames = null;
      const conn = this.getConn();
      const current = conn ? activeBrowserDragCanvas.get(conn) : undefined;
      if (conn && current === this) {
        activeBrowserDragCanvas.delete(conn);
        conn.sendSurfaceDragCancel();
      } else if (conn && !current) {
        conn.sendSurfaceDragCancel();
      }
    }
    this._pendingPaste = null;
    this._pendingPasteFlush = null;
    this._pendingPasteAbandon = null;
    if (this._iosInputRepadTimer !== null) {
      clearTimeout(this._iosInputRepadTimer);
      this._iosInputRepadTimer = null;
    }

    const ta = this.textInput;
    if (ta) {
      if (this.boundTextInput)
        ta.removeEventListener("input", this.boundTextInput);
      if (this.boundCompositionStart)
        ta.removeEventListener("compositionstart", this.boundCompositionStart);
      if (this.boundCompositionEnd)
        ta.removeEventListener("compositionend", this.boundCompositionEnd);
      if (this.boundKeyDown)
        ta.removeEventListener("keydown", this.boundKeyDown);
      if (this.boundKeyUp) ta.removeEventListener("keyup", this.boundKeyUp);
      if (this.boundFocus) ta.removeEventListener("focus", this.boundFocus);
      if (this.boundBlur) ta.removeEventListener("blur", this.boundBlur);
      if (this.boundPaste) ta.removeEventListener("paste", this.boundPaste);
    }
    this.compositionActive = false;
  }

  private handleMouse(e: MouseEvent, type: number): void {
    // The window-capture bridge already routed this physical event to the
    // surface under the pointer.  Its ordinary target listener must not
    // put the same packet on the wire a second time.
    if (routedGrabMouseEvents.has(e)) return;
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
    this.sendPointerAt(e.clientX, e.clientY, type, e.button, e.timeStamp);
    if (
      type === SURFACE_POINTER_DOWN &&
      e.button === 0 &&
      this.pressedButtons.has(e.button)
    ) {
      if (!activeSurfaceMouseGrab || activeSurfaceMouseGrab.owner !== this) {
        activeSurfaceMouseGrab = { owner: this, buttons: new Set() };
      }
      activeSurfaceMouseGrab.buttons.add(e.button);
    }
  }

  /** Find the mounted surface canvas under a window mouse event.  Use the
   * full hit-test stack so pane chrome layered above a canvas does not turn
   * a cross-surface drag into a gap. */
  private mouseGrabTarget(e: MouseEvent): BlitSurfaceCanvas | null {
    const doc = this.canvas?.ownerDocument ?? document;
    const hitTest = doc.elementsFromPoint?.bind(doc);
    const hits = hitTest ? hitTest(e.clientX, e.clientY) : [];
    const elements =
      hits.length > 0 ? hits : e.target instanceof Element ? [e.target] : [];
    const connection = this.getConn();
    for (const element of elements) {
      let node: Element | null = element;
      while (node) {
        const target =
          node instanceof HTMLCanvasElement
            ? mountedSurfaceCanvases.get(node)
            : undefined;
        if (target && target.getConn() === connection) return target;
        node = node.parentElement;
      }
    }

    // Passive previews deliberately use `pointer-events: none`, so they are
    // omitted from elementsFromPoint() even though their surrounding card is
    // the visible element under the pointer.  Match such a mounted canvas by
    // geometry, but only when the top hit belongs to its nearby wrapper.  The
    // wrapper check keeps a canvas hidden under an unrelated overlay from
    // becoming a drag target merely because their rectangles overlap.
    const topHit = elements[0];
    let nearest: { target: BlitSurfaceCanvas; distance: number } | null = null;
    if (topHit) {
      for (const [canvas, target] of mountedSurfaceCanvases) {
        if (target.getConn() !== connection) continue;
        const rect = canvas.getBoundingClientRect();
        if (
          e.clientX < rect.left ||
          e.clientX >= rect.left + rect.width ||
          e.clientY < rect.top ||
          e.clientY >= rect.top + rect.height
        ) {
          continue;
        }
        let wrapper = canvas.parentElement;
        for (let distance = 0; wrapper && distance < 4; distance++) {
          if (wrapper.contains(topHit)) {
            if (!nearest || distance < nearest.distance) {
              nearest = { target, distance };
            }
            break;
          }
          wrapper = wrapper.parentElement;
        }
      }
    }
    if (nearest) return nearest.target;
    return null;
  }

  /** Route held-mouse move/release at window capture phase.  This is the
   * browser half of Wayland's implicit DnD grab: it remains alive through
   * pane gaps and switches surface ids as the pointer crosses canvases. */
  private handleWindowMouseGrab(e: MouseEvent, type: number): void {
    const grab = activeSurfaceMouseGrab;
    if (!grab || grab.owner !== this || routedGrabMouseEvents.has(e)) return;
    if (type === SURFACE_POINTER_UP && !grab.buttons.has(e.button)) return;
    const target = this.mouseGrabTarget(e);
    // A gap carries no surface-local coordinate.  Keep the grab alive and
    // wait for the next canvas; a release in the gap still has to cancel it.
    if (type === SURFACE_POINTER_MOVE && !target) return;
    const receiver = target ?? this;
    routedGrabMouseEvents.add(e);
    receiver.sendPointerAt(e.clientX, e.clientY, type, e.button, e.timeStamp);
    if (type === SURFACE_POINTER_UP) {
      // The release may have been sent by another canvas, but the button was
      // recorded by the origin.  Clear both views of the physical grab.
      this.pressedButtons.delete(e.button);
      grab.buttons.delete(e.button);
      if (grab.buttons.size === 0) activeSurfaceMouseGrab = null;
    }
  }

  /** Focus where keystrokes should land: the editable textarea, so an input
   *  method has something to attach to.  The canvas routes the same key
   *  handlers, so it stands in only while the textarea does not exist. */
  private focusKeyboardTarget(): void {
    const target = this.textInput ?? this.canvas;
    target?.focus({ preventScroll: true });
  }

  private applyTextInputState(state: SurfaceTextInputEvent): void {
    const ta = this.textInput;
    if (!ta) return;

    if (state.enabled) {
      const inputMode = inputModeForContentPurpose(state.purpose);
      ta.dataset.blitInputmode = inputMode;
      // The app shell parks mobile inputs at `none` until it chooses to show
      // the keyboard. Do not defeat that policy, but keep an already-enabled
      // target in sync when content type changes without another enable.
      if (ta.getAttribute("inputmode") !== "none") {
        ta.setAttribute("inputmode", inputMode);
      }
      // ContentHint values from text-input-v3. These are advisory: mapping
      // them onto the editable capture target lets the host keyboard match
      // the remote field without pretending the textarea owns its contents.
      const completion = (state.hint & 0x1) !== 0;
      const spellcheck = (state.hint & 0x2) !== 0;
      ta.setAttribute("autocorrect", completion || spellcheck ? "on" : "off");
      ta.spellcheck = spellcheck;
      if ((state.hint & 0x10) !== 0) {
        ta.setAttribute("autocapitalize", "characters");
      } else if ((state.hint & 0x20) !== 0) {
        ta.setAttribute("autocapitalize", "words");
      } else if ((state.hint & 0x4) !== 0) {
        ta.setAttribute("autocapitalize", "sentences");
      } else {
        ta.setAttribute("autocapitalize", "none");
      }
    } else {
      delete ta.dataset.blitInputmode;
      if (ta.getAttribute("inputmode") !== "none") {
        ta.removeAttribute("inputmode");
      }
      ta.setAttribute("autocorrect", "off");
      ta.setAttribute("autocapitalize", "off");
      ta.spellcheck = false;
    }

    ta.dispatchEvent(
      new CustomEvent<SurfaceTextInputEvent>(BLIT_SURFACE_TEXT_INPUT_EVENT, {
        bubbles: true,
        composed: true,
        detail: state,
      }),
    );
  }

  private sendPointerAt(
    clientX: number,
    clientY: number,
    type: number,
    button: number,
    timeMs = 0,
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
    const point = this.surfaceWirePoint(clientX, clientY);
    if (!point) return;
    conn.sendSurfacePointer(
      this._surfaceId,
      type,
      button,
      point.x,
      point.y,
      timeMs,
    );
  }

  /**
   * A surface position ready for the wire.
   *
   * Every one of these coordinates is encoded into an unsigned 16-bit field, so
   * a position outside the drawn frame — the letterbox margin of an
   * `object-fit: contain` canvas, or a fractional `rect.left` against an integer
   * `clientX` — would be sent as its two's-complement wrap. The server reads
   * ~65535, and since it now mirrors these positions to other viewers, their
   * overlay clamps the bogus value to the opposite edge. Use this for anything
   * that reaches `C2S_SURFACE_POINTER` or `C2S_SURFACE_DRAG_*`.
   */
  private surfaceWirePoint(
    clientX: number,
    clientY: number,
  ): { x: number; y: number } | null {
    const point = this.surfacePointFromClient(clientX, clientY);
    if (!point || !this.surface) return null;
    return {
      x: Math.min(Math.max(point.x, 0), Math.max(0, this.surface.width - 1)),
      y: Math.min(Math.max(point.y, 0), Math.max(0, this.surface.height - 1)),
    };
  }

  /** Retire this view's shared-pointer overlay on its peers. */
  private sendPointerLeave(): void {
    if (!this.surface) return;
    this.getConn()?.sendSurfacePointer(
      this._surfaceId,
      SURFACE_POINTER_LEAVE,
      0,
      0,
      0,
    );
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
    rounded = true,
  ): { x: number; y: number } | null {
    const g = this.drawnGeometry();
    if (!g) return null;
    const x = (clientX - g.rect.left - g.dx) * g.sx;
    const y = (clientY - g.rect.top - g.dy) * g.sy;
    return {
      x: rounded ? Math.round(x) : x,
      y: rounded ? Math.round(y) : y,
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
    if (activeSurfaceMouseGrab?.owner === this) {
      activeSurfaceMouseGrab = null;
    }
  }

  private clearActiveTouch(): void {
    if (this.activeTouch?.longPressTimer) {
      clearTimeout(this.activeTouch.longPressTimer);
    }
    this.activeTouch = null;
  }

  private directTouchActive(): boolean {
    return (
      this._touchMode === "direct" && !!this.getConn()?.supportsSurfaceTouch
    );
  }

  private syncTouchCapability(): void {
    if (
      this.disposed ||
      !this.container ||
      this._touchMode !== "direct" ||
      this.touchCapabilityAcquired
    )
      return;
    const conn = this.getConn();
    // Embedders may provide a connection-like workspace compiled against a
    // core version predating direct touch. Defaulting to direct must remain
    // harmless for those connections, just as an unsupported server is.
    if (!conn || typeof conn.acquireSurfaceTouch !== "function") return;
    conn.acquireSurfaceTouch();
    this.touchCapabilityAcquired = true;
  }

  private releaseTouchCapability(): void {
    if (!this.touchCapabilityAcquired) return;
    this.getConn()?.releaseSurfaceTouch();
    this.touchCapabilityAcquired = false;
  }

  private cancelPointerTouchGesture(): void {
    const active = this.activeTouch;
    if (!active) return;
    if (active.pointerId != null)
      this.canvas?.releasePointerCapture?.(active.pointerId);
    if (active.mode === "drag") {
      this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_UP, 0);
    } else if (active.mode === "scroll") {
      this.endScrollSequence();
    }
    this.clearActiveTouch();
  }

  private cancelDirectTouches(): void {
    if (this.directTouchIds.size === 0) return;
    this.getConn()?.sendSurfaceTouch(this._surfaceId, SURFACE_TOUCH_CANCEL);
    this.directTouchIds.clear();
  }

  private directTouchPoints(list: TouchList): {
    identifier: number;
    x: number;
    y: number;
  }[] {
    const points: { identifier: number; x: number; y: number }[] = [];
    for (let i = 0; i < list.length; i++) {
      const touch = list.item(i);
      if (!touch) continue;
      const point = this.surfacePointFromClient(
        touch.clientX,
        touch.clientY,
        false,
      );
      if (!point) continue;
      points.push({
        identifier: touch.identifier,
        x: point.x,
        y: point.y,
      });
    }
    return points;
  }

  private sendDirectTouch(e: TouchEvent, phase: number): void {
    const points = this.directTouchPoints(e.changedTouches);
    if (points.length === 0) return;
    if (phase === SURFACE_TOUCH_DOWN) {
      this.focusKeyboardTarget();
      // A first contact starts a fresh sequence, so anything left in the set is
      // a `touchend` the browser never delivered — it drops them when a contact
      // leaves the element. Cancel rather than clearing locally: the server keeps
      // its own live set, which is what peers' overlays draw and what pins the
      // one-viewer touch lock. A purely local clear left a phantom ring on every
      // peer and no other viewer able to touch at all.
      if (
        this.directTouchIds.size > 0 &&
        e.touches.length === e.changedTouches.length
      ) {
        this.cancelDirectTouches();
      }
      for (const point of points) this.directTouchIds.add(point.identifier);
    }
    this.getConn()?.sendSurfaceTouch(
      this._surfaceId,
      phase,
      points,
      e.timeStamp,
    );
    if (phase === SURFACE_TOUCH_UP) {
      for (const point of points) this.directTouchIds.delete(point.identifier);
      if (e.touches.length === 0) this.directTouchIds.clear();
    }
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
        // The hold completed, but nothing goes on the wire yet: moving from
        // here starts the left-button drag, lifting without moving is a
        // right-click. The finger's next event decides which.
        active.mode = "held";
      }, 350),
    };
  }

  private moveTouchGesture(clientX: number, clientY: number, timeMs = 0): void {
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
      // Scroll goes to the surface holding pointer focus, and only motion
      // moves that focus (a tap counts: the server synthesises a move from
      // the press coordinates).  A finger drag sends no motion of its own,
      // so without this the axis events land wherever the cursor was last
      // left — another window, or nowhere — and the first drag after
      // touching elsewhere scrolls nothing until a tap re-seeds it.
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_MOVE, 0);
    }

    if (active.mode === "held" && moved > 8) {
      // The held finger moved: this is the drag the hold was waiting for.
      // The press lands where the finger is now, and the motion that
      // follows carries it.
      active.mode = "drag";
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_DOWN, 0);
    }

    active.lastX = clientX;
    active.lastY = clientY;

    if (active.mode === "drag") {
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_MOVE, 0, timeMs);
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
          timeMs,
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
    } else if (active.mode === "held") {
      // A hold that never moved is a right-click. Button 2 is the DOM's
      // right button, mapped to BTN_RIGHT server-side like a mouse's.
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_DOWN, 2);
      this.sendPointerAt(clientX, clientY, SURFACE_POINTER_UP, 2);
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
    if (e.pointerType === "touch" && this.directTouchActive()) return;
    this.canvas.setPointerCapture?.(e.pointerId);
    this.startTouchGesture(e.pointerId, e.clientX, e.clientY, e.pointerId);
  }

  private handlePointerMove(e: PointerEvent): void {
    if (e.pointerType === "touch" && this.directTouchActive()) {
      e.preventDefault();
      return;
    }
    const active = this.activeTouch;
    if (
      e.pointerType === "mouse" ||
      !active ||
      active.pointerId !== e.pointerId
    )
      return;
    e.preventDefault();
    this.moveTouchGesture(e.clientX, e.clientY, e.timeStamp);
  }

  private handlePointerUp(e: PointerEvent): void {
    if (e.pointerType === "touch" && this.directTouchActive()) {
      e.preventDefault();
      return;
    }
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
    if (e.pointerType === "touch" && this.directTouchActive()) {
      e.preventDefault();
      return;
    }
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
    if (
      this._touchMode === "direct" &&
      !this.directTouchActive() &&
      this.directTouchIds.size > 0
    ) {
      // A reconnect to a server without the feature already terminated the
      // old server-side sequence. Do not let its browser identifiers leak
      // into the pointer fallback's next gesture.
      this.directTouchIds.clear();
    }
    if (this.directTouchActive()) {
      this.sendDirectTouch(e, SURFACE_TOUCH_DOWN);
      return;
    }
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
    if (this.directTouchIds.size > 0 && this.directTouchActive()) {
      this.sendDirectTouch(e, SURFACE_TOUCH_MOTION);
      return;
    }
    const active = this.activeTouch;
    if (!active || active.pointerId != null) return;
    const touch = this.findActiveTouch(e.touches);
    if (!touch) return;
    this.moveTouchGesture(touch.clientX, touch.clientY, e.timeStamp);
  }

  private handleTouchEnd(e: TouchEvent): void {
    // Same reasoning as handleTouchStart: the pointer path has usually
    // already ended the gesture and nulled activeTouch by the time this
    // runs, so cancel the default first or the guards below skip it.
    e.preventDefault();
    if (this.directTouchIds.size > 0 && this.directTouchActive()) {
      this.sendDirectTouch(e, SURFACE_TOUCH_UP);
      return;
    }
    const active = this.activeTouch;
    if (!active) return;
    const touch = this.findActiveTouch(e.changedTouches);
    if (!touch) return;
    if (active.longPressTimer) clearTimeout(active.longPressTimer);

    if (active.mode === "drag") {
      this.sendPointerAt(active.lastX, active.lastY, SURFACE_POINTER_UP, 0);
    } else if (active.mode === "held") {
      // As in endTouchGesture(): a hold that never moved is a right-click.
      this.sendPointerAt(touch.clientX, touch.clientY, SURFACE_POINTER_DOWN, 2);
      this.sendPointerAt(touch.clientX, touch.clientY, SURFACE_POINTER_UP, 2);
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
    if (this.directTouchIds.size > 0 && this.directTouchActive()) {
      e.preventDefault();
      this.cancelDirectTouches();
      return;
    }
    const active = this.activeTouch;
    if (!active) return;
    e.preventDefault();
    this.cancelPointerTouchGesture();
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

  /**
   * Report a swallowed wheel event once per reason.
   *
   * Every gate below is per-pane state, so any of them can silence one
   * pane's wheel while its neighbours scroll normally — and all of them
   * used to do it silently, which is unfalsifiable from the outside. Once
   * per reason, because a stuck gate is re-hit at the wheel's event rate,
   * and the set is cleared by the next wheel that gets through so a
   * recurrence is reported again.
   */
  private reportWheelIgnored(reason: string): void {
    if (this.wheelIgnoredReported.has(reason)) return;
    this.wheelIgnoredReported.add(reason);
    console.warn(
      `blit: surface ${this._surfaceId} ignored a wheel (${reason})`,
    );
  }

  private handleWheel(e: WheelEvent): void {
    // No display size means a thumbnail rather than a live view, and
    // those take no other input either. Claiming the wheel there would
    // scroll an app the user is only previewing, and the preventDefault
    // below would stop the page scrolling under the cursor.
    const conn = this.getConn();
    if (!conn) return this.reportWheelIgnored("no connection");
    if (!this.surface) {
      return this.reportWheelIgnored("no surface entry in the store");
    }
    if (!this._displaySize) {
      return this.reportWheelIgnored(
        "no display size (preview, or an unmeasured pane)",
      );
    }
    // Ctrl+wheel is how browsers report a pinch-zoom gesture, including
    // macOS trackpad pinches. It is a zoom request, not a scroll; sending
    // it on would scroll the surface while the user pinches. Deliberate,
    // so it is not reported.
    if (e.ctrlKey) return;
    const g = this.drawnGeometry();
    if (!g) {
      return this.reportWheelIgnored("canvas or CSS box measures zero");
    }
    this.wheelIgnoredReported.clear();
    e.preventDefault();
    // Alt+scroll is a real chord (horizontal scroll, zoom in some apps):
    // a held-back Alt press belongs ahead of the axis events.  No-op when
    // no Alt press is pending dead-key detection.
    this.flushPendingAlt(conn);
    // Axis routing follows the compositor's last pointer-motion hit test,
    // not the toplevel id carried by the axis message.  Re-seed it from the
    // wheel's own coordinates before every delta: a popup can disappear
    // under a stationary cursor (including halfway through momentum), which
    // otherwise leaves no live surface to receive this or any later scroll.
    // Touch scrolling does the same when the drag first becomes a scroll.
    this.sendPointerAt(e.clientX, e.clientY, SURFACE_POINTER_MOVE, 0);

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
      timeMs: e.timeStamp,
    });
  }

  /**
   * Add to the pending scroll and arrange for it to be sent.
   *
   * Smooth wheel events already arrive at the browser's input cadence. Send
   * those immediately: waiting for the next animation frame couples input
   * to presentation, so one missed viewer frame merges two small trackpad
   * deltas into one visible jump. Notched wheels and direct touch gestures
   * retain frame batching to keep bursts bounded.
   */
  private queueScroll(part: {
    dx: number;
    dy: number;
    v120x: number;
    v120y: number;
    source: number;
    timeMs?: number;
  }): void {
    const source = this.latchScrollSource(part.source);
    this.scrollSequenceOpen = true;
    const a = (this.scrollAccum ??= {
      dx: 0,
      dy: 0,
      v120x: 0,
      v120y: 0,
      timeMs: 0,
    });
    a.dx += part.dx;
    a.dy += part.dy;
    a.v120x += part.v120x;
    a.v120y += part.v120y;
    a.timeMs = Math.max(a.timeMs, part.timeMs ?? 0);

    if (source === AXIS_SOURCE_CONTINUOUS) {
      if (this.scrollFlushHandle !== null) {
        cancelAnimationFrame(this.scrollFlushHandle);
        this.scrollFlushHandle = null;
      }
      this.flushScroll();
    } else if (this.scrollFlushHandle === null) {
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
    if (!conn || !this.surface) {
      return this.reportWheelIgnored(
        "connection or surface lost before the flush",
      );
    }
    if (a.dx === 0 && a.dy === 0 && a.v120x === 0 && a.v120y === 0) {
      // `drawnGeometry`'s sx/sy come from the surface's own dimensions, so a
      // surface reported as zero-sized scales every delta to nothing and the
      // pane looks dead rather than slow.
      return this.reportWheelIgnored("accumulated delta scaled to zero");
    }
    conn.sendSurfaceAxis2(this._surfaceId, {
      dx: a.dx,
      dy: a.dy,
      v120x: a.v120x,
      v120y: a.v120y,
      source: this.scrollSource ?? AXIS_SOURCE_CONTINUOUS,
      stop: false,
      timeMs: a.timeMs,
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

  private handleDragEnter(e: DragEvent): void {
    const mimes = dragOfferMimes(e.dataTransfer);
    if (!mimes) return;
    const conn = this.getConn();
    if (!conn || !this.surface || !this._displaySize) return;
    const point = this.surfaceWirePoint(e.clientX, e.clientY);
    if (!point) return;
    // WebKit's target contract asks both ENTER and OVER to prevent default.
    // In particular, accepting only OVER is not enough for every iPad drag
    // provider to deliver the terminal DROP.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
    this.dragActive = true;
    this.dragFilesActive = !!e.dataTransfer && dragHasFiles(e.dataTransfer);
    this.dragLastPoint = point;
    activeBrowserDragCanvas.set(conn, this);
    const itemMimes = dragFileItemMimes(e.dataTransfer);
    this.dragPlannedNames =
      itemMimes?.map((mime, index) => plannedDropName(mime, index)) ?? null;
    // The file items' MIMEs ride along so the server can pre-create the
    // planned staging files and serve their text/uri-list during hover:
    // Chromium fetches the offer's data at wl_data_device.enter and only
    // fires the page's dragenter — the remote app's drop UI — once that
    // completes.  Chrome preserves items/files order between dragover and
    // drop, so the index alignment the planned names rely on holds.
    conn.sendSurfaceDragEnter(
      this._surfaceId,
      point.x,
      point.y,
      mimes,
      itemMimes,
    );
  }

  private handleDragOver(e: DragEvent): void {
    // Once ENTER claimed a native drag, keep claiming it.  WebKit's
    // protected DataTransfer view is not stable across the gesture; asking
    // it to classify every DRAGOVER can skip preventDefault(), which makes
    // the browser suppress DROP and strands the Wayland session at ENTER.
    if (!this.dragActive && !dragOfferMimes(e.dataTransfer)) return;
    // Required for `drop` to fire at all.  We always allow: whether the
    // remote app accepts the offered types is not reported back.
    e.preventDefault();
    if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
    const conn = this.getConn();
    if (!conn || !this.surface || !this._displaySize) return;
    const point = this.surfaceWirePoint(e.clientX, e.clientY);
    if (!point) return;
    this.dragLastPoint = point;
    // Unthrottled, like pointer motion — the events fire continuously.
    conn.sendSurfaceDragMotion(this._surfaceId, point.x, point.y);
  }

  private handleDragLeave(e: DragEvent): void {
    if (!this.dragActive) return;
    // dragleave also fires when the pointer crosses into a child element;
    // only leaving the canvas itself ends the session.
    if (e.relatedTarget && this.canvas?.contains(e.relatedTarget as Node))
      return;
    // It fires *after* the new target's dragenter.  The latest canvas map
    // distinguishes that stale old-mount LEAVE from a real, possibly very
    // quick exit; never discard a genuine exit on timing alone.
    const conn = this.getConn();
    const current = conn ? activeBrowserDragCanvas.get(conn) : undefined;
    if (current && current !== this) {
      this.dragActive = false;
      this.dragFilesActive = false;
      this.dragLastPoint = null;
      this.dragPlannedNames = null;
      return;
    }
    this.dragActive = false;
    this.dragFilesActive = false;
    this.dragLastPoint = null;
    this.dragPlannedNames = null;
    if (conn && activeBrowserDragCanvas.get(conn) === this)
      activeBrowserDragCanvas.delete(conn);
    conn?.sendSurfaceDragLeave(this._surfaceId);
  }

  private handleDrop(e: DragEvent): void {
    const claimed = e as DragEvent & { [DROP_CLAIMED]?: true };
    if (claimed[DROP_CLAIMED]) return;
    const dt = e.dataTransfer;
    const conn = this.getConn();
    const current = conn ? activeBrowserDragCanvas.get(conn) : undefined;
    // Several mounts can briefly retain dragActive across enter-before-leave.
    // The most recently entered one owns a document-retargeted DROP.
    if (current && current !== this) return;
    const targetIsThisCanvas =
      !!this.canvas && e.composedPath().includes(this.canvas);
    // A DROP for a session we entered is ours even if WebKit now presents a
    // different (or empty) DataTransfer view.  Every accepted ENTER must end
    // in DROP or CANCEL; returning here leaves the remote app looking as if
    // the user never released the drag.
    if (
      !this.dragActive &&
      current !== this &&
      !(targetIsThisCanvas && dragOfferMimes(dt))
    )
      return;
    claimed[DROP_CLAIMED] = true;
    e.preventDefault();
    const fileDrag = this.dragFilesActive || (!!dt && dragHasFiles(dt));
    const lastPoint = this.dragLastPoint;
    this.dragActive = false;
    this.dragFilesActive = false;
    this.dragLastPoint = null;
    const plannedNames = this.dragPlannedNames;
    this.dragPlannedNames = null;
    if (conn && activeBrowserDragCanvas.get(conn) === this)
      activeBrowserDragCanvas.delete(conn);
    if (!conn) return;
    if (!this.surface || !this._displaySize || !dt) {
      conn.sendSurfaceDragCancel();
      return;
    }
    const eventPoint = this.surfaceWirePoint(e.clientX, e.clientY);
    const point =
      lastPoint && e.clientX === 0 && e.clientY === 0
        ? lastPoint
        : (eventPoint ?? lastPoint);
    if (!point) {
      conn.sendSurfaceDragCancel();
      return;
    }
    const surfaceId = this._surfaceId;

    const files = fileDrag ? droppedFiles(dt) : [];
    if (fileDrag) {
      if (files.length === 0) {
        // A file drag we could not read any file out of — say so rather
        // than silently falling through to the text path.
        console.warn("blit: file drop carried no readable files");
        conn.sendSurfaceDragCancel();
        return;
      }
      if (plannedNames && plannedNames.length !== files.length) {
        // The remote app already received a URI list with this many paths.
        // A different DROP cannot be made consistent with that snapshot.
        console.warn(
          "blit: file drop item count changed after drag enter — drag cancelled",
        );
        conn.sendSurfaceDragCancel();
        return;
      }
      // Files are staged through the chunked upload pump first; the DROP
      // goes out once every upload settles, naming the staged paths with
      // empty data — the paced chunks never stall interactive input the
      // way one big inline frame did.
      void this.dropFiles(
        conn,
        surfaceId,
        point.x,
        point.y,
        files,
        plannedNames,
      );
      return;
    }

    const text = dt.getData("text/plain");
    const data = new TextEncoder().encode(text);
    if (data.length > MAX_DND_BYTES) {
      console.warn(
        `blit: dropped text is ${data.length} bytes, over the ` +
          `${MAX_DND_BYTES}-byte drag-and-drop limit — not dropped`,
      );
      conn.sendSurfaceDragCancel();
      return;
    }
    conn.sendSurfaceDragDrop(surfaceId, point.x, point.y, [
      {
        mime: "text/plain;charset=utf-8",
        name: "",
        data,
      },
    ]);
  }

  /** Upload dropped files into the connection's drag staging dir, then
   *  send the DROP naming them.  Any failure — the staging open or an
   *  upload — cancels the session so no drag session dangles
   *  compositor-side. */
  private async dropFiles(
    conn: BlitConnection,
    surfaceId: number,
    x: number,
    y: number,
    files: File[],
    plannedNames: readonly string[] | null,
  ): Promise<void> {
    let totalBytes = files.reduce((total, file) => total + file.size, 0);
    const firstName =
      files[0]?.name ||
      (files[0]
        ? (plannedNames?.[0] ?? materializedDropName(files[0], 0))
        : "file");
    const target = this.surface?.title || this.surface?.appId || undefined;
    // Tests and third-party structural workspace mocks may predate the
    // activity registry, hence the defensive optional access.
    const activity = this._workspace.activities?.begin({
      kind: "upload",
      label: files.length === 1 ? firstName : `${files.length} files`,
      target,
      completed: 0,
      total: totalBytes,
    });
    let completedBytes = 0;
    try {
      const knownMimes = files.map(materializedDropMimeFromMetadata);
      let materializedMimes = knownMimes.every(
        (mime): mime is string => mime !== null,
      )
        ? knownMimes
        : await Promise.all(
            files.map(
              (file, index) => knownMimes[index] ?? materializedDropMime(file),
            ),
          );
      if (
        isIPadOS() &&
        materializedMimes.some(
          (mime) => mime === "image/heic" || mime === "image/heif",
        )
      ) {
        const compatible = await Promise.all(
          files.map((file, index) =>
            compatibleIPadDropFile(file, materializedMimes[index]),
          ),
        );
        files = compatible.map((item) => item.file);
        materializedMimes = compatible.map((item) => item.mime);
        totalBytes = files.reduce((total, file) => total + file.size, 0);
        activity?.update({
          label:
            files.length === 1
              ? files[0].name || plannedDropName(materializedMimes[0], 0)
              : `${files.length} files`,
          completed: 0,
          total: totalBytes,
        });
      }
      const materializedNames = materializedMimes.map((mime, index) =>
        plannedDropName(mime, index),
      );
      let stagedNames = plannedNames;
      if (
        plannedNames &&
        plannedNames.some((name, index) => name !== materializedNames[index])
      ) {
        // WebKit's typeless iPad hover used a provisional .bin URI solely
        // to let the destination show its drag UI.  Now that DROP exposes
        // the real representation, replace that offer before any bytes land
        // so the destination receives the truthful filename and MIME.
        conn.sendSurfaceDragEnter(
          surfaceId,
          x,
          y,
          ["text/uri-list", "application/octet-stream"],
          materializedMimes,
        );
        stagedNames = materializedNames;
      }
      const handle = await this.dragStagingHandle(conn);
      const items: { mime: string; name: string; data: Uint8Array }[] = [];
      for (let i = 0; i < files.length; i++) {
        const file = files[i];
        const mime = materializedMimes[i];
        // Stage to the path ENTER pre-announced for this item — the
        // uri-list the remote app already received names exactly it.
        const staged =
          stagedNames?.[i] ??
          (plannedDropExtension(mime)
            ? plannedDropName(mime, i)
            : materializedDropName(file, i));
        await handle.upload(staged, file, {
          onProgress: (uploaded) =>
            activity?.update({
              label: file.name || staged,
              completed: completedBytes + uploaded,
              total: totalBytes,
            }),
        });
        completedBytes += file.size;
        items.push({
          mime,
          name: staged,
          data: new Uint8Array(0),
        });
      }
      conn.sendSurfaceDragDrop(surfaceId, x, y, items);
    } catch (err) {
      console.warn(
        `blit: could not stage a dropped file — drag cancelled`,
        err,
      );
      conn.sendSurfaceDragCancel();
    } finally {
      activity?.finish();
    }
  }

  /** The canvas's staging sync, opened on first use and reused; a drop
   *  that arrives while one is opening joins the same open. */
  private dragStagingHandle(conn: BlitConnection): Promise<FsSyncHandle> {
    if (this.dragStaging) return Promise.resolve(this.dragStaging);
    this.dragStagingOpening ??= conn
      .syncFs("", {
        staging: true,
        onClosed: () => {
          // Closed from under us (server restart, connection loss) — the
          // next drop reopens fresh.
          this.dragStaging = null;
        },
      })
      .then((handle) => {
        this.dragStagingOpening = null;
        if (this.disposed) {
          handle.stop();
        } else {
          this.dragStaging = handle;
        }
        return handle;
      })
      .catch((err) => {
        this.dragStagingOpening = null;
        throw err;
      });
    return this.dragStagingOpening;
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
    if (conn.usesWaylandClipboard()) return;

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

  /** Read an image from a clipboard read started by the Ctrl keydown.
   *
   *  Starting `clipboard.read()` synchronously is load-bearing on browsers
   *  that gate it on transient user activation: waiting for `readText()` to
   *  settle first loses the key event's activation on macOS, so an image-only
   *  clipboard is refused even though the user just pressed Ctrl+V.  We still
   *  wait for `readText()` before consuming this result because text wins when
   *  the clipboard carries both representations.
   *
   *  Only used for Ctrl chords.  A Cmd chord's paste event is guaranteed (the
   *  macOS menu command fires for the textarea focus is forced onto) and owns
   *  the chord's outcome. */
  private readClipboardImage(
    read: Promise<ClipboardItem[] | null> | null,
    flush: (payload: ClipboardPayload | null) => void,
  ): void {
    if (!read) {
      this._pendingPasteAbandon?.();
      return;
    }
    void read.then(
      async (items) => {
        try {
          if (!this._pendingPasteFlush) return; // a paste event claimed it
          if (!items) {
            this._pendingPasteAbandon?.();
            return;
          }
          const images: { mime: string; item: ClipboardItem }[] = [];
          for (const item of items) {
            for (const mime of item.types) {
              if (mime.startsWith("image/")) images.push({ mime, item });
            }
          }
          // No image either: the clipboard holds nothing we can paste.
          if (images.length === 0) {
            this._pendingPasteAbandon?.();
            return;
          }
          // Same preference order as the paste-event path: PNG is what
          // every toolkit asks for.
          const pick =
            IMAGE_MIME_PREFERENCE.map((mime) =>
              images.find((i) => i.mime === mime),
            ).find((i) => i !== undefined) ?? images[0];
          const buf = await (await pick.item.getType(pick.mime)).arrayBuffer();
          if (!this._pendingPasteFlush) return;
          if (buf.byteLength > MAX_CLIPBOARD_BYTES) {
            console.warn(
              `blit: clipboard image is ${buf.byteLength} bytes, over the ` +
                `${MAX_CLIPBOARD_BYTES}-byte paste limit — not pasted`,
            );
            this._pendingPasteAbandon?.();
            return;
          }
          flush({ mime: pick.mime || "image/png", data: new Uint8Array(buf) });
        } catch {
          this._pendingPasteAbandon?.();
        }
      },
      // The promise is normalized when it is started, but keep the rejection
      // path defensive in case a non-native implementation returns one.
      () => this._pendingPasteAbandon?.(),
    );
  }

  private handleKey(e: KeyboardEvent, pressed: boolean): void {
    // If a global shortcut (capture-phase) already handled this event,
    // don't forward it to the Wayland surface.
    if (e.defaultPrevented) return;
    // Only forward input when interactive (resizable/focused mode).
    // Sidebar previews should not intercept keyboard or send events.
    if (!this._displaySize) return;

    // Dead keys / ongoing IME composition stay with the hidden textarea so
    // the browser can finish the composition. Its compositionend handler
    // sends the result; focus remains on this editable target.
    if (
      pressed &&
      (e.key === "Dead" ||
        e.isComposing ||
        this.compositionActive ||
        e.keyCode === 229)
    ) {
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
      domKeyToEvdev(e.code) === 0 &&
      !isEnterKeyEvent(e)
    )
      return;

    // Ctrl/Alt in the mobile row are one-shot modifiers. Browser-generated
    // events cannot carry a modifier armed by page chrome, so synthesize the
    // complete native chord here and consume the modifier. A later key-up is
    // harmless because this path completes the press/release atomically.
    if (
      pressed &&
      !EVDEV_MODIFIERS.has(domKeyToEvdev(e.code)) &&
      this.sendOneShotModifiedKey(e.key, e.code, e.shiftKey)
    ) {
      e.preventDefault();
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
    const preserveWaylandClipboard =
      isPasteShortcut && conn.usesWaylandClipboard();
    if (preserveWaylandClipboard) e.preventDefault();

    // Android Chromium/Brave can report the virtual Return key as
    // key="Enter", code="".  The code-only evdev path below used to drop it
    // after preventDefault had also cancelled the textarea's line-break input
    // event.  A soft key has no reliable key-up sequence, so complete the
    // press atomically; a late key-up remains an ignored orphan.
    if (domKeyToEvdev(e.code) === 0 && isEnterKeyEvent(e)) {
      if (pressed) {
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, false);
      }
      return;
    }

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
        metaChord: e.metaKey && !e.ctrlKey,
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
        this._pendingPasteAbandon = null;
        if (payload) {
          conn.sendClipboard(payload.mime, payload.data);
        }
        if (keycode !== 0) {
          this.pressedKeys.add(keycode);
          conn.sendSurfaceInput(surfaceId, keycode, true);
          // A Cmd chord's V key-up never reaches the page (Chrome on macOS
          // consumes the key equivalent whole — see metaChord above), so
          // its release goes out with the press; waiting for it would
          // leave V held and the app key-repeating the paste forever.
          if (p.released || p.metaChord) {
            this.pressedKeys.delete(keycode);
            conn.sendSurfaceInput(surfaceId, keycode, false);
          }
        }
        if (p.deferredCtrlRelease) {
          if (keycode !== 0 && !p.released && !p.metaChord) {
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

      // Stand-down for the outcomes that must not press V: a clipboard
      // holding nothing we can paste, or an image we declined to forward —
      // pressing V behind either would paste something the user did not
      // copy.  Releases whatever the deferral held back and undoes the
      // Meta→Ctrl translation.  Never on a timer: the chord's outcome is
      // decided by the clipboard reads settling, the paste event, or blur.
      this._pendingPasteAbandon = () => {
        const p = this._pendingPaste;
        if (!p || p.keycode !== keycode) return;
        this._pendingPaste = null;
        this._pendingPasteFlush = null;
        this._pendingPasteAbandon = null;
        if (p.deferredCtrlRelease) {
          this.pressedKeys.delete(29);
          conn.sendSurfaceInput(surfaceId, 29, false);
          this._metaToCtrlKey = 0;
        }
      };

      // The compositor already has a live client-owned selection.  Press V
      // immediately and let the destination receive its chosen MIME
      // directly from that source; touching navigator.clipboard here would
      // collapse/replace the selection (and loses image-only app copies).
      if (preserveWaylandClipboard) {
        flush(null);
        return;
      }

      // `navigator.clipboard.readText()` is often denied without an
      // explicit user-granted permission, and the `paste` event that backs
      // it up only fires reliably on an editable element — Chromium/Brave
      // do not dispatch it to a focused canvas.  Focus normally rests on
      // the textarea already; this is the belt for a view that somehow
      // left it on the canvas.  The canvas↔textarea shuffle this can cause
      // is exactly what handleBlur's relatedTarget check ignores.
      if (this.textInput) this.textInput.focus({ preventScroll: true });

      const metaChord = e.metaKey && !e.ctrlKey;
      const startImageRead = (): Promise<ClipboardItem[] | null> | null => {
        const read = navigator.clipboard?.read?.bind(navigator.clipboard);
        if (!read) return null;
        try {
          return read().then(
            (items) => items,
            () => null,
          );
        } catch {
          return Promise.resolve(null);
        }
      };
      // macOS does not treat Ctrl+V as its native paste command, so no paste
      // event will authorize the richer read.  Start it now, within the
      // keydown's transient activation.  Elsewhere Ctrl+V normally produces a
      // paste event; don't prompt for async clipboard permission unless that
      // event and readText both fail to supply content.
      const imageRead =
        !metaChord && this.macOptionChars ? startImageRead() : null;
      const imageFallback = () => {
        // `_pendingPasteFlush` being cleared means a paste event already
        // claimed this chord: an image is on its way and its text
        // representation, if any, must not pre-empt it.
        if (!this._pendingPasteFlush) return;
        // A Cmd chord's paste event is guaranteed — the macOS menu command
        // fires against the textarea focus is forced onto — and may trail
        // the readText settle by a task, so leave the chord for it.  A Ctrl
        // chord no paste event has claimed by now never gets one (browsers
        // dispatch it with the keydown, or not at all — macOS Chrome
        // reserves paste for Cmd), so the image has to be read directly.
        if (!metaChord)
          this.readClipboardImage(imageRead ?? startImageRead(), flush);
      };
      navigator.clipboard.readText().then((text) => {
        if (!this._pendingPasteFlush) return; // a paste event claimed it
        // Only flush when readText actually returned content.  Some
        // browsers (Brave with sanitization) resolve with `""` instead
        // of rejecting — if we flushed on empty here, we'd close out
        // the pending paste and dispatch V with no clipboard update,
        // causing the Wayland app to paste its previous selection.
        if (text) {
          flush(textPayload(text));
          return;
        }
        imageFallback();
      }, imageFallback);
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
      // Chromium/WebKit on macOS may consume the key-up of a key that
      // triggered a Cmd menu command (Cmd+A is the common example).  Leaving
      // the press live makes the remote compositor key-repeat that key after
      // Cmd is released.  macOS Cmd chords do not autorepeat, so complete the
      // remote press/release while Meta is still depressed.  A real late
      // key-up is then ignored by the normal
      // pressedKeys guard below.  Linux/Windows retain down-until-key-up.
      if (
        pressed &&
        this.macOptionChars &&
        e.metaKey &&
        !EVDEV_MODIFIERS.has(keycode)
      ) {
        conn.sendSurfaceInput(this._surfaceId, keycode, true);
        conn.sendSurfaceInput(this._surfaceId, keycode, false);
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
      // The one real browser key event in this handler. The chord and modifier
      // keys synthesised around it carry no time, which the compositor takes as
      // "use your own clock" without disturbing the anchor.
      conn.sendSurfaceInput(this._surfaceId, keycode, pressed, e.timeStamp);
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

  /** Send one key with the mobile toolbar's armed modifier, if any. */
  private sendOneShotModifiedKey(
    key: string,
    code: string,
    shiftKey: boolean,
  ): boolean {
    if (!this._ctrlModifier && !this._altModifier) return false;
    const keycode = domKeyToEvdev(code || domCodeForLogicalKey(key));
    if (keycode === 0) return false;

    const conn = this.getConn();
    if (!conn || !this.surface || !this._displaySize) return false;

    const modifiers: number[] = [];
    if (this._ctrlModifier) modifiers.push(EVDEV_MAP.ControlLeft);
    if (this._altModifier) modifiers.push(EVDEV_MAP.AltLeft);
    if (shiftKey || characterNeedsShift(key)) {
      modifiers.push(EVDEV_MAP.ShiftLeft);
    }
    for (const modifier of modifiers) {
      conn.sendSurfaceInput(this._surfaceId, modifier, true);
    }
    conn.sendSurfaceInput(this._surfaceId, keycode, true);
    conn.sendSurfaceInput(this._surfaceId, keycode, false);
    for (const modifier of modifiers.reverse()) {
      conn.sendSurfaceInput(this._surfaceId, modifier, false);
    }
    this.setCtrlModifier(false);
    this.setAltModifier(false);
    return true;
  }

  /** Seed the hidden textarea with deletable filler and park its caret at the
   *  end. The filler is never forwarded to the Wayland client. */
  private seedIOSInputPad(): void {
    if (!this._iosInputPad || !this.textInput) return;
    if (this._iosInputRepadTimer !== null) {
      clearTimeout(this._iosInputRepadTimer);
      this._iosInputRepadTimer = null;
    }
    this.textInput.value = IOS_INPUT_PAD;
    const end = IOS_INPUT_PAD.length;
    try {
      this.textInput.setSelectionRange(end, end);
    } catch {
      // Detached/hidden fields can reject selection changes in some engines.
    }
  }

  private scheduleIOSInputRepad(): void {
    if (!this._iosInputPad) return;
    if (this._iosInputRepadTimer !== null)
      clearTimeout(this._iosInputRepadTimer);
    this._iosInputRepadTimer = setTimeout(() => {
      this._iosInputRepadTimer = null;
      this.seedIOSInputPad();
    }, 400);
  }

  private resetTextInput(): void {
    if (this._iosInputPad) this.seedIOSInputPad();
    else if (this.textInput) this.textInput.value = "";
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
    if (this.compositionActive || e.isComposing) {
      const conn = this.getConn();
      if (conn && this.surface && this._displaySize && ta) {
        const text = this._iosInputPad ? stripIOSInputPad(ta.value) : ta.value;
        const padLength = ta.value.length - text.length;
        conn.sendSurfacePreedit(
          this._surfaceId,
          text,
          Math.max(0, ta.selectionStart - padLength),
        );
      }
      return;
    }

    const textareaEnterFallback =
      !e.inputType &&
      !!ta &&
      (ta.value.includes("\n") || ta.value.includes("\r"));
    if (this._ctrlModifier || this._altModifier) {
      const modified =
        isEnterInputEvent(e) || textareaEnterFallback
          ? this.sendOneShotModifiedKey("Enter", "Enter", false)
          : e.inputType === "insertText" && e.data
            ? this.sendOneShotModifiedKey(e.data[0], "", false)
            : e.inputType === "deleteContentBackward"
              ? this.sendOneShotModifiedKey("Backspace", "Backspace", false)
              : false;
      if (modified) {
        this.resetTextInput();
        return;
      }
    }
    // iOS consumes one filler character per native Backspace repeat. Leave
    // the shortened value in place during the burst: clearing or re-seeding
    // it here makes WebKit stop after the first deletion.
    if (this._iosInputPad && e.inputType === "deleteContentBackward") {
      const conn = this.getConn();
      if (conn && this.surface && this._displaySize) {
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Backspace, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Backspace, false);
      }
      if (ta && ta.value.length <= 4) this.seedIOSInputPad();
      else this.scheduleIOSInputRepad();
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
      if (isEnterInputEvent(e)) {
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, false);
      } else if (e.inputType === "insertText" && e.data) {
        conn.sendSurfaceText(this._surfaceId, e.data);
      } else if (e.inputType === "deleteContentBackward") {
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Backspace, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Backspace, false);
      } else if (textareaEnterFallback) {
        // Last-resort shape: no useful input metadata, but the textarea
        // itself proves Return inserted a line break. Typed text and delete
        // events above remain authoritative when they do carry metadata.
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, true);
        conn.sendSurfaceInput(this._surfaceId, EVDEV_MAP.Enter, false);
      }
    }
    this.resetTextInput();
  }

  /** Handle IME composition end — send the composed text. */
  private handleCompositionEnd(e: CompositionEvent): void {
    const ta = this.textInput;
    if (!ta) return;
    this.compositionActive = false;
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
    this.resetTextInput();
    // Focus stays here.  Handing it back to the canvas would end the next
    // composition before it started, and the keydown/keyup handlers the
    // canvas would take back are already attached to this element.
  }

  /** Send synthetic key-up for every key still held.  Prevents stuck
   *  modifiers and runaway key-repeat when focus leaves the canvas. */
  private releaseAllKeys(): void {
    this._pendingPaste = null;
    this._pendingPasteFlush = null;
    this._pendingPasteAbandon = null;
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
    this.compositionActive = false;
    // Focus genuinely leaving mid-paste-chord is the one thing no
    // clipboard read or paste event will ever settle: stand the chord
    // down (its V was never pressed) before releasing what is held.
    this._pendingPasteAbandon?.();
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
    if (e.target === this.textInput) this.seedIOSInputPad();
    const conn = this.getConn();
    if (!conn || !this.surface || !this._displaySize) return;
    conn.sendSurfaceFocus(this._surfaceId);
  }
}
