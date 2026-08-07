import type { BlitSurface, ConnectionId } from "./types";
import {
  SURFACE_FRAME_FLAG_KEYFRAME,
  SURFACE_FRAME_CODEC_MASK,
  SURFACE_FRAME_CODEC_AV1,
} from "./types";

/**
 * Every Blit encoder produces full-range BT.601 (sRGB primaries/transfer).
 * Most streams also say so in-band (H.264 VUI, AV1 color_config); this
 * config hint covers the ones whose encoder cannot write it (openh264)
 * and matches the rest.  Without it a decoder assumes limited range and
 * lifts every black to gray.
 */
const FULL_RANGE_BT601: VideoColorSpaceInit = {
  primaries: "bt709",
  transfer: "iec61966-2-1",
  matrix: "smpte170m",
  fullRange: true,
};

/**
 * Frame-ready callback.  Listeners receive only the surface ID; they should
 * call {@link SurfaceStore.getCanvas} to obtain the shared backing canvas
 * that already contains the latest rendered frame.
 */
export type SurfaceFrameCallback = (surfaceId: number) => void;

export type SurfaceEventCallback = (
  surfaces: ReadonlyMap<number, BlitSurface>,
) => void;

/** Timestamped record of an incoming surface video frame. */
export interface SurfaceFrameSample {
  /** `performance.now()` when the frame arrived. */
  t: number;
  /** Encoded frame payload size in bytes. */
  bytes: number;
  /** Whether this was a keyframe. */
  key: boolean;
}

type SurfaceCodec = "h264" | "av1";

interface DecoderEntry {
  decoder: VideoDecoder;
  codec: SurfaceCodec;
  pendingKeyframe: boolean;
  /** True once a keyframe request has been sent for the current
   *  `pendingKeyframe` episode.  Reset when a keyframe successfully
   *  decodes.  Prevents every errored delta frame from firing a fresh
   *  keyframe request (which over the wire is a full SURFACE_SUBSCRIBE
   *  — each one resets server-side pacing/burst state). */
  keyframeRequested: boolean;
  /** Last H.264 codec string (e.g. "avc1.42001e"), used to avoid
   *  reconfiguring on every keyframe.  We compare the codec string
   *  (profile/compat/level) rather than raw SPS bytes because some
   *  encoders rotate sps_id on each IDR, which changes the AVCC
   *  description without affecting decode parameters.  Unnecessary
   *  reconfigures orphan in-flight VideoFrame objects (GC warning)
   *  and can stall the decode pipeline. */
  lastCodecString: string | null;
  /** Last AVCC description passed to configure(). */
  lastDescription: ArrayBuffer | null;
  /** Dimensions of the frame that triggered the most recent configure().
   *  A resolution-only resize keeps the same profile/level (and thus the
   *  same codec string), so the cs comparison above can't detect it — but
   *  the SPS embedded in the description carries the new resolution and
   *  the decoder needs to pick it up, otherwise it errors on the first
   *  post-resize keyframe with "Decoding error" and closes. */
  lastConfiguredWidth: number;
  lastConfiguredHeight: number;
}

interface CanvasEntry {
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
}

/** Per-surface presenter state.  Queues decoded frames so presentation
 *  happens at vsync boundaries (one `requestAnimationFrame` per surface)
 *  rather than at arbitrary decoder-output moments.
 *
 *  Once a surface has been producing frames continuously for a moment the
 *  presenter switches from "draw whatever arrived, newest wins" to
 *  scheduling each frame against its capture-time PTS
 *  (`S2C_SURFACE_FRAME.timestamp`, stamped at compositor-commit time — see
 *  docs/protocol.md).  That is the only clock in the pipeline taken before
 *  encode and transport, so replaying against it cancels the jitter both
 *  add.  Without it, a frame that took 4 ms longer to encode is drawn 4 ms
 *  late, and at 60 fps into a 60 Hz display that is the difference between
 *  one frame per refresh and an endless 2-0-1-2-0 cadence. */
interface SurfacePresenter {
  /** Decoded VideoFrames waiting to be presented.  Bounded at
   *  {@link SurfaceStore.PRESENT_QUEUE_MAX} (or
   *  {@link SurfaceStore.PRESENT_QUEUE_MAX_SMOOTHED} once scheduling is
   *  engaged) — each entry pins a decoded buffer in the codec's frame
   *  pool, so an undrained queue (hidden tab, throttled rAF) would
   *  otherwise grow until the renderer OOMs. */
  queue: VideoFrame[];
  /** Pending `requestAnimationFrame` handle, or null. */
  rafId: number | null;
  /** True after the first frame has been presented.  The first frame
   *  paints synchronously to minimise time-to-first-pixel. */
  initialized: boolean;
  /** Recent `arrival - pts` samples (ms), covering roughly
   *  {@link SurfaceStore.OFFSET_WINDOW_MS} of stream.  Both the fast-path
   *  baseline and the presentation point are quantiles of this one
   *  distribution, which is what makes the scheduler robust in both
   *  directions: a burst frame arriving early is a low outlier and a late
   *  frame is a high outlier, and a quantile ignores each without needing
   *  a separate clamp or leak rule for either.
   *
   *  The absolute values are meaningless — they carry the arbitrary offset
   *  between the server's `elapsed_ms()` epoch and `performance.now()` —
   *  but that constant cancels out, since every number derived here is a
   *  difference or is added straight back to a PTS. */
  offsets: number[];
  /** {@link SurfaceStore.FAST_QUANTILE} of {@link offsets}: the fastest the
   *  path has recently shown itself to be, outlier-resistant.  Only used as
   *  the reference for how much latency presentation is adding. */
  fastOffsetMs: number;
  /** The offset presentation actually runs at: a frame is drawn at
   *  `pts + presentOffsetMs`.  Slewed toward its target rather than set to
   *  it, because moving it shifts every future due time at once. */
  presentOffsetMs: number;
  /** PTS (ms) of the previous arrival, for rewind/wrap detection. */
  lastPtsMs: number | null;
  /** Consecutive arrivals that looked like part of one continuous stream. */
  steadyRun: number;
  /** EWMA of the stream's own frame interval, from PTS deltas (ms).  The
   *  source runs at whatever rate the server paces this surface — the
   *  client's display rate, up to 480 Hz — so the number of frames the
   *  playout margin spans is not a constant. */
  frameIntervalMs: number;
  /** True while presentation is scheduled off PTS.  False for sparse or
   *  interactive repaints, which present as soon as they decode. */
  smoothing: boolean;
}

/** Nearest-rank quantile of `samples`, which is left unmodified.  `q` is in
 *  [0, 1]; an empty set is 0. */
function quantile(samples: readonly number[], q: number): number {
  const n = samples.length;
  if (n === 0) return 0;
  const sorted = Array.from(samples).sort((a, b) => a - b);
  return sorted[Math.min(n - 1, Math.max(0, Math.ceil(q * n) - 1))];
}

function codecFromFlags(flags: number): SurfaceCodec {
  const bits = flags & SURFACE_FRAME_CODEC_MASK;
  if (bits === SURFACE_FRAME_CODEC_AV1) return "av1";
  return "h264";
}

/** Gracefully shut down a decoder, ensuring every in-flight VideoFrame
 *  reaches the output callback (which calls frame.close()) before the
 *  decoder is destroyed.
 *
 *  Chromium's reset()/close() drops internally-queued VideoFrame objects
 *  without calling .close(), triggering the "VideoFrame was garbage
 *  collected without being closed" console warning and potentially
 *  stalling the frame buffer pool.  flush() drains the queue through
 *  the normal output path first.
 *
 *  The flush is fire-and-forget — callers continue immediately.  The
 *  output callback still closes every frame via its finally block even
 *  after the decoder entry has been removed from the map. */
function safeClose(decoder: VideoDecoder): void {
  try {
    if (decoder.state === "configured") {
      const close = () => {
        try {
          if (decoder.state !== "closed") decoder.close();
        } catch {
          /* already closed */
        }
      };
      decoder.flush().then(close, close);
    } else if (decoder.state !== "closed") {
      decoder.close();
    }
  } catch {
    // Already closed or in an invalid state.
  }
}

/**
 * Derive the H.264 WebCodecs codec string from the SPS NAL unit so it
 * matches the actual profile/level the encoder produced.
 */
function h264CodecStringFromSps(sps: Uint8Array): string | null {
  if (sps.length < 4) return null;
  const profile = sps[1];
  const compat = sps[2];
  const level = sps[3];
  const hex = (b: number) => b.toString(16).padStart(2, "0");
  return `avc1.${hex(profile)}${hex(compat)}${hex(level)}`;
}

// ---------------------------------------------------------------------------
// Annex B → length-prefixed NAL conversion
//
// The server sends Annex B bitstreams (start-code delimited NAL units).
// WebCodecs defaults to length-prefixed containers (AVCC for H.264).
// The `avc.format` annexb hint is not universally supported (macOS
// VideoToolbox rejects with -12909, Windows Media Foundation doesn't
// support the option at all), so we convert Annex B →
// 4-byte-length-prefixed on every frame.
// ---------------------------------------------------------------------------

/** Split Annex B byte stream into individual NAL units (without start codes). */
function splitNALs(data: Uint8Array): Uint8Array[] {
  const nals: Uint8Array[] = [];
  const len = data.length;
  let i = 0;

  // Advance past the first start code.
  while (i < len - 3) {
    if (data[i] === 0 && data[i + 1] === 0) {
      if (data[i + 2] === 1) {
        i += 3;
        break;
      }
      if (data[i + 2] === 0 && i + 3 < len && data[i + 3] === 1) {
        i += 4;
        break;
      }
    }
    i++;
  }

  let nalStart = i;
  while (i < len) {
    if (
      i + 2 < len &&
      data[i] === 0 &&
      data[i + 1] === 0 &&
      (data[i + 2] === 1 ||
        (data[i + 2] === 0 && i + 3 < len && data[i + 3] === 1))
    ) {
      if (i > nalStart) nals.push(data.subarray(nalStart, i));
      i += data[i + 2] === 1 ? 3 : 4;
      nalStart = i;
    } else {
      i++;
    }
  }
  if (nalStart < len) nals.push(data.subarray(nalStart, len));
  return nals;
}

/** Replace Annex B start codes with 4-byte big-endian length prefixes. */
function toLengthPrefixed(nals: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const n of nals) total += 4 + n.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const n of nals) {
    const l = n.length;
    out[off] = (l >>> 24) & 0xff;
    out[off + 1] = (l >>> 16) & 0xff;
    out[off + 2] = (l >>> 8) & 0xff;
    out[off + 3] = l & 0xff;
    out.set(n, off + 4);
    off += 4 + l;
  }
  return out;
}

/** H.264 NAL unit type (5 low bits of the first byte). */
function h264NalType(nal: Uint8Array): number {
  return nal[0] & 0x1f;
}

/**
 * Build an AVCDecoderConfigurationRecord (ISO 14496-15 §5.3.3.1)
 * from raw SPS and PPS NAL units (without start codes).
 */
function buildAvccDescription(sps: Uint8Array, pps: Uint8Array): ArrayBuffer {
  // Parse profile/level from SPS NAL (bytes 1-3 after the NAL type byte).
  const profileIdc = sps[1];
  const profileCompat = sps[2];
  const levelIdc = sps[3];

  const size = 6 + 1 + 2 + sps.length + 1 + 2 + pps.length;
  const buf = new ArrayBuffer(size);
  const v = new DataView(buf);
  const u = new Uint8Array(buf);
  let o = 0;

  v.setUint8(o++, 1); // configurationVersion
  v.setUint8(o++, profileIdc); // AVCProfileIndication
  v.setUint8(o++, profileCompat); // profile_compatibility
  v.setUint8(o++, levelIdc); // AVCLevelIndication
  v.setUint8(o++, 0xff); // 6 reserved bits (111111) + lengthSizeMinusOne=3
  v.setUint8(o++, 0xe1); // 3 reserved bits (111) + numOfSequenceParameterSets=1
  v.setUint16(o, sps.length); // sequenceParameterSetLength
  o += 2;
  u.set(sps, o); // sequenceParameterSetNALUnit
  o += sps.length;
  v.setUint8(o++, 1); // numOfPictureParameterSets
  v.setUint16(o, pps.length); // pictureParameterSetLength
  o += 2;
  u.set(pps, o); // pictureParameterSetNALUnit

  return buf;
}

export class SurfaceStore {
  private surfaces = new Map<number, BlitSurface>();
  private connectionId: ConnectionId = "";
  private decoders = new Map<number, DecoderEntry>();
  private canvases = new Map<number, CanvasEntry>();
  private frameListeners = new Set<SurfaceFrameCallback>();
  private cursorShapes = new Map<number, string>();
  private encoderNames = new Map<number, string>();
  private codecStrings = new Map<number, string>();
  private cursorListeners = new Set<
    (surfaceId: number, shape: string) => void
  >();
  private eventListeners = new Set<SurfaceEventCallback>();
  private _diag = {
    received: 0,
    decoded: 0,
    output: 0,
    presented: 0,
    dropped: 0,
    errors: 0,
  };
  private _diagTimer: ReturnType<typeof setInterval> | null = null;
  private _visibilityHandler: (() => void) | null = null;

  // Per-surface diagnostics exposed to the debug panel.
  private _surfaceFrameSamples = new Map<number, SurfaceFrameSample[]>();
  /** Timestamps of decoded output frames (for computing output fps). */
  private _surfaceOutputSamples = new Map<number, number[]>();
  /** Cumulative per-surface drop/error counters. */
  private _surfaceDrops = new Map<number, number>();
  private _surfaceErrors = new Map<number, number>();

  private static readonly FRAME_SAMPLE_MAX = 500;
  private static readonly OUTPUT_SAMPLE_MAX = 500;
  /** Max decoded frames a presenter may hold between rAF ticks while
   *  presenting newest-wins (no scheduling, so depth is pure overflow
   *  slack). */
  private static readonly PRESENT_QUEUE_MAX = 2;
  /** Fastest frame interval the pipeline can legitimately produce.  The
   *  server clamps a client's reported display rate to `MAX_DISPLAY_FPS`
   *  (480) and paces surfaces at it, so nothing real arrives faster.
   *  Flooring the learned interval here — rather than capping depth — is
   *  what keeps a degenerate PTS stream from inflating the queue. */
  private static readonly MIN_FRAME_INTERVAL_MS = 1000 / 480;
  /** Ceiling on held frames once PTS scheduling is engaged.
   *
   *  Deliberately sized so it never binds: the margin can reach
   *  {@link PRESENT_DELAY_MAX_MS} and frames arrive no faster than
   *  {@link MIN_FRAME_INTERVAL_MS}, so the most any real stream can need is
   *  `50 / 2.08 = 24`, plus the two of slack {@link smoothedQueueCap} adds.
   *  Anything that would exceed this is a broken frame interval, and that
   *  is already handled by the floor above — so a stream is never made to
   *  drop frames just because it runs at a high rate. */
  private static readonly PRESENT_QUEUE_MAX_SMOOTHED =
    Math.ceil(50 / (1000 / 480)) + 2;
  /** Consecutive continuous arrivals before PTS scheduling engages.  Long
   *  enough that a couple of repaints from a click don't trip it, short
   *  enough that it is running well inside the first second of playback. */
  private static readonly SMOOTHING_ENGAGE_FRAMES = 8;
  /** An arrival or PTS gap longer than this ends the current stream
   *  episode: the surface went idle, so the next frame is a fresh
   *  interaction and must paint immediately rather than wait out a
   *  playout margin computed for the previous episode. */
  private static readonly STREAM_GAP_MS = 250;
  /** Hard ceiling on latency the presenter is allowed to add.  Smoothing
   *  past this trades away more interactivity than juddering costs. */
  private static readonly PRESENT_DELAY_MAX_MS = 50;
  /** How much stream the offset distribution covers.  Expressed in time
   *  rather than frames so the horizon is the same at 24 and 240 fps —
   *  too short and the schedule chases noise, too long and it responds
   *  sluggishly to a link that genuinely changed (a Wi-Fi roam). */
  private static readonly OFFSET_WINDOW_MS = 1000;
  private static readonly OFFSET_WINDOW_MIN = 60;
  private static readonly OFFSET_WINDOW_MAX = 480;
  /** Quantile of the offset distribution that presentation targets.  The
   *  remaining tail is deliberately not buffered for: those frames arrive
   *  overdue and are skipped to the newest due one, which costs nothing and
   *  is what should happen to a rare outlier. */
  private static readonly PRESENT_QUANTILE = 0.95;
  /** Low quantile taken as "the fastest this path goes".  Not the strict
   *  minimum: a burst frame is captured later but shipped immediately
   *  behind its predecessor, so its transit genuinely is shorter, and a
   *  minimum would take that one-off as a permanently faster link.  A
   *  quantile ignores it for the same reason the high end ignores a single
   *  late frame — no separate clamp or leak rule needed at either end. */
  private static readonly FAST_QUANTILE = 0.02;
  /** Maximum movement of {@link SurfacePresenter.presentOffsetMs} per frame.
   *
   *  Moving it *is* a latency change — every future due time shifts with
   *  it — so stepping injects exactly the timing discontinuity this
   *  scheduler exists to remove.  Slewing turns it into a sub-perceptual
   *  rate nudge: 2 ms against a 16.7 ms frame is 12% while it moves.
   *
   *  Shrinking is proportional rather than a flat crawl.  A flat
   *  0.25 ms/frame took ~5 s to unwind a single stall, and because video
   *  rides a reliable ordered channel every lost packet is such a stall —
   *  so a lossy link sat near the latency ceiling permanently, strictly
   *  worse than not scheduling at all, for exactly the users this exists
   *  to help.  Proportional decay unwinds the same stall in ~0.5 s. */
  private static readonly MARGIN_GROW_MS = 2;
  private static readonly MARGIN_SHRINK_MS = 0.25;
  private static readonly MARGIN_SHRINK_FRAC = 0.08;
  /** Fallback display refresh interval before any rAF delta is measured. */
  private static readonly DEFAULT_REFRESH_MS = 1000 / 60;
  /** Bounds on an rAF delta that counts as a refresh period: 1000 Hz to
   *  10 Hz.  Outside this it is a stalled or backgrounded tick, not a
   *  display rate — see {@link noteRafInterval} for why both ends are
   *  this permissive. */
  private static readonly RAF_DELTA_MIN_MS = 1;
  private static readonly RAF_DELTA_MAX_MS = 100;

  /** EWMA of observed rAF intervals — the display's refresh period.  Used
   *  to round each frame's due time to the nearest refresh instead of
   *  systematically deferring anything due a hair after this tick. */
  private refreshMs = SurfaceStore.DEFAULT_REFRESH_MS;
  private lastRafMs: number | null = null;

  /** Per-surface presenter: queues decoded frames and paints them at vsync
   *  via rAF — newest-wins while the surface is idle or interactive,
   *  scheduled against capture-time PTS once it is streaming continuously.
   *  See {@link SurfacePresenter}. */
  private presenters = new Map<number, SurfacePresenter>();

  /**
   * Callback to send a surface ACK to the server.  Injected by the
   * connection layer so the store can defer ACKs when the decode queue
   * is deep (backpressure).
   */
  private _ackSender: ((surfaceId: number) => void) | null = null;

  /**
   * Callback to request a keyframe from the server (re-subscribe).
   * Called when the decoder enters an error state and needs a clean
   * reference point to recover.
   */
  private _keyframeSender: ((surfaceId: number) => void) | null = null;

  /** Install the ACK sender callback (called once by BlitConnection). */
  setAckSender(fn: (surfaceId: number) => void): void {
    this._ackSender = fn;
  }

  /** Install the keyframe-request callback (called once by BlitConnection). */
  setKeyframeSender(fn: (surfaceId: number) => void): void {
    this._keyframeSender = fn;
  }

  private sendAck(surfaceId: number): void {
    this._ackSender?.(surfaceId);
  }

  /** Send an ACK unconditionally — used by the connection layer's catch
   *  path when handleSurfaceFrame throws before it can ACK itself. */
  sendAckFallback(surfaceId: number): void {
    this._ackSender?.(surfaceId);
  }

  /**
   * Monotonically increasing counter bumped on every disconnect.  Consumers
   * (e.g. {@link BlitSurfaceCanvas}) compare their last-seen generation to
   * detect reconnects and re-subscribe for video frames.
   */
  private _generation = 0;
  get generation(): number {
    return this._generation;
  }

  /**
   * Whether the browser can decode surface video frames (WebCodecs + secure
   * context).  Checked eagerly at construction time so callers can skip
   * surface subscriptions that would only drive the server encoder for
   * nothing (and risk crashing it).
   */
  readonly canDecodeVideo: boolean;

  /**
   * Non-null when surface video decoding is unavailable (e.g. insecure
   * context or missing WebCodecs).  UI components should display this
   * message instead of a blank canvas.
   */
  videoUnavailableReason: string | null = null;

  constructor() {
    const hasWebCodecs =
      typeof VideoDecoder !== "undefined" &&
      typeof EncodedVideoChunk !== "undefined";
    const isSecure = typeof window === "undefined" || window.isSecureContext;
    this.canDecodeVideo = hasWebCodecs && isSecure;
    if (!this.canDecodeVideo) {
      const insecure = typeof window !== "undefined" && !window.isSecureContext;
      this.videoUnavailableReason = insecure
        ? "Secure context required (HTTPS or localhost)"
        : "WebCodecs API not available in this browser";
    }
    this._diagTimer = setInterval(() => {
      const d = this._diag;
      if (d.received > 0) {
        console.log(
          `[blit-video] recv=${d.received} decoded=${d.decoded} output=${d.output} presented=${d.presented} dropped=${d.dropped} errors=${d.errors} listeners=${this.frameListeners.size}`,
        );
        // Every counter here is per-window; one that misses this reset
        // accumulates for the process lifetime and silently dwarfs the
        // others, which is exactly what `presented` did.
        d.received =
          d.decoded =
          d.output =
          d.presented =
          d.dropped =
          d.errors =
            0;
      }
    }, 5000);
    if (typeof document !== "undefined") {
      // Drain presenter queues the moment the tab goes hidden: any pending
      // rAF will never fire while hidden, and enqueueFrame's hidden path
      // only covers frames that arrive after this point.
      this._visibilityHandler = () => {
        if (document.visibilityState === "hidden") {
          this.flushAllPresenters();
        }
      };
      document.addEventListener("visibilitychange", this._visibilityHandler);
    }
  }

  onFrame(listener: SurfaceFrameCallback): () => void {
    this.frameListeners.add(listener);
    return () => this.frameListeners.delete(listener);
  }

  onChange(listener: SurfaceEventCallback): () => void {
    this.eventListeners.add(listener);
    return () => this.eventListeners.delete(listener);
  }

  getSurfaces(): ReadonlyMap<number, BlitSurface> {
    return this.surfaces;
  }

  /** Debug info about all known surfaces (encoder, codec, size, decode stats). */
  getDebugStats(): {
    surfaceId: number;
    codec: string;
    encoder: string;
    width: number;
    height: number;
    /** Ring buffer of recent incoming frame samples (for timeline graph). */
    frameSamples: SurfaceFrameSample[];
    /** Ring buffer of decoded-output timestamps (for fps computation). */
    outputSamples: readonly number[];
    /** Cumulative dropped frame count. */
    dropped: number;
    /** Cumulative decode error count. */
    errors: number;
    /** Current WebCodecs decode queue depth. */
    queueDepth: number;
  }[] {
    const result: ReturnType<SurfaceStore["getDebugStats"]> = [];
    for (const [id, surface] of this.surfaces) {
      // Skip subsurfaces — they are composited into their parent and
      // don't have their own encoder or codec.
      if (surface.parentId !== 0) continue;
      const entry = this.decoders.get(id);
      let queueDepth = 0;
      try {
        queueDepth =
          entry && entry.decoder.state === "configured"
            ? entry.decoder.decodeQueueSize
            : 0;
      } catch {
        // decoder may be closed
      }
      result.push({
        surfaceId: id,
        codec: entry?.codec ?? "",
        encoder: this.encoderNames.get(id) ?? "",
        width: surface.width,
        height: surface.height,
        frameSamples: this._surfaceFrameSamples.get(id) ?? [],
        outputSamples: this._surfaceOutputSamples.get(id) ?? [],
        dropped: this._surfaceDrops.get(id) ?? 0,
        errors: this._surfaceErrors.get(id) ?? 0,
        queueDepth,
      });
    }
    return result;
  }

  getSurface(surfaceId: number): BlitSurface | undefined {
    return this.surfaces.get(surfaceId);
  }

  /** Return the shared backing canvas for a surface — the server sends
   *  one stream per `(cid, sid)`, so a single decoder and canvas per
   *  surface suffice.  The canvas is never attached to the DOM;
   *  callers blit from it into their visible canvases. */
  getCanvas(surfaceId: number): HTMLCanvasElement | null {
    return this.canvases.get(surfaceId)?.canvas ?? null;
  }

  setConnectionId(id: ConnectionId): void {
    this.connectionId = id;
  }

  handleSurfaceCreated(
    surfaceId: number,
    parentId: number,
    width: number,
    height: number,
    title: string,
    appId: string,
  ): void {
    this.surfaces.set(surfaceId, {
      connectionId: this.connectionId,
      surfaceId,
      parentId,
      title,
      appId,
      width,
      height,
    });
    // Don't create a canvas yet — canvases are per-subscription now,
    // keyed by sub_id, and we don't have one until a view subscribes.
    this.emitChange();
  }

  handleSurfaceDestroyed(surfaceId: number): void {
    this.surfaces.delete(surfaceId);
    this.encoderNames.delete(surfaceId);
    this.codecStrings.delete(surfaceId);
    this._surfaceFrameSamples.delete(surfaceId);
    this._surfaceOutputSamples.delete(surfaceId);
    this._surfaceDrops.delete(surfaceId);
    this._surfaceErrors.delete(surfaceId);
    this.discardPresenter(surfaceId);
    const entry = this.decoders.get(surfaceId);
    if (entry) safeClose(entry.decoder);
    this.decoders.delete(surfaceId);
    this.canvases.delete(surfaceId);
    this.emitChange();
  }

  handleSurfaceFrame(
    surfaceId: number,
    _timestamp: number,
    flags: number,
    width: number,
    height: number,
    data: Uint8Array,
  ): void {
    this._diag.received++;
    const isKey = (flags & SURFACE_FRAME_FLAG_KEYFRAME) !== 0;

    // Per-surface frame timeline sample.
    let samples = this._surfaceFrameSamples.get(surfaceId);
    if (!samples) {
      samples = [];
      this._surfaceFrameSamples.set(surfaceId, samples);
    }
    samples.push({ t: performance.now(), bytes: data.length, key: isKey });
    if (samples.length > SurfaceStore.FRAME_SAMPLE_MAX)
      samples.splice(0, samples.length - SurfaceStore.FRAME_SAMPLE_MAX);

    const codec = codecFromFlags(flags);

    let entry = this.decoders.get(surfaceId);
    if (!entry || entry.codec !== codec) {
      if (entry) {
        safeClose(entry.decoder);
      }
      this.decoders.delete(surfaceId);
      this.initDecoder(surfaceId, codec, width, height);
      entry = this.decoders.get(surfaceId);
    }
    if (!entry) {
      // No decoder — ACK immediately so the server doesn't stall.
      this.sendAck(surfaceId);
      return;
    }

    if (entry.pendingKeyframe && !isKey) {
      this._diag.dropped++;
      this._surfaceDrops.set(
        surfaceId,
        (this._surfaceDrops.get(surfaceId) ?? 0) + 1,
      );
      // Dropped frame — ACK immediately.
      this.sendAck(surfaceId);
      return;
    }
    entry.pendingKeyframe = false;
    // A keyframe landed (or at least was accepted for decode) — future
    // decode errors will legitimately need a fresh keyframe request, so
    // drop the "already asked" latch.
    entry.keyframeRequested = false;

    const surface = this.surfaces.get(surfaceId);
    // Frame dimensions are the *stream* size, which the server downscales
    // per client (per_client_encode_target), while surface.width/height
    // must stay the *native* composite size from SurfaceResized — pointer
    // coordinates are scaled by surface.width, so overwriting it with a
    // downscaled stream size makes every pointer position land short of
    // the cursor.  Frame dims only seed a surface still at the 0×0 the
    // compositor reports in SurfaceCreated before the first buffer commit.
    if (
      surface &&
      (surface.width === 0 || surface.height === 0) &&
      width > 0 &&
      height > 0
    ) {
      // Mutate in place so downstream <For> children keep their object
      // identity (no remount → no decoder race).  Subscribers read the
      // fresh fields on the next emitChange-driven recomputation.
      surface.width = width;
      surface.height = height;
      this.emitChange();
    }

    this.ensureCanvas(surfaceId, width, height);

    try {
      let frameData: Uint8Array;

      if (codec === "av1") {
        // AV1: raw OBU "low-overhead bitstream format" per WebCodecs spec.
        // No description, no NAL splitting, no length-prefix — pass through.
        frameData = data;
      } else {
        // H.264: Annex B → AVCC length-prefixed + description
        const nals = splitNALs(data);
        if (isKey) {
          let sps: Uint8Array | undefined;
          let pps: Uint8Array | undefined;
          const vclNals: Uint8Array[] = [];
          for (const nal of nals) {
            const t = h264NalType(nal);
            if (t === 7) sps = nal;
            else if (t === 8) pps = nal;
            else vclNals.push(nal);
          }
          if (sps && pps) {
            const description = buildAvccDescription(sps, pps);
            const cs = h264CodecStringFromSps(sps) ?? "avc1.42001e";
            const dimsChanged =
              width !== entry.lastConfiguredWidth ||
              height !== entry.lastConfiguredHeight;
            if (cs !== entry.lastCodecString || dimsChanged) {
              entry.lastCodecString = cs;
              entry.lastDescription = description;
              entry.lastConfiguredWidth = width;
              entry.lastConfiguredHeight = height;
              // If the decoder already has queued work, calling
              // configure() directly resets its state and orphans any
              // in-flight VideoFrame objects — Chromium then logs
              // "A VideoFrame was garbage collected without being
              // closed" and eventually exhausts its frame pool,
              // stalling decode.  Queue a flush() first so pending
              // frames drain through the output callback (which
              // closes them) before the reset.  WebCodecs processes
              // control messages in order, so the subsequent
              // configure() and decode() of the current keyframe
              // simply run after the flush completes.
              if (entry.decoder.state === "configured") {
                entry.decoder.flush().catch(() => {
                  /* flush rejected — decoder likely closed */
                });
              }
              entry.decoder.configure({
                codec: cs,
                optimizeForLatency: true,
                description,
                colorSpace: FULL_RANGE_BT601,
              });
            }
          }
          // In AVCC mode, parameter-set NALs (SPS/PPS) belong in the
          // description — strip them from the frame data.
          frameData = toLengthPrefixed(vclNals.length > 0 ? vclNals : nals);
        } else {
          frameData = toLengthPrefixed(nals);
        }
      }

      // Guard: don't decode if the decoder was never configured
      // (e.g., old server without VPS/SPS/PPS or HVCC prefix).
      if (entry.decoder.state !== "configured") {
        this._diag.dropped++;
        this.sendAck(surfaceId);
        return;
      }

      const chunk = new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: _timestamp * 1000,
        data: frameData,
      });
      entry.decoder.decode(chunk);
      this._diag.decoded++;

      // ACK immediately — the server already paces delivery via its own
      // inflight window and time-based send interval.  Deferring ACKs
      // until the output callback adds decode latency to the effective
      // round-trip, starving the server's pacing window on high-latency
      // or software-decode paths.
      this.sendAck(surfaceId);
    } catch (e) {
      console.warn(
        "[blit] surface decode error:",
        surfaceId,
        codec,
        `${width}x${height}`,
        isKey ? "key" : "delta",
        `${data.length}B`,
        e,
      );
      if (entry) entry.pendingKeyframe = true;
      this._diag.errors++;
      this._surfaceErrors.set(
        surfaceId,
        (this._surfaceErrors.get(surfaceId) ?? 0) + 1,
      );
      // Error — ACK immediately so the server doesn't permanently stall.
      this.sendAck(surfaceId);
      // Ask the server for a keyframe so the decoder can recover.
      // Fire at most once per pendingKeyframe episode — each request is
      // a SURFACE_SUBSCRIBE on the wire and resets server-side pacing.
      // The flag is cleared when a keyframe decodes successfully.
      if (entry && !entry.keyframeRequested) {
        entry.keyframeRequested = true;
        this._keyframeSender?.(surfaceId);
      }
    }
  }

  handleSurfaceTitle(surfaceId: number, title: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, title });
      this.emitChange();
    }
  }

  handleSurfaceCursor(surfaceId: number, shape: string): void {
    this.cursorShapes.set(surfaceId, shape);
    // Notify cursor listeners without triggering a full change cycle.
    for (const listener of this.cursorListeners) {
      try {
        listener(surfaceId, shape);
      } catch {}
    }
  }

  /** Get the current CSS cursor for a surface. */
  getCursor(surfaceId: number): string {
    return this.cursorShapes.get(surfaceId) ?? "default";
  }

  /** Register a callback for cursor shape changes. Returns unsubscribe fn. */
  onCursor(listener: (surfaceId: number, shape: string) => void): () => void {
    this.cursorListeners.add(listener);
    return () => {
      this.cursorListeners.delete(listener);
    };
  }

  handleSurfaceEncoder(surfaceId: number, rawPayload: string): void {
    // Format: "encoder-name\0codec-string" (NUL-separated).
    const nul = rawPayload.indexOf("\0");
    const encoderName = nul >= 0 ? rawPayload.slice(0, nul) : rawPayload;
    const codecString = nul >= 0 ? rawPayload.slice(nul + 1) : null;
    this.encoderNames.set(surfaceId, encoderName);
    if (codecString) {
      const prev = this.codecStrings.get(surfaceId);
      this.codecStrings.set(surfaceId, codecString);
      // A rebuilt session can change the stream's level mid-subscription —
      // resizing a pane across an AV1 level boundary (~2254px wide at
      // 2094 tall flips av01.0.09M ↔ av01.0.13M) re-announces the codec
      // string, and a live decoder configured for the lower level rejects
      // the higher-level stream that follows.  H.264 re-derives its config
      // from in-band SPS; AV1 has no in-band trigger, so reconfigure here.
      // The announcement always precedes the new session's opening
      // keyframe, and pendingKeyframe drops any stale deltas in between.
      const entry = this.decoders.get(surfaceId);
      if (
        prev !== undefined &&
        prev !== codecString &&
        entry &&
        entry.codec === "av1" &&
        entry.decoder.state === "configured"
      ) {
        // Flush first so in-flight frames drain through the output
        // callback before the reset (same reasoning as the H.264
        // reconfigure path).
        entry.decoder.flush().catch(() => {
          /* flush rejected — decoder likely closed */
        });
        try {
          entry.decoder.configure({
            codec: codecString,
            optimizeForLatency: true,
            colorSpace: FULL_RANGE_BT601,
          });
          entry.pendingKeyframe = true;
        } catch (e) {
          console.warn(
            "[blit] surface decoder reconfigure failed:",
            surfaceId,
            codecString,
            e,
          );
        }
      }
    }
  }

  handleSurfaceAppId(surfaceId: number, appId: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, appId });
      this.emitChange();
    }
  }

  handleSurfaceResized(surfaceId: number, width: number, height: number): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface && (surface.width !== width || surface.height !== height)) {
      // Only emit a change for significant resizes (> 1px) to avoid
      // triggering a BSP re-render → ResizeObserver → resize feedback loop
      // from sub-pixel rounding in the compositor's physical↔logical
      // conversion.  The initial 0x0 → real size always emits.
      const significant =
        surface.width === 0 ||
        surface.height === 0 ||
        Math.abs(surface.width - width) > 1 ||
        Math.abs(surface.height - height) > 1;
      surface.width = width;
      surface.height = height;
      // Flush any queued frames from the old resolution.  Without this,
      // stale VideoFrames occupy the decode buffer pool and the presenter
      // draws a wrong-sized frame, stalling the pipeline.  Discarding
      // resets `initialized` so the first frame at the new resolution
      // paints synchronously (fast path).
      this.discardPresenter(surfaceId);
      // Proactively ask the server for a keyframe at the new dimensions
      // and drop any delta frames that arrive before it.  The decoder
      // must be reconfigured with the new SPS/PPS (H.264) or size hint
      // anyway, so a keyframe is mandatory; waiting passively for the
      // server to produce one adds an extra round-trip to the recovery.
      const entry = this.decoders.get(surfaceId);
      if (entry) {
        entry.pendingKeyframe = true;
        if (!entry.keyframeRequested) {
          entry.keyframeRequested = true;
          this._keyframeSender?.(surfaceId);
        }
      }
      if (significant) this.emitChange();
    }
  }

  /**
   * Full teardown on transport disconnect.  Clears all surfaces, canvases,
   * and decoders so the UI reflects the disconnected state immediately.
   * The server's initial message sequence after reconnect
   * ({@link reset} via S2C_HELLO, then S2C_SURFACE_CREATED) will rebuild
   * the surface list.  The generation counter is bumped so
   * {@link BlitSurfaceCanvas} instances detect the reconnect and
   * re-subscribe for video frames.
   */
  handleDisconnect(): void {
    this.discardAllPresenters();
    for (const entry of this.decoders.values()) {
      safeClose(entry.decoder);
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    this.encoderNames.clear();
    this.codecStrings.clear();
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._surfaceDrops.clear();
    this._surfaceErrors.clear();
    this._generation++;
    this.emitChange();
  }

  /**
   * Full surface reset — called when S2C_HELLO signals a (possibly new)
   * server instance.  Clears all surfaces, canvases, and decoders.  The
   * server's initial message sequence will rebuild the surface list via
   * individual S2C_SURFACE_CREATED messages.
   */
  reset(): void {
    this.discardAllPresenters();
    for (const entry of this.decoders.values()) {
      safeClose(entry.decoder);
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    this.encoderNames.clear();
    this.codecStrings.clear();
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._surfaceDrops.clear();
    this._surfaceErrors.clear();
    this._generation++;
    this.emitChange();
  }

  /**
   * Full teardown — only called when the connection is permanently disposed.
   */
  destroy(): void {
    if (this._diagTimer !== null) {
      clearInterval(this._diagTimer);
      this._diagTimer = null;
    }
    if (this._visibilityHandler !== null) {
      document.removeEventListener("visibilitychange", this._visibilityHandler);
      this._visibilityHandler = null;
    }
    this.reset();
  }

  // -----------------------------------------------------------------------
  // Private
  // -----------------------------------------------------------------------

  /** Push a decoded frame into the surface's presenter, paint the very
   *  first one synchronously, and schedule the next vsync tick. */
  private enqueueFrame(surfaceId: number, frame: VideoFrame): void {
    let p = this.presenters.get(surfaceId);
    if (!p) {
      p = {
        queue: [],
        rafId: null,
        initialized: false,
        offsets: [],
        fastOffsetMs: 0,
        presentOffsetMs: NaN,
        lastPtsMs: null,
        steadyRun: 0,
        frameIntervalMs: SurfaceStore.DEFAULT_REFRESH_MS,
        smoothing: false,
      };
      this.presenters.set(surfaceId, p);
    }

    this.trackArrival(p, frame);

    if (!p.initialized) {
      p.initialized = true;
      this.presentFrame(surfaceId, frame);
      return;
    }

    p.queue.push(frame);

    // Hidden tabs never fire rAF, but decode output keeps arriving (the
    // stream stays subscribed and ACKed).  Present immediately instead of
    // queueing so every frame is closed promptly and the backing canvas
    // holds the latest frame when the tab is refocused.
    if (
      typeof document !== "undefined" &&
      document.visibilityState === "hidden"
    ) {
      if (p.rafId !== null) {
        cancelAnimationFrame(p.rafId);
        p.rafId = null;
      }
      this.flushPresenter(surfaceId);
      return;
    }

    // Bound the queue even while visible: a throttled rAF (occluded
    // window, busy main thread) must not let unclosed frames — each
    // pinning a decoded buffer in the codec's frame pool — pile up.
    // Trimming from the front is also the right call when scheduling: the
    // frames at the front are the most overdue.
    const cap = p.smoothing
      ? this.smoothedQueueCap(p)
      : SurfaceStore.PRESENT_QUEUE_MAX;
    const excess = p.queue.length - cap;
    if (excess > 0) {
      for (let i = 0; i < excess; i++) {
        try {
          p.queue[i].close();
        } catch {
          /* already closed */
        }
      }
      p.queue.splice(0, excess);
      this._diag.dropped += excess;
    }

    this.schedulePresent(surfaceId);
  }

  /** Fold one arrival into the presenter's clock model and decide whether
   *  this surface is streaming continuously enough to schedule off PTS. */
  private trackArrival(p: SurfacePresenter, frame: VideoFrame): void {
    const nowMs = performance.now();
    // VideoFrame.timestamp is µs; the wire carries u32 ms (see
    // handleSurfaceFrame), so this divides back to whole ms.
    const ptsMs = frame.timestamp / 1000;

    // No usable PTS — stay on newest-wins.  Scheduling against a NaN due
    // time would mean no frame ever compares as due and the surface would
    // freeze outright, which is far worse than the judder being fixed here.
    if (!Number.isFinite(ptsMs)) {
      p.offsets.length = 0;
      p.fastOffsetMs = 0;
      p.presentOffsetMs = NaN;
      p.steadyRun = 0;
      p.smoothing = false;
      p.lastPtsMs = null;
      return;
    }

    // Reset on a break in *capture* time, never on a break in arrival time.
    //
    // Both look like "a gap" locally, but they mean opposite things and
    // want opposite handling.  A source that went idle stops advancing PTS:
    // the next frame answers someone's input and must paint immediately,
    // not wait behind a margin fitted to the stream that ended.  A stalled
    // transport keeps producing frames the whole time — they just arrive
    // late, in a burst, with their PTS spacing intact.
    //
    // Judging by arrival could not tell those apart, so any stall longer
    // than the threshold disengaged scheduling.  On a reliable ordered
    // channel that is every lost packet, and recovery costs at least one
    // RTT — so on a high-latency link the scheduler switched itself off
    // permanently.  PTS spacing survives head-of-line blocking, which makes
    // this correct at any RTT without needing to know the RTT.
    //
    // A backwards or far-future PTS also covers the server's monotonic ms
    // counter wrapping (u32, ~49 days) and the stream being torn down and
    // restarted; in both the old baseline is meaningless.
    const ptsBroke =
      p.lastPtsMs !== null &&
      (ptsMs < p.lastPtsMs || ptsMs - p.lastPtsMs > SurfaceStore.STREAM_GAP_MS);

    if (ptsBroke) {
      p.offsets.length = 0;
      p.fastOffsetMs = 0;
      p.presentOffsetMs = NaN;
      p.steadyRun = 0;
      p.smoothing = false;
    }

    if (p.lastPtsMs !== null) {
      const ptsDelta = ptsMs - p.lastPtsMs;
      // Guard against the duplicate PTS a stalled encoder can emit, which
      // would drag the interval to zero and blow the derived queue cap up.
      if (ptsDelta > 0 && ptsDelta <= SurfaceStore.STREAM_GAP_MS) {
        p.frameIntervalMs += (ptsDelta - p.frameIntervalMs) * 0.1;
      }
    }

    p.offsets.push(nowMs - ptsMs);
    this.updateSchedule(p);

    p.lastPtsMs = ptsMs;
    p.steadyRun++;
    if (p.steadyRun >= SurfaceStore.SMOOTHING_ENGAGE_FRAMES) p.smoothing = true;
  }

  /** Trim the offset window to ~{@link OFFSET_WINDOW_MS} of stream, then
   *  slew the presentation offset toward the window's
   *  {@link PRESENT_QUANTILE}, capped at {@link PRESENT_DELAY_MAX_MS} of
   *  added latency over {@link FAST_QUANTILE}. */
  private updateSchedule(p: SurfacePresenter): void {
    const interval = Math.max(
      SurfaceStore.MIN_FRAME_INTERVAL_MS,
      p.frameIntervalMs,
    );
    const window = Math.min(
      SurfaceStore.OFFSET_WINDOW_MAX,
      Math.max(
        SurfaceStore.OFFSET_WINDOW_MIN,
        Math.round(SurfaceStore.OFFSET_WINDOW_MS / interval),
      ),
    );
    // One element off the front per frame, on an array of at most a few
    // hundred numbers — a memmove of a couple of KB, well below the cost
    // of decoding the frame it accompanies.
    if (p.offsets.length > window) {
      p.offsets.splice(0, p.offsets.length - window);
    }

    p.fastOffsetMs = quantile(p.offsets, SurfaceStore.FAST_QUANTILE);
    const target = Math.min(
      quantile(p.offsets, SurfaceStore.PRESENT_QUANTILE),
      p.fastOffsetMs + SurfaceStore.PRESENT_DELAY_MAX_MS,
    );

    if (!Number.isFinite(p.presentOffsetMs)) {
      p.presentOffsetMs = target;
      return;
    }
    if (target > p.presentOffsetMs) {
      p.presentOffsetMs = Math.min(
        target,
        p.presentOffsetMs + SurfaceStore.MARGIN_GROW_MS,
      );
    } else {
      const gap = p.presentOffsetMs - target;
      const step = Math.max(
        SurfaceStore.MARGIN_SHRINK_MS,
        gap * SurfaceStore.MARGIN_SHRINK_FRAC,
      );
      p.presentOffsetMs = Math.max(target, p.presentOffsetMs - step);
    }
  }

  /** Playout margin: how far behind the fastest observed path frames are
   *  held so a late one still lands on its intended refresh. */
  private playoutDelayMs(p: SurfacePresenter): number {
    if (!Number.isFinite(p.presentOffsetMs)) return 0;
    return Math.max(0, p.presentOffsetMs - p.fastOffsetMs);
  }

  /** How many frames the presenter may hold while scheduling.
   *
   *  A margin of `d` ms over a stream running at one frame every `i` ms
   *  has `d / i` frames legitimately in hand at any moment.  A fixed cap
   *  would fight the margin exactly where it is needed most: at 240 Hz a
   *  50 ms margin spans 12 frames, so a cap of 4 would trim eight
   *  not-yet-due frames per interval — dropping most of the stream in the
   *  name of bounding it.
   *
   *  The interval is floored rather than the depth ceilinged, so the outer
   *  bound is unreachable for any real frame rate and no stream is made to
   *  drop frames merely for being fast. */
  private smoothedQueueCap(p: SurfacePresenter): number {
    const interval = Math.max(
      SurfaceStore.MIN_FRAME_INTERVAL_MS,
      p.frameIntervalMs,
    );
    const span = Math.ceil(this.playoutDelayMs(p) / interval);
    return Math.min(
      Math.max(span + 2, SurfaceStore.PRESENT_QUEUE_MAX),
      SurfaceStore.PRESENT_QUEUE_MAX_SMOOTHED,
    );
  }

  private schedulePresent(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.rafId !== null) return;
    p.rafId = requestAnimationFrame(() => {
      p.rafId = null;
      this.noteRafInterval();
      this.tickPresent(surfaceId);
    });
  }

  /** Track the display's refresh period from rAF deltas.  Accepts anything
   *  from {@link RAF_DELTA_MIN_MS} to {@link RAF_DELTA_MAX_MS} — 1000 Hz
   *  down to 10 Hz — and ignores the rest as a stalled or backgrounded
   *  tick rather than a refresh rate. */
  private noteRafInterval(): void {
    const now = performance.now();
    if (this.lastRafMs !== null) {
      const dt = now - this.lastRafMs;
      // The band is wide on purpose, at both ends.
      //
      // Low: the server accepts a reported display rate up to
      // MAX_DISPLAY_FPS (480) and paces surfaces at it, so anything above
      // that is already beyond what the pipeline produces — but rejecting
      // fast deltas is the expensive mistake.  A 4 ms floor (250 Hz) threw
      // away every sample on a 360/480 Hz panel and left this pinned at the
      // 60 Hz default, which then puts half a *60 Hz* refresh of lookahead
      // on the due-time comparison — several refreshes early at that rate.
      //
      // High: a 10 Hz tick is a real cadence on a loaded machine or an
      // occluded window, and the rounding window should match whatever the
      // page is actually painting at.  The cost of admitting it is that a
      // transient stall drags the estimate up and presents slightly early
      // until it recovers — one 100 ms sample moves a 60 Hz estimate to
      // ~25 ms, about 4 ms of extra lookahead, gone within ten frames at
      // the 0.1 EWMA weight.  Cheaper than mistaking a slow display for a
      // fast one.
      if (
        dt >= SurfaceStore.RAF_DELTA_MIN_MS &&
        dt <= SurfaceStore.RAF_DELTA_MAX_MS
      ) {
        this.refreshMs += (dt - this.refreshMs) * 0.1;
      }
    }
    this.lastRafMs = now;
  }

  /** vsync tick.
   *
   *  Newest-wins until the surface proves it is streaming: that keeps
   *  time-to-pixel minimal for the interactive case, where a repaint is a
   *  response to input and any hold is felt as lag.
   *
   *  Once streaming, each frame is drawn on the refresh its capture-time
   *  PTS maps to.  Frames not yet due stay queued — that is what makes a
   *  30 fps source hold each frame for exactly two refreshes on a 60 Hz
   *  display instead of racing through the queue and then starving. */
  private tickPresent(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.queue.length === 0) return;

    if (!p.smoothing || !Number.isFinite(p.presentOffsetMs)) {
      this.presentIndex(surfaceId, p, p.queue.length - 1);
      return;
    }

    // rAF fires just before the next composite, so what is drawn now lands
    // one refresh from here.  Rounding by half a refresh picks the nearest
    // vsync rather than always the later one.
    const deadline = performance.now() + this.refreshMs / 2;
    const due = p.presentOffsetMs;

    let idx = -1;
    for (let i = 0; i < p.queue.length; i++) {
      if (p.queue[i].timestamp / 1000 + due <= deadline) idx = i;
      else break;
    }

    if (idx < 0) {
      // Nothing due yet — hold the last drawn frame for another refresh and
      // keep the loop alive, or the queue would sit here until the next
      // arrival happened to re-arm it.
      this.schedulePresent(surfaceId);
      return;
    }

    this.presentIndex(surfaceId, p, idx);
    if (p.queue.length > 0) this.schedulePresent(surfaceId);
  }

  /** Present `queue[idx]`, closing everything older, and keep the rest. */
  private presentIndex(
    surfaceId: number,
    p: SurfacePresenter,
    idx: number,
  ): void {
    for (let i = 0; i < idx; i++) {
      try {
        p.queue[i].close();
      } catch {
        /* already closed */
      }
    }
    if (idx > 0) this._diag.dropped += idx;
    const chosen = p.queue[idx];
    p.queue.splice(0, idx + 1);
    this.presentFrame(surfaceId, chosen);
  }

  /** Drain everything now, newest wins — for paths where rAF will not run
   *  again soon (hidden tab) or the queue must not outlive the surface. */
  private flushPresenter(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.queue.length === 0) return;
    this.presentIndex(surfaceId, p, p.queue.length - 1);
  }

  /** Draw a frame to the backing canvas and notify listeners.  Closes the
   *  frame on the way out. */
  private presentFrame(surfaceId: number, frame: VideoFrame): void {
    // Counted here rather than at the call sites: this is the one place a
    // frame actually reaches the canvas, so `presented` stays comparable
    // against `output` no matter which path drew it.  A healthy stream has
    // presented ≈ output; a gap between them is the judder this scheduler
    // exists to remove.
    this._diag.presented++;
    try {
      const ce = this.canvases.get(surfaceId);
      if (ce) {
        if (
          ce.canvas.width !== frame.displayWidth ||
          ce.canvas.height !== frame.displayHeight
        ) {
          ce.canvas.width = frame.displayWidth;
          ce.canvas.height = frame.displayHeight;
        }
        ce.ctx.drawImage(frame, 0, 0);
      }
    } finally {
      try {
        frame.close();
      } catch {
        /* already closed */
      }
    }
    for (const listener of this.frameListeners) {
      try {
        listener(surfaceId);
      } catch {
        // Prevent a single broken listener from blocking others.
      }
    }
  }

  private discardPresenter(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p) return;
    if (p.rafId !== null) cancelAnimationFrame(p.rafId);
    for (const f of p.queue) {
      try {
        f.close();
      } catch {
        /* already closed */
      }
    }
    this.presenters.delete(surfaceId);
  }

  private discardAllPresenters(): void {
    for (const sid of Array.from(this.presenters.keys())) {
      this.discardPresenter(sid);
    }
  }

  /** Present the newest queued frame (closing older ones) for every
   *  surface, cancelling pending rAFs.  Called when the tab goes hidden,
   *  where the rAFs would otherwise never fire.
   *
   *  Uses {@link flushPresenter}, not {@link tickPresent}: a scheduling
   *  tick with nothing yet due re-arms rAF, and while hidden that callback
   *  never runs — the queue would sit there holding decoder buffers until
   *  the tab came back. */
  private flushAllPresenters(): void {
    for (const [sid, p] of this.presenters) {
      if (p.rafId !== null) {
        cancelAnimationFrame(p.rafId);
        p.rafId = null;
      }
      // The stream is about to go unobserved; the clock model fitted to it
      // will be stale on return.  Reset so the first visible frame paints
      // immediately instead of waiting out a margin from before the gap.
      p.steadyRun = 0;
      p.smoothing = false;
      p.offsets.length = 0;
      p.fastOffsetMs = 0;
      p.presentOffsetMs = NaN;
      this.flushPresenter(sid);
    }
  }

  /**
   * Create an off-DOM canvas for *surfaceId* if one does not already exist.
   * Existing canvases are never resized here — resizing clears content and
   * must only happen inside the decoder output callback where a new frame is
   * immediately drawn afterwards.
   */
  private ensureCanvas(surfaceId: number, width: number, height: number): void {
    if (typeof document === "undefined") return;
    const w = width || 640;
    const h = height || 480;
    if (this.canvases.has(surfaceId)) return;
    try {
      const canvas = document.createElement("canvas");
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      this.canvases.set(surfaceId, { canvas, ctx });
    } catch {
      // Fallback for environments where canvas creation fails.
    }
  }

  private webCodecsUnavailableWarned = false;

  private initDecoder(
    surfaceId: number,
    codec: SurfaceCodec,
    width: number,
    height: number,
  ): void {
    if (!this.canDecodeVideo) {
      if (!this.webCodecsUnavailableWarned) {
        this.webCodecsUnavailableWarned = true;
        console.error(
          `[blit] Cannot decode surface video: ${this.videoUnavailableReason}.\n` +
            (typeof window !== "undefined" && !window.isSecureContext
              ? `Connect via HTTPS or localhost to enable surface streaming.`
              : `See https://developer.mozilla.org/en-US/docs/Web/API/WebCodecs_API#browser_compatibility`),
        );
        this.emitChange();
      }
      return;
    }
    const decoder = new VideoDecoder({
      output: (frame) => {
        this._diag.output++;

        // Per-surface output sample for debug panel rate computation.
        let outputs = this._surfaceOutputSamples.get(surfaceId);
        if (!outputs) {
          outputs = [];
          this._surfaceOutputSamples.set(surfaceId, outputs);
        }
        outputs.push(performance.now());
        if (outputs.length > SurfaceStore.OUTPUT_SAMPLE_MAX)
          outputs.splice(0, outputs.length - SurfaceStore.OUTPUT_SAMPLE_MAX);

        // Queue + paced presentation absorbs network/decoder jitter and
        // prevents 30 fps content from juddering on a 120 Hz display.
        // The first frame paints synchronously inside enqueueFrame to
        // minimise time-to-first-pixel.
        this.enqueueFrame(surfaceId, frame);
      },
      error: (e: DOMException) => {
        console.warn(
          "[blit] surface decoder error:",
          surfaceId,
          `${width}x${height}`,
          e.name,
          e.message,
          e.code,
          "state:",
          decoder.state,
        );
        // Only clean up if this decoder is still the active one —
        // handleSurfaceFrame may have already replaced it with a fresh
        // instance by the time this async callback fires.
        const entry = this.decoders.get(surfaceId);
        if (entry?.decoder === decoder) {
          safeClose(entry.decoder);
          this.decoders.delete(surfaceId);
        }
        // Ask the server for a keyframe so the next decoder gets a
        // clean reference point.
        this._keyframeSender?.(surfaceId);
      },
    });
    // Defer configure() until the first keyframe provides the codec
    // description (AVCC for H.264).  Configuring without a description
    // then reconfiguring with one causes VideoToolbox on macOS to drop
    // the first decoded frame.
    // AV1 has no description — configure it eagerly using the server-
    // provided WebCodecs codec string.
    if (codec === "av1") {
      const cs = this.codecStrings.get(surfaceId);
      if (cs) {
        try {
          decoder.configure({
            codec: cs,
            optimizeForLatency: true,
            colorSpace: FULL_RANGE_BT601,
          });
        } catch (e) {
          console.warn(
            "[blit] surface decoder configure failed:",
            surfaceId,
            codec,
            cs,
            e,
          );
          decoder.close();
          return;
        }
      }
    }
    this.decoders.set(surfaceId, {
      decoder,
      codec,
      pendingKeyframe: true,
      keyframeRequested: false,
      lastCodecString: null,
      lastDescription: null,
      lastConfiguredWidth: 0,
      lastConfiguredHeight: 0,
    });
  }

  private emitChange(): void {
    for (const listener of this.eventListeners) {
      try {
        listener(this.surfaces);
      } catch {
        // Prevent a single broken listener from blocking others.
      }
    }
  }
}
