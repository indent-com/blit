import type { BlitSurface, ConnectionId } from "./types";
import {
  SURFACE_FRAME_FLAG_KEYFRAME,
  SURFACE_FRAME_CODEC_MASK,
  SURFACE_FRAME_CODEC_AV1,
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_H264_444,
  CODEC_SUPPORT_AV1_444,
} from "./types";
// Shared with the codec probe rather than duplicated: the probe answers
// for what this browser accepts at a given level, and a decoder configured
// here at a different one would be asking a question nobody answered.
import { av1LevelString } from "./videoCodec";

/**
 * Every Blit encoder produces limited-range BT.601 (sRGB
 * primaries/transfer). Most streams also say so in-band (H.264 VUI, AV1
 * color_config); this hint covers encoders that cannot write it and keeps
 * decoder resets identical. Studio swing is intentional: Firefox can lose a
 * full-range flag before decoded YUV becomes RGB, progressively crushing dark
 * UI colors in a recursively captured surface.
 */
const LIMITED_RANGE_BT601: VideoColorSpaceInit = {
  primaries: "bt709",
  transfer: "iec61966-2-1",
  matrix: "smpte170m",
  fullRange: false,
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

/** A position in the composited frame's pixel space. */
export interface RemoteSurfacePointer {
  x: number;
  y: number;
}

/**
 * What another viewer is currently doing to a surface.
 *
 * Both kinds at once, because a touchscreen laptop drives a mouse and a
 * touchscreen together and each is its own mark set — a retire of one must not
 * erase the other. `pointer` holds at most one point.
 */
export interface RemoteSurfaceInput {
  pointer: readonly RemoteSurfacePointer[];
  touch: readonly RemoteSurfacePointer[];
}

export type RemoteInputKind = "pointer" | "touch";

/** Effective text-input state for one Wayland toplevel. */
export interface SurfaceTextInputState {
  enabled: boolean;
  /** `zwp_text_input_v3.content_hint` bitmask. */
  hint: number;
  /** Numeric `zwp_text_input_v3.content_purpose`. */
  purpose: number;
}

/** One state delivery. `requested` is true only for a fresh committed enable. */
export interface SurfaceTextInputEvent extends SurfaceTextInputState {
  requested: boolean;
}

const NO_REMOTE_INPUT: RemoteSurfaceInput = { pointer: [], touch: [] };

function samePoints(
  a: readonly RemoteSurfacePointer[],
  b: readonly RemoteSurfacePointer[],
): boolean {
  return (
    a.length === b.length &&
    a.every((point, i) => point.x === b[i]!.x && point.y === b[i]!.y)
  );
}

/** True when two mark sets would draw identically. */
function sameRemoteInput(
  a: RemoteSurfaceInput | null,
  b: RemoteSurfaceInput | null,
): boolean {
  if (!a || !b) return a === b;
  return samePoints(a.pointer, b.pointer) && samePoints(a.touch, b.touch);
}

/** Cursor artwork announced by the Wayland client for a surface. */
export type SurfaceCursorImage =
  | { kind: "named"; name: string }
  | { kind: "hidden" }
  | {
      kind: "custom";
      url: string;
      hotspotX: number;
      hotspotY: number;
      width: number;
      height: number;
    };

/** Timestamped record of an incoming surface video frame. */
export interface SurfaceFrameSample {
  /** `performance.now()` when the frame arrived. */
  t: number;
  /** Server CLOCK_MONOTONIC capture timestamp (wrapping u32 ms). */
  sourceT: number;
  /** Microseconds within `sourceT`, when negotiated with the server. */
  sourceSubUs?: number;
  /** Exact integer microsecond PTS submitted to WebCodecs. */
  ptsUs?: number;
  /** Encoded frame payload size in bytes. */
  bytes: number;
  /** Whether this was a keyframe. */
  key: boolean;
  /** `performance.now()` when WebCodecs produced the decoded frame. */
  decodeT?: number;
  /** `performance.now()` after synchronous visible-canvas blits completed. */
  presentT?: number;
  /** Estimated capture-to-receive time after midpoint clock calibration. */
  sourceToRecvMs?: number;
  /** Receive-to-decoder-output time. */
  decodeMs?: number;
  /** Decoder-output-to-visible-canvas time, including playout delay. */
  presentMs?: number;
  /** Estimated capture-to-visible-canvas submission time. */
  e2eMs?: number;
}

export interface ServerClockSample {
  serverMs: number;
  clientMidMs: number;
  rttMs: number;
}

/** Signed difference between two wrapping u32 millisecond timestamps. */
export function wrappingTimestampDelta(a: number, b: number): number {
  return ((a - b + 0x8000_0000) >>> 0) - 0x8000_0000;
}

/** Signed source-time delta in ms, including the wire's fractional part. */
export function sourceTimestampDelta(
  a: Pick<SurfaceFrameSample, "sourceT" | "sourceSubUs">,
  b: Pick<SurfaceFrameSample, "sourceT" | "sourceSubUs">,
): number {
  return (
    wrappingTimestampDelta(a.sourceT, b.sourceT) +
    ((a.sourceSubUs ?? 0) - (b.sourceSubUs ?? 0)) / 1000
  );
}

/** Map a server timestamp to the browser performance timeline. */
export function estimateSourceToReceiveMs(
  sourceMs: number,
  receiveMs: number,
  sync: ServerClockSample,
): number {
  const sourceClientMs =
    sync.clientMidMs + wrappingTimestampDelta(sourceMs, sync.serverMs);
  return receiveMs - sourceClientMs;
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
   *  {@link SurfaceStore.PRESENT_QUEUE_MAX}, or by the cadence-derived
   *  {@link SurfaceStore.smoothedQueueCap} once scheduling is engaged — each
   *  entry pins a decoded buffer in the codec's frame pool, so an undrained
   *  queue (hidden tab, throttled rAF) would otherwise grow until the
   *  renderer OOMs. */
  queue: VideoFrame[];
  /** Diagnostic slot tokens parallel to {@link queue}; -1 when sampling is
   * disabled or the frame predates the active sampling window. */
  sampleTokens: number[];
  /** Pending `requestAnimationFrame` handle, or null. */
  rafId: number | null;
  /** Stable callback reused for every tick. Allocating a closure for every
   * refresh is visible GC pressure at 240–480 Hz. */
  rafCallback: FrameRequestCallback;
  /** True after the first frame has been presented.  The first frame
   *  paints synchronously to minimise time-to-first-pixel. */
  initialized: boolean;
  /** Recent transport `receive - pts` samples (ms), covering roughly
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
  offsets: RollingQuantile;
  /** Recent receive-to-decoder-output durations. Kept separate from
   *  {@link offsets} so hardware-decoder batching is not mistaken for
   *  network jitter and converted into permanent playout latency. */
  decodeDelays: RollingQuantile;
  /** Fast transport offset plus fast decode duration: the earliest useful
   *  presentation point the pipeline has recently demonstrated. Only used as
   *  the reference for how much latency presentation is adding. */
  fastOffsetMs: number;
  /** The offset presentation actually runs at: a frame is drawn at
   *  `pts + presentOffsetMs`. Grows immediately when recurring jitter needs
   *  more cover and slews down when the path settles, so shedding latency
   *  does not make several queued frames become due at once. */
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

/** Fixed-capacity insertion-order numeric ring backed by a TypedArray. */
export class NumberRing {
  private readonly values: Float64Array;
  private start = 0;
  length = 0;

  constructor(readonly capacity: number) {
    if (!Number.isInteger(capacity) || capacity < 1)
      throw new RangeError("capacity must be a positive integer");
    this.values = new Float64Array(capacity);
  }

  push(value: number): void {
    if (this.length < this.capacity) {
      this.values[(this.start + this.length) % this.capacity] = value;
      this.length++;
      return;
    }
    this.values[this.start] = value;
    this.start = (this.start + 1) % this.capacity;
  }

  time(index: number): number {
    return index < 0 || index >= this.length
      ? NaN
      : this.values[(this.start + index) % this.capacity];
  }

  toArray(): number[] {
    const result = new Array<number>(this.length);
    for (let i = 0; i < this.length; i++) {
      result[i] = this.values[(this.start + i) % this.capacity];
    }
    return result;
  }
}

/** Struct-of-TypedArrays storage for the surface debug timeline. A compact
 * numeric token identifies a slot while it moves through decode/presentation,
 * so the 240–480 Hz receive path creates no sample objects. */
export class SurfaceFrameHistory {
  private readonly times: Float64Array;
  private readonly sourceTimes: Uint32Array;
  private readonly sourceSubTimes: Uint16Array;
  private readonly pts: Float64Array;
  private readonly sizes: Uint32Array;
  private readonly keys: Uint8Array;
  private readonly sourceToReceive: Float64Array;
  private readonly decodeTimes: Float64Array;
  private readonly decodeDurations: Float64Array;
  private readonly presentTimes: Float64Array;
  private readonly presentDurations: Float64Array;
  private readonly e2eDurations: Float64Array;
  private readonly generations: Uint32Array;
  private start = 0;
  length = 0;

  constructor(readonly capacity: number) {
    this.times = new Float64Array(capacity);
    this.sourceTimes = new Uint32Array(capacity);
    this.sourceSubTimes = new Uint16Array(capacity);
    this.pts = new Float64Array(capacity);
    this.sizes = new Uint32Array(capacity);
    this.keys = new Uint8Array(capacity);
    this.sourceToReceive = new Float64Array(capacity);
    this.decodeTimes = new Float64Array(capacity);
    this.decodeDurations = new Float64Array(capacity);
    this.presentTimes = new Float64Array(capacity);
    this.presentDurations = new Float64Array(capacity);
    this.e2eDurations = new Float64Array(capacity);
    this.generations = new Uint32Array(capacity);
  }

  push(
    time: number,
    sourceTime: number,
    sourceSubTime: number,
    pts: number,
    bytes: number,
    key: boolean,
    sourceToReceive: number,
  ): number {
    let index: number;
    if (this.length < this.capacity) {
      index = (this.start + this.length) % this.capacity;
      this.length++;
    } else {
      index = this.start;
      this.start = (this.start + 1) % this.capacity;
    }
    let generation = (this.generations[index] + 1) >>> 0;
    if (generation === 0) generation = 1;
    this.generations[index] = generation;
    this.times[index] = time;
    this.sourceTimes[index] = sourceTime;
    this.sourceSubTimes[index] = sourceSubTime;
    this.pts[index] = pts;
    this.sizes[index] = bytes;
    this.keys[index] = key ? 1 : 0;
    this.sourceToReceive[index] = sourceToReceive;
    this.decodeTimes[index] = NaN;
    this.decodeDurations[index] = NaN;
    this.presentTimes[index] = NaN;
    this.presentDurations[index] = NaN;
    this.e2eDurations[index] = NaN;
    return generation * this.capacity + index;
  }

  private resolve(token: number): number {
    if (token < 0) return -1;
    const index = token % this.capacity;
    const generation = Math.floor(token / this.capacity);
    return this.generations[index] === generation ? index : -1;
  }

  private physical(logical: number): number {
    return logical < 0 || logical >= this.length
      ? -1
      : (this.start + logical) % this.capacity;
  }

  time(logical: number): number {
    const index = this.physical(logical);
    return index < 0 ? NaN : this.times[index];
  }

  bytes(logical: number): number {
    const index = this.physical(logical);
    return index < 0 ? 0 : this.sizes[index];
  }

  isKey(logical: number): boolean {
    const index = this.physical(logical);
    return index >= 0 && this.keys[index] !== 0;
  }

  sourceToRecvMs(logical: number): number {
    const index = this.physical(logical);
    return index < 0 ? NaN : this.sourceToReceive[index];
  }

  decodeMs(logical: number): number {
    const index = this.physical(logical);
    return index < 0 ? NaN : this.decodeDurations[index];
  }

  presentMs(logical: number): number {
    const index = this.physical(logical);
    return index < 0 ? NaN : this.presentDurations[index];
  }

  e2eMs(logical: number): number {
    const index = this.physical(logical);
    return index < 0 ? NaN : this.e2eDurations[index];
  }

  sourceDelta(later: number, earlier: number): number {
    const a = this.physical(later);
    const b = this.physical(earlier);
    if (a < 0 || b < 0) return NaN;
    const ms = wrappingTimestampDelta(this.sourceTimes[a], this.sourceTimes[b]);
    return ms + (this.sourceSubTimes[a] - this.sourceSubTimes[b]) / 1000;
  }

  markDecoded(token: number, time: number): boolean {
    const index = this.resolve(token);
    if (index < 0) return false;
    this.decodeTimes[index] = time;
    this.decodeDurations[index] = Math.max(0, time - this.times[index]);
    return true;
  }

  markPresented(token: number, time: number): void {
    const index = this.resolve(token);
    if (index < 0) return;
    this.presentTimes[index] = time;
    const decodeTime = this.decodeTimes[index];
    this.presentDurations[index] = Math.max(
      0,
      time - (Number.isFinite(decodeTime) ? decodeTime : time),
    );
    if (Number.isFinite(this.sourceToReceive[index])) {
      this.e2eDurations[index] =
        this.sourceToReceive[index] +
        (Number.isFinite(this.decodeDurations[index])
          ? this.decodeDurations[index]
          : 0) +
        this.presentDurations[index];
    }
  }

  toArray(): SurfaceFrameSample[] {
    const result = new Array<SurfaceFrameSample>(this.length);
    for (let logical = 0; logical < this.length; logical++) {
      const index = (this.start + logical) % this.capacity;
      const sample: SurfaceFrameSample = {
        t: this.times[index],
        sourceT: this.sourceTimes[index],
        sourceSubUs: this.sourceSubTimes[index],
        ptsUs: this.pts[index],
        bytes: this.sizes[index],
        key: this.keys[index] !== 0,
      };
      if (Number.isFinite(this.sourceToReceive[index]))
        sample.sourceToRecvMs = this.sourceToReceive[index];
      if (Number.isFinite(this.decodeTimes[index]))
        sample.decodeT = this.decodeTimes[index];
      if (Number.isFinite(this.decodeDurations[index]))
        sample.decodeMs = this.decodeDurations[index];
      if (Number.isFinite(this.presentTimes[index]))
        sample.presentT = this.presentTimes[index];
      if (Number.isFinite(this.presentDurations[index]))
        sample.presentMs = this.presentDurations[index];
      if (Number.isFinite(this.e2eDurations[index]))
        sample.e2eMs = this.e2eDurations[index];
      result[logical] = sample;
    }
    return result;
  }
}

/** Bounded decoder-correlation queue backed by TypedArrays. */
export class PendingFrameSamples {
  private readonly pts: Float64Array;
  private readonly tokens: Float64Array;
  length = 0;

  constructor(readonly capacity: number) {
    this.pts = new Float64Array(capacity);
    this.tokens = new Float64Array(capacity);
  }

  push(pts: number, token: number): void {
    if (this.length === this.capacity) {
      this.pts.copyWithin(0, 1);
      this.tokens.copyWithin(0, 1);
      this.length--;
    }
    this.pts[this.length] = pts;
    this.tokens[this.length] = token;
    this.length++;
  }

  takeByPts(pts: number): number {
    for (let i = 0; i < this.length; i++) {
      if (this.pts[i] !== pts) continue;
      return this.removeAt(i);
    }
    return -1;
  }

  removeToken(token: number): void {
    for (let i = 0; i < this.length; i++) {
      if (this.tokens[i] === token) {
        this.removeAt(i);
        return;
      }
    }
  }

  private removeAt(index: number): number {
    const token = this.tokens[index];
    this.pts.copyWithin(index, index + 1, this.length);
    this.tokens.copyWithin(index, index + 1, this.length);
    this.length--;
    return token;
  }
}

/** Sliding nearest-rank quantile with no per-sample allocations. The FIFO
 * and sorted views are typed arrays; insertion/removal are bounded memmoves
 * over at most 480 doubles, replacing an allocated full sort every frame. */
export class RollingQuantile {
  private readonly fifo: Float64Array;
  private readonly sorted: Float64Array;
  private head = 0;
  length = 0;

  constructor(readonly capacity: number) {
    if (!Number.isInteger(capacity) || capacity < 1)
      throw new RangeError("capacity must be a positive integer");
    this.fifo = new Float64Array(capacity);
    this.sorted = new Float64Array(capacity);
  }

  clear(): void {
    this.head = 0;
    this.length = 0;
  }

  private lowerBound(value: number): number {
    let lo = 0;
    let hi = this.length;
    while (lo < hi) {
      const mid = (lo + hi) >>> 1;
      if (this.sorted[mid] < value) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }

  private removeOldest(): void {
    const value = this.fifo[this.head];
    this.head = (this.head + 1) % this.capacity;
    const index = this.lowerBound(value);
    this.sorted.copyWithin(index, index + 1, this.length);
    this.length--;
  }

  push(value: number, window: number = this.capacity): void {
    const limit = Math.max(1, Math.min(this.capacity, window | 0));
    while (this.length >= limit) this.removeOldest();

    const insertAt = this.lowerBound(value);
    this.sorted.copyWithin(insertAt + 1, insertAt, this.length);
    this.sorted[insertAt] = value;
    this.fifo[(this.head + this.length) % this.capacity] = value;
    this.length++;
  }

  quantile(q: number): number {
    if (this.length === 0) return 0;
    const index = Math.min(
      this.length - 1,
      Math.max(0, Math.ceil(q * this.length) - 1),
    );
    return this.sorted[index];
  }
}

/** Remove an array prefix without allocating the discarded array that
 * `splice()` returns. This is used on the per-refresh VideoFrame queue. */
function removePrefixInPlace<T>(values: T[], count: number): void {
  if (count <= 0) return;
  if (count >= values.length) {
    values.length = 0;
    return;
  }
  values.copyWithin(0, count);
  values.length -= count;
}

/** Insert into an ascending numeric array without creating a temporary
 *  array. */
function insertSorted(sorted: number[], value: number): void {
  let lo = 0;
  let hi = sorted.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (sorted[mid] <= value) lo = mid + 1;
    else hi = mid;
  }
  sorted.push(value);
  sorted.copyWithin(lo + 1, lo, sorted.length - 1);
  sorted[lo] = value;
}

/** Remove one exact value from an ascending numeric array in place. */
function removeSorted(sorted: number[], value: number): void {
  let lo = 0;
  let hi = sorted.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (sorted[mid] < value) lo = mid + 1;
    else hi = mid;
  }
  if (lo >= sorted.length || sorted[lo] !== value) return;
  sorted.copyWithin(lo, lo + 1);
  sorted.length--;
}

/** Discard a prefix without `splice()` allocating the removed elements. */
function dropPrefixInPlace<T>(items: T[], count: number): void {
  if (count <= 0) return;
  if (count >= items.length) {
    items.length = 0;
    return;
  }
  items.copyWithin(0, count);
  items.length -= count;
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
  /**
   * Sizes from an S2C_SURFACE_RESIZED that arrived before the matching
   * S2C_SURFACE_CREATED, replayed by `handleSurfaceCreated`.  The server
   * builds a joining client's replay under the session lock but broadcasts
   * concurrently, so a live resize can overtake the create.  Dropping it
   * would be permanent: the compositor only emits a resize when the size
   * changes and nothing re-announces the current one, so the surface would
   * keep the stale dimensions — and hence a wrong pointer scale — until
   * the next genuine resize, which for an idle app may be never.
   */
  private _pendingResizes = new Map<
    number,
    {
      width: number;
      height: number;
      logicalWidth: number;
      logicalHeight: number;
    }
  >();
  /**
   * Surface ids destroyed but not yet re-created.  A resize can also trail
   * its own destroy (the compositor queues native sizes during render and
   * flushes them after the toplevel is gone), and that straggler must not
   * be stashed — ids are recycled, so it would be replayed onto whatever
   * surface claims the id next.
   */
  private _destroyedSurfaceIds = new Set<number>();
  private connectionId: ConnectionId = "";
  private decoders = new Map<number, DecoderEntry>();
  private canvases = new Map<number, CanvasEntry>();
  private frameListeners = new Set<SurfaceFrameCallback>();
  private cursorShapes = new Map<number, string>();
  private cursorImages = new Map<number, SurfaceCursorImage>();
  private remoteInputs = new Map<number, RemoteSurfaceInput>();
  private textInputs = new Map<number, SurfaceTextInputState>();
  private encoderNames = new Map<number, string>();
  private codecStrings = new Map<number, string>();
  /** Most recent *AV1* codec string announced per surface.  The plain
   *  announcement can be an avc1 one from a preference walk the server is
   *  still working through, which says nothing about the AV1 frames in
   *  flight; this remembers the last string that did. */
  private av1CodecStrings = new Map<number, string>();
  private cursorListeners = new Set<
    (surfaceId: number, shape: string) => void
  >();
  private remoteInputListeners = new Set<
    (surfaceId: number, input: RemoteSurfaceInput | null) => void
  >();
  private activationListeners = new Set<(surfaceId: number) => void>();
  private textInputListeners = new Set<
    (surfaceId: number, state: SurfaceTextInputEvent) => void
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
  private diagnosticsEnabled = true;
  /** Whether continuous streams may trade latency for smoother cadence.
   *  Embedders retain the historical smoothing default; the interactive UI
   *  exposes this as a device-local preference and defaults it off. */
  private presentationSmoothingEnabled = true;

  // Per-surface diagnostics exposed to the debug panel.
  private _surfaceFrameSamples = new Map<number, SurfaceFrameHistory>();
  private readonly _emptyFrameSamples = new SurfaceFrameHistory(1);
  /** Timestamps of decoded output frames (for computing output fps). */
  private _surfaceOutputSamples = new Map<number, NumberRing>();
  private readonly _emptyOutputSamples = new NumberRing(1);
  /** Cumulative per-surface drop/error counters. */
  private _surfaceDrops = new Map<number, number>();
  private _surfaceErrors = new Map<number, number>();
  /** Recent midpoint clock calibrations; the lowest-RTT sample has the
   *  smallest one-way/asymmetry error and is used for source-age estimates. */
  private _serverClockSamples: ServerClockSample[] = [];
  private _serverClock: ServerClockSample | null = null;
  /** Received samples waiting for the corresponding WebCodecs output. */
  private _pendingFrameSamples = new Map<number, PendingFrameSamples>();
  /** Transport receive timestamps waiting for the corresponding decoder
   *  output. This is always populated; unlike diagnostic samples it drives
   *  presentation timing. */
  private _pendingFrameReceiveTimes = new Map<number, PendingFrameSamples>();

  private static readonly FRAME_SAMPLE_MAX = 500;
  private static readonly OUTPUT_SAMPLE_MAX = 500;
  private static readonly CLOCK_SAMPLE_MAX = 12;
  /** Max decoded frames a presenter may hold between rAF ticks while
   *  presenting newest-wins (no scheduling, so depth is pure overflow
   *  slack). */
  private static readonly PRESENT_QUEUE_MAX = 2;
  /** Consecutive continuous arrivals before PTS scheduling engages.  Long
   *  enough that a couple of repaints from a click don't trip it, short
   *  enough that it is running well inside the first second of playback. */
  private static readonly SMOOTHING_ENGAGE_FRAMES = 8;
  /** An arrival or PTS gap longer than this ends the current stream
   *  episode: the surface went idle, so the next frame is a fresh
   *  interaction and must paint immediately rather than wait out a
   *  playout margin computed for the previous episode. */
  private static readonly STREAM_GAP_MS = 250;
  /** How much stream the offset distribution covers.  Expressed in time
   *  rather than frames so the horizon is the same at 24 and 240 fps —
   *  too short and the schedule chases noise, too long and it responds
   *  sluggishly to a link that genuinely changed (a Wi-Fi roam). */
  private static readonly OFFSET_WINDOW_MS = 1000;
  private static readonly OFFSET_WINDOW_MIN = 60;
  private static readonly OFFSET_WINDOW_MAX = 480;
  /** Low quantile taken as "the fastest this path goes".  Not the strict
   *  minimum: a burst frame is captured later but shipped immediately
   *  behind its predecessor, so its transit genuinely is shorter, and a
   *  minimum would take that one-off as a permanently faster link.  A
   *  quantile ignores it for the same reason the high end ignores a single
   *  late frame — no separate clamp or leak rule needed at either end. */
  private static readonly FAST_QUANTILE = 0.02;
  /** Highest receive samples ignored when choosing the playout point. A
   *  percentage tail is the wrong unit here: at 240 Hz even 2% discards five
   *  frames, enough to hide the leading edge of every Wi-Fi recovery burst.
   *  Ignoring exactly one sample rejects an isolated pause at every rate but
   *  retains any recurring jitter. */
  private static readonly PLAYOUT_OUTLIERS = 1;
  /** A sub-2 ms protocol ping identifies a same-host server even when the UI
   *  itself was opened through a named gateway route. */
  private static readonly LOCAL_RTT_MAX_MS = 2;
  /** Bound the interaction-latency cost of smoothing. The transport is
   *  expected to deliver inside this window; longer outages still collapse
   *  to newest-wins when the burst arrives rather than replaying stale video. */
  private static readonly MAX_PLAYOUT_DELAY_MS = 96;
  /** Fraction of one source interval removed from a shrinking playout margin
   *  per frame. This sheds the full margin in roughly 3.2 seconds at any FPS. */
  private static readonly PLAYOUT_SLEW_DOWN_PER_FRAME = 0.03;
  /** Fallback display refresh interval before any rAF delta is measured. */
  private static readonly DEFAULT_REFRESH_MS = 1000 / 60;
  /** Longest rAF delta that counts as a refresh period.  Faster positive
   *  cadences have no policy cutoff; longer gaps are stalls/backgrounding. */
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
   * connection layer; each ACK carries the current WebCodecs queue depth so
   * the server sees decoder pressure without interpreting JS scheduling as
   * congestion.
   */
  private _ackSender:
    | ((surfaceId: number, decoderQueueDepth: number) => void)
    | null = null;

  /**
   * Callback to request a keyframe from the server (re-subscribe).
   * Called when the decoder enters an error state and needs a clean
   * reference point to recover.
   */
  private _keyframeSender: ((surfaceId: number) => void) | null = null;

  /** Install the ACK sender callback (called once by BlitConnection). */
  setAckSender(
    fn: (surfaceId: number, decoderQueueDepth: number) => void,
  ): void {
    this._ackSender = fn;
  }

  /** Install the keyframe-request callback (called once by BlitConnection). */
  setKeyframeSender(fn: (surfaceId: number) => void): void {
    this._keyframeSender = fn;
  }

  /** Keyframe requests made while a decoder cannot produce output, per
   *  surface: when the last one went out and how many this episode has cost.
   *  Successful decoded output clears the episode. */
  private _unconfiguredRetry = new Map<number, { at: number; count: number }>();

  /** Spacing and budget for recovery requests.  Each is a SURFACE_SUBSCRIBE on
   *  the wire that forces another keyframe, so this must never become a
   *  per-frame ask; a handful, seconds apart, is enough for a stream whose
   *  configuration is one announcement away, and a stream that stays
   *  unconfigurable stops costing anything after that. */
  private static readonly UNCONFIGURED_RETRY_MS = 2000;
  private static readonly UNCONFIGURED_RETRY_MAX = 5;

  /**
   * Ask for a keyframe on behalf of a surface whose decoder is dropping
   * every frame because it cannot configure or decode the stream.
   *
   * Without this the drop is silent and terminal: the codec-string
   * announcement that would configure the decoder only arrives when the
   * server rebuilds the session, which a healthy encoder has no reason to
   * do, so the pane stays black for as long as frames keep flowing.  The
   * request re-subscribes, which does rebuild it.
   */
  private retryUnconfigured(surfaceId: number): void {
    const now = performance.now();
    const state = this._unconfiguredRetry.get(surfaceId);
    if (state) {
      if (
        state.count >= SurfaceStore.UNCONFIGURED_RETRY_MAX ||
        now - state.at < SurfaceStore.UNCONFIGURED_RETRY_MS
      ) {
        return;
      }
      state.at = now;
      state.count++;
    } else {
      this._unconfiguredRetry.set(surfaceId, { at: now, count: 1 });
    }
    this._keyframeSender?.(surfaceId);
  }

  /**
   * WebCodecs codec string to configure an AV1 decoder for `surfaceId`
   * with.  The announced string is authoritative when it describes an AV1
   * stream; otherwise the frames themselves are the better evidence —
   * they say AV1, so a string derived from them beats leaving the decoder
   * unconfigured and dropping them.
   */
  private av1CodecString(
    surfaceId: number,
    width: number,
    height: number,
  ): string {
    const announced = this.codecStrings.get(surfaceId);
    if (announced?.startsWith("av01")) return announced;
    const remembered = this.av1CodecStrings.get(surfaceId);
    if (remembered) return remembered;
    // Profile 0 at 8 bits: a 4:4:4 stream is profile 1, but the decoder
    // reads the real profile out of the sequence header, and this is the
    // last resort before a black pane.
    return `av01.0.${av1LevelString(width, height)}M.08`;
  }

  /** Configure `entry`'s AV1 decoder, reporting whether it took.  A failure
   *  leaves the decoder closed — it cannot decode and cannot be retried. */
  private configureAv1Decoder(
    entry: DecoderEntry,
    surfaceId: number,
    width: number,
    height: number,
  ): boolean {
    const cs = this.av1CodecString(surfaceId, width, height);
    try {
      entry.decoder.configure({
        codec: cs,
        optimizeForLatency: true,
        colorSpace: LIMITED_RANGE_BT601,
      });
      entry.lastCodecString = cs;
      entry.lastConfiguredWidth = width;
      entry.lastConfiguredHeight = height;
      return true;
    } catch (e) {
      console.warn(
        "[blit] surface decoder configure failed:",
        surfaceId,
        "av1",
        cs,
        e,
      );
      safeClose(entry.decoder);
      return false;
    }
  }

  /**
   * Callback to drop codec-support bits and renegotiate the encoder.
   * Called after a stream fails to decode repeatedly: a fresh keyframe of
   * the same stream will fail the same way, so re-requesting keyframes
   * forever just loops on a black pane.
   */
  private _codecDemoter: ((surfaceId: number, bits: number) => void) | null =
    null;

  /** Consecutive decode failures per surface since the last decoded frame,
   *  with the timestamp of the most recent one. */
  private _decodeFailStreak = new Map<number, { count: number; at: number }>();

  /** Decode-failure episodes tolerated before demoting codec support. */
  static readonly DECODE_FAILURES_BEFORE_DEMOTION = 3;

  /** How long a decode failure keeps counting towards the streak.
   *
   *  Demotion is for a stream this platform cannot decode at all, which
   *  fails on every keyframe recovery in a row — seconds apart at most.
   *  Without a window, one bad frame an hour still accumulates, so a page
   *  left open all day eventually demotes a codec that works. */
  static readonly DECODE_FAILURE_WINDOW_MS = 10_000;

  /** Install the codec-demotion callback (called once by BlitConnection). */
  setCodecDemoter(fn: (surfaceId: number, bits: number) => void): void {
    this._codecDemoter = fn;
  }

  /**
   * Record a decode failure; after
   * {@link DECODE_FAILURES_BEFORE_DEMOTION} in a row (each already a
   * keyframe-recovery attempt), stop asking for keyframes of a stream this
   * platform's decoder rejects and demote the codec-support bits that
   * selected it, so the server renegotiates to a different encoder.  The
   * 4:4:4 flavor goes first when the announced string says that is what we
   * are being sent; the base codec goes only if failures continue.
   */
  private noteDecodeFailure(surfaceId: number, codec: "h264" | "av1"): void {
    const now = performance.now();
    const prev = this._decodeFailStreak.get(surfaceId);
    const within =
      prev !== undefined &&
      now - prev.at <= SurfaceStore.DECODE_FAILURE_WINDOW_MS;
    const n = within ? prev.count + 1 : 1;
    if (n < SurfaceStore.DECODE_FAILURES_BEFORE_DEMOTION) {
      this._decodeFailStreak.set(surfaceId, { count: n, at: now });
      return;
    }
    this._decodeFailStreak.delete(surfaceId);
    const announced = this.codecStrings.get(surfaceId);
    const bits =
      codec === "av1"
        ? announced?.startsWith("av01.1")
          ? CODEC_SUPPORT_AV1_444
          : CODEC_SUPPORT_AV1 | CODEC_SUPPORT_AV1_444
        : announced?.startsWith("avc1.F4")
          ? CODEC_SUPPORT_H264_444
          : CODEC_SUPPORT_H264 | CODEC_SUPPORT_H264_444;
    this._codecDemoter?.(surfaceId, bits);
  }

  private sendAck(surfaceId: number): void {
    let decoderQueueDepth = 0;
    const entry = this.decoders.get(surfaceId);
    try {
      if (entry?.decoder.state === "configured") {
        decoderQueueDepth = entry.decoder.decodeQueueSize;
      }
    } catch {
      // The decoder can close between the state and queue-depth reads.
    }
    this._ackSender?.(surfaceId, decoderQueueDepth);
  }

  /** Send an ACK unconditionally — used by the connection layer's catch
   *  path when handleSurfaceFrame throws before it can ACK itself. */
  sendAckFallback(surfaceId: number): void {
    this.sendAck(surfaceId);
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
    /** Typed ring of recent incoming frame samples (for timeline graph). */
    frameSamples: SurfaceFrameHistory;
    /** Ring buffer of decoded-output timestamps (for fps computation). */
    outputSamples: NumberRing;
    /** Cumulative dropped frame count. */
    dropped: number;
    /** Cumulative decode error count. */
    errors: number;
    /** Current WebCodecs decode queue depth. */
    queueDepth: number;
    /** RTT of the midpoint clock sample used for latency estimation. */
    clockRttMs: number | null;
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
        frameSamples:
          this._surfaceFrameSamples.get(id) ?? this._emptyFrameSamples,
        outputSamples:
          this._surfaceOutputSamples.get(id) ?? this._emptyOutputSamples,
        dropped: this._surfaceDrops.get(id) ?? 0,
        errors: this._surfaceErrors.get(id) ?? 0,
        queueDepth,
        clockRttMs: this._serverClock?.rttMs ?? null,
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

  /** Per-frame latency histories are only useful while the debug pane is
   * visible. Keeping them off otherwise removes diagnostic allocation and
   * correlation work from the video hot path. */
  setDiagnosticsEnabled(enabled: boolean): void {
    if (enabled === this.diagnosticsEnabled) return;
    this.diagnosticsEnabled = enabled;
    if (enabled) return;
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._pendingFrameSamples.clear();
    for (const presenter of this.presenters.values()) {
      presenter.sampleTokens.fill(-1);
    }
  }

  /** Allow or bypass the decoded-frame playout buffer.
   *
   * Disabling it is an immediate latency operation: cancel any pending rAF,
   * discard the learned path margin, and paint the newest queued frame now.
   * Re-enabling starts with a fresh timing window so an old network stall
   * cannot become latency in the new smoothing episode. */
  setPresentationSmoothingEnabled(enabled: boolean): void {
    if (enabled === this.presentationSmoothingEnabled) return;
    this.presentationSmoothingEnabled = enabled;
    for (const [surfaceId, p] of this.presenters) {
      if (p.rafId !== null) {
        cancelAnimationFrame(p.rafId);
        p.rafId = null;
      }
      p.steadyRun = 0;
      p.smoothing = false;
      p.offsets.clear();
      p.decodeDelays.clear();
      p.fastOffsetMs = 0;
      p.presentOffsetMs = NaN;
      p.lastPtsMs = null;
      if (!enabled) this.flushPresenter(surfaceId);
    }
  }

  setConnectionId(id: ConnectionId): void {
    this.connectionId = id;
  }

  /** Add one server CLOCK_MONOTONIC ↔ performance.now() calibration.
   *  The midpoint estimate assumes a roughly symmetric path; retaining the
   *  lowest-RTT sample bounds queueing error and gives the debug pane an
   *  honest uncertainty indicator. */
  noteServerClock(
    serverMs: number,
    clientSendMs: number,
    clientReceiveMs: number,
  ): void {
    const rttMs = Math.max(0, clientReceiveMs - clientSendMs);
    if (!Number.isFinite(rttMs) || rttMs > 60_000) return;
    const sample: ServerClockSample = {
      serverMs: serverMs >>> 0,
      clientMidMs: clientSendMs + rttMs / 2,
      rttMs,
    };
    this._serverClockSamples.push(sample);
    if (this._serverClockSamples.length > SurfaceStore.CLOCK_SAMPLE_MAX) {
      this._serverClockSamples.splice(
        0,
        this._serverClockSamples.length - SurfaceStore.CLOCK_SAMPLE_MAX,
      );
    }
    this._serverClock = this._serverClockSamples.reduce((best, candidate) =>
      candidate.rttMs < best.rttMs ? candidate : best,
    );
  }

  clearServerClock(): void {
    this._serverClockSamples.length = 0;
    this._serverClock = null;
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
      // S2C_SURFACE_CREATED carries no scale; the S2C_SURFACE_RESIZED the
      // server sends right behind it does.  Unknown until then.
      logicalWidth: 0,
      logicalHeight: 0,
    });
    this._destroyedSurfaceIds.delete(surfaceId);
    // Apply a resize that overtook this create, so the surface starts at
    // the size the compositor actually last reported rather than the one
    // this (older) create carried.
    const pending = this._pendingResizes.get(surfaceId);
    if (pending) {
      this._pendingResizes.delete(surfaceId);
      this.handleSurfaceResized(
        surfaceId,
        pending.width,
        pending.height,
        pending.logicalWidth,
        pending.logicalHeight,
      );
    }
    // Don't create a canvas yet — canvases are per-subscription now,
    // keyed by sub_id, and we don't have one until a view subscribes.
    this.emitChange();
  }

  handleSurfaceDestroyed(surfaceId: number): void {
    this.surfaces.delete(surfaceId);
    this._pendingResizes.delete(surfaceId);
    this._destroyedSurfaceIds.add(surfaceId);
    this.clearTextInput(surfaceId);
    this.clearCursor(surfaceId);
    this.clearRemoteInput(surfaceId);
    this.encoderNames.delete(surfaceId);
    this.codecStrings.delete(surfaceId);
    this.av1CodecStrings.delete(surfaceId);
    this._decodeFailStreak.delete(surfaceId);
    this._unconfiguredRetry.delete(surfaceId);
    this._surfaceFrameSamples.delete(surfaceId);
    this._surfaceOutputSamples.delete(surfaceId);
    this._surfaceDrops.delete(surfaceId);
    this._surfaceErrors.delete(surfaceId);
    this._pendingFrameSamples.delete(surfaceId);
    this._pendingFrameReceiveTimes.delete(surfaceId);
    this.discardPresenter(surfaceId);
    const entry = this.decoders.get(surfaceId);
    if (entry) safeClose(entry.decoder);
    this.decoders.delete(surfaceId);
    this.canvases.delete(surfaceId);
    this.emitChange();
  }

  handleSurfaceFrame(
    surfaceId: number,
    timestamp: number,
    flags: number,
    width: number,
    height: number,
    data: Uint8Array,
    timestampSubUs: number = 0,
  ): void {
    this._diag.received++;
    const receiveT = performance.now();
    const isKey = (flags & SURFACE_FRAME_FLAG_KEYFRAME) !== 0;
    const sourceSubUs = Math.max(0, Math.min(999, timestampSubUs | 0));
    const ptsUs = (timestamp >>> 0) * 1000 + sourceSubUs;
    let sampleToken = -1;
    if (this.diagnosticsEnabled) {
      const sourceToReceive = this._serverClock
        ? Math.max(
            0,
            estimateSourceToReceiveMs(
              timestamp >>> 0,
              receiveT,
              this._serverClock,
            ) -
              sourceSubUs / 1000,
          )
        : NaN;

      // Per-surface frame timeline sample.
      let samples = this._surfaceFrameSamples.get(surfaceId);
      if (!samples) {
        samples = new SurfaceFrameHistory(SurfaceStore.FRAME_SAMPLE_MAX);
        this._surfaceFrameSamples.set(surfaceId, samples);
      }
      sampleToken = samples.push(
        receiveT,
        timestamp >>> 0,
        sourceSubUs,
        ptsUs,
        data.length,
        isKey,
        sourceToReceive,
      );
    }

    const codec = codecFromFlags(flags);

    let entry = this.decoders.get(surfaceId);
    if (!entry || entry.codec !== codec) {
      if (entry) {
        safeClose(entry.decoder);
      }
      this.decoders.delete(surfaceId);
      this._pendingFrameSamples.delete(surfaceId);
      this._pendingFrameReceiveTimes.delete(surfaceId);
      this.initDecoder(surfaceId, codec, width, height);
      entry = this.decoders.get(surfaceId);
    }
    if (!entry) {
      // No decoder — ACK immediately so the server doesn't stall.
      this.sendAck(surfaceId);
      // initDecoder could not configure one for the codec string it had.
      // A re-subscribe rebuilds the session and re-announces that string,
      // which is the only thing that can change the outcome.  Rate-limited
      // and capped, so a decoder that can never be built costs a handful of
      // requests rather than one per frame.
      this.retryUnconfigured(surfaceId);
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
      // Deltas arriving while we wait mean the keyframe this flag was set
      // for may never come on its own — the reconfigure path relies on the
      // server's promise that a rebuilt session opens with one, and that
      // opening frame can be lost.  Ask instead of dropping forever.  The
      // Share the recovery budget with decoder errors. An asynchronous
      // decoder failure removes its decoder; without the shared budget, the
      // first delta seen by each replacement decoder could send a second
      // immediate subscribe and recreate the original per-frame loop.
      if (!entry.keyframeRequested) {
        entry.keyframeRequested = true;
        this.retryUnconfigured(surfaceId);
      }
      return;
    }

    // A scaled subscription can change the stream resolution without the
    // Wayland surface changing at all.  Moving a native 1920x1080 surface
    // from a 320x180 dock thumbnail back into a pane is exactly that: there
    // is no SURFACE_RESIZED to reset the decoder, only a new full-size
    // keyframe.  H.264 reconfigures from the SPS below; AV1 has no separate
    // description, so explicitly re-apply its configuration for the new
    // dimensions.  Replace the decoder instead of reconfiguring it in
    // place: Chromium accepts configure() but its AV1 decoder can stop
    // producing output after that resolution transition.  A fresh decoder
    // also gives the new keyframe a clean reference chain.  The old one is
    // flushed asynchronously; its output callback rejects it by identity.
    // Discard presentation and correlation state with it so nothing from
    // the thumbnail stream can be painted or matched after the restore.
    const streamDimensionsChanged =
      entry.lastConfiguredWidth > 0 &&
      entry.lastConfiguredHeight > 0 &&
      (width !== entry.lastConfiguredWidth ||
        height !== entry.lastConfiguredHeight);
    if (isKey && streamDimensionsChanged) {
      entry = this.replaceDecoder(surfaceId, codec, width, height);
      if (!entry) {
        this.sendAck(surfaceId);
        this.retryUnconfigured(surfaceId);
        return;
      }
    }
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
              if (dimsChanged) this.discardPresenter(surfaceId);
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
                colorSpace: LIMITED_RANGE_BT601,
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

      // An AV1 decoder can only be here if its configure() was skipped or
      // undone; the frame in hand names its codec, so configure from that
      // rather than dropping it (see {@link av1CodecString}).
      if (codec === "av1" && entry.decoder.state === "unconfigured") {
        if (!this.configureAv1Decoder(entry, surfaceId, width, height)) {
          this.decoders.delete(surfaceId);
        }
      }

      // Guard: don't decode if the decoder was never configured
      // (e.g., old server without VPS/SPS/PPS or HVCC prefix).
      if (entry.decoder.state !== "configured") {
        this._diag.dropped++;
        this.sendAck(surfaceId);
        // Nothing else will configure this decoder on its own — ask for a
        // keyframe, which re-subscribes and rebuilds the session (and with
        // it the codec announcement).  Rate-limited and capped.
        this.retryUnconfigured(surfaceId);
        return;
      }
      const chunk = new EncodedVideoChunk({
        type: isKey ? "key" : "delta",
        timestamp: ptsUs,
        data: frameData,
      });
      if (sampleToken >= 0) {
        let pending = this._pendingFrameSamples.get(surfaceId);
        if (!pending) {
          pending = new PendingFrameSamples(SurfaceStore.FRAME_SAMPLE_MAX);
          this._pendingFrameSamples.set(surfaceId, pending);
        }
        pending.push(ptsUs, sampleToken);
      }
      let pendingReceive = this._pendingFrameReceiveTimes.get(surfaceId);
      if (!pendingReceive) {
        pendingReceive = new PendingFrameSamples(SurfaceStore.FRAME_SAMPLE_MAX);
        this._pendingFrameReceiveTimes.set(surfaceId, pendingReceive);
      }
      pendingReceive.push(ptsUs, receiveT);
      entry.decoder.decode(chunk);
      // decode() accepted the keyframe. Deltas may now be queued behind it,
      // but keep the request latch armed until the output callback proves
      // that the decoder actually produced a frame.
      if (isKey) entry.pendingKeyframe = false;
      this._diag.decoded++;

      // ACK immediately with decodeQueueSize.  Deferring until output would
      // mix decode latency into delivery accounting; queue depth reports the
      // same pressure directly and independently of path RTT.
      this.sendAck(surfaceId);
    } catch (e) {
      this._pendingFrameReceiveTimes.get(surfaceId)?.takeByPts(ptsUs);
      if (sampleToken >= 0) {
        const pending = this._pendingFrameSamples.get(surfaceId);
        pending?.removeToken(sampleToken);
      }
      console.warn(
        "[blit] surface decode error:",
        surfaceId,
        codec,
        `${width}x${height}`,
        isKey ? "key" : "delta",
        `${data.length}B`,
        "head=" +
          Array.from(data.slice(0, 24))
            .map((b) => b.toString(16).padStart(2, "0"))
            .join(""),
        "cs=" + this.codecStrings.get(surfaceId),
        e,
      );
      if (entry) entry.pendingKeyframe = true;
      this._diag.errors++;
      this._surfaceErrors.set(
        surfaceId,
        (this._surfaceErrors.get(surfaceId) ?? 0) + 1,
      );
      this.noteDecodeFailure(surfaceId, codec);
      // Error — ACK immediately so the server doesn't permanently stall.
      this.sendAck(surfaceId);
      // Ask the server for a keyframe so the decoder can recover.
      // Fire at most once per pendingKeyframe episode — each request is
      // a SURFACE_SUBSCRIBE on the wire and resets server-side pacing.
      // The flag is cleared when a keyframe decodes successfully.
      if (entry) {
        entry.keyframeRequested = true;
      }
      this.retryUnconfigured(surfaceId);
    }
  }

  handleSurfaceTitle(surfaceId: number, title: string): void {
    const surface = this.surfaces.get(surfaceId);
    if (surface) {
      this.surfaces.set(surfaceId, { ...surface, title });
      this.emitChange();
    }
  }

  handleSurfaceCursor(
    surfaceId: number,
    shape: string,
    image: SurfaceCursorImage = shape === "none"
      ? { kind: "hidden" }
      : { kind: "named", name: shape },
  ): void {
    this.releaseCursorImage(this.cursorImages.get(surfaceId), image);
    this.cursorShapes.set(surfaceId, shape);
    this.cursorImages.set(surfaceId, image);
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

  /** Get the cursor artwork used for another viewer's pointer overlay. */
  getCursorImage(surfaceId: number): SurfaceCursorImage {
    return (
      this.cursorImages.get(surfaceId) ?? { kind: "named", name: "default" }
    );
  }

  /** Register a callback for cursor shape changes. Returns unsubscribe fn. */
  onCursor(listener: (surfaceId: number, shape: string) => void): () => void {
    this.cursorListeners.add(listener);
    return () => {
      this.cursorListeners.delete(listener);
    };
  }
  /**
   * Replace one kind of mark for a surface. An empty `points` retires that kind
   * and leaves the other alone.
   */
  handleRemoteInput(
    surfaceId: number,
    kind: RemoteInputKind,
    points: readonly RemoteSurfacePointer[],
  ): void {
    const previous = this.remoteInputs.get(surfaceId) ?? null;
    const merged: RemoteSurfaceInput = {
      pointer: kind === "pointer" ? points : (previous?.pointer ?? []),
      touch: kind === "touch" ? points : (previous?.touch ?? []),
    };
    const next =
      merged.pointer.length === 0 && merged.touch.length === 0 ? null : merged;
    // Idempotent. A repeated retire, or marks that did not move, must not wake
    // the overlay: each notification rewrites SVG attributes, and this arrives
    // at the remote user's full mouse or touch rate across every mounted view
    // of the surface.
    if (sameRemoteInput(previous, next)) return;
    if (next) this.remoteInputs.set(surfaceId, next);
    else this.remoteInputs.delete(surfaceId);
    for (const listener of this.remoteInputListeners) {
      try {
        listener(surfaceId, next);
      } catch {}
    }
  }

  getRemoteInput(surfaceId: number): RemoteSurfaceInput | null {
    return this.remoteInputs.get(surfaceId) ?? null;
  }

  onRemoteInput(
    listener: (surfaceId: number, input: RemoteSurfaceInput | null) => void,
  ): () => void {
    this.remoteInputListeners.add(listener);
    return () => {
      this.remoteInputListeners.delete(listener);
    };
  }

  /** A Wayland client asked for its toplevel to be activated
   *  (xdg_activation_v1 — e.g. a notification click).  Not a state change,
   *  so it goes to dedicated listeners, not the change cycle. */
  handleSurfaceActivated(surfaceId: number): void {
    if (!this.surfaces.has(surfaceId)) return;
    for (const listener of this.activationListeners) {
      try {
        listener(surfaceId);
      } catch {}
    }
  }

  /** Register a callback for surface activation requests. Returns unsubscribe fn. */
  onActivated(listener: (surfaceId: number) => void): () => void {
    this.activationListeners.add(listener);
    return () => {
      this.activationListeners.delete(listener);
    };
  }

  handleSurfaceTextInput(
    surfaceId: number,
    event: SurfaceTextInputEvent,
  ): void {
    const state: SurfaceTextInputState = {
      enabled: event.enabled,
      hint: event.hint >>> 0,
      purpose: event.purpose >>> 0,
    };
    if (state.enabled) this.textInputs.set(surfaceId, state);
    else this.textInputs.delete(surfaceId);
    // Deliberately notify repeated enables. They identify a new focused text
    // field even when its content type happens to match the previous one.
    for (const listener of this.textInputListeners) {
      try {
        listener(surfaceId, { ...state, requested: event.requested });
      } catch {}
    }
  }

  getTextInput(surfaceId: number): SurfaceTextInputState | null {
    return this.textInputs.get(surfaceId) ?? null;
  }

  onTextInput(
    listener: (surfaceId: number, state: SurfaceTextInputEvent) => void,
  ): () => void {
    this.textInputListeners.add(listener);
    return () => this.textInputListeners.delete(listener);
  }

  handleSurfaceEncoder(surfaceId: number, rawPayload: string): void {
    // Format: "encoder-name\0codec-string" (NUL-separated).
    const nul = rawPayload.indexOf("\0");
    const encoderName = nul >= 0 ? rawPayload.slice(0, nul) : rawPayload;
    const codecString = nul >= 0 ? rawPayload.slice(nul + 1) : null;
    this.encoderNames.set(surfaceId, encoderName);
    if (codecString) {
      this.codecStrings.set(surfaceId, codecString);
      if (codecString.startsWith("av01")) {
        this.av1CodecStrings.set(surfaceId, codecString);
      }
      // A rebuilt session can change the stream's profile or level
      // mid-subscription —
      // resizing a pane across an AV1 level boundary (~2254px wide at
      // 2094 tall flips av01.0.09M ↔ av01.0.13M) re-announces the codec
      // string, while switching between compositor 4:4:4 and a thumbnail
      // encoder changes profile 1 ↔ 0 at the same dimensions.  A live
      // decoder configured for the old string rejects the stream that
      // follows.  H.264 re-derives its config from in-band SPS; AV1 has no
      // separate description, so replace its decoder here.
      // The announcement always precedes the new session's opening
      // keyframe, and pendingKeyframe drops any stale deltas in between.
      const entry = this.decoders.get(surfaceId);
      // Selection churn announces the whole preference walk — an avc1
      // string can arrive while an AV1 decoder is live (and vice versa)
      // before the codec actually switches.  Only apply a same-codec
      // string to a live decoder; a real codec switch replaces the
      // decoder when its first frame arrives (handleSurfaceFrame).
      //
      // The comparison is against what the decoder was actually configured
      // with, not against the previously announced string: those differ
      // whenever the decoder had to fall back to a derived string, and that
      // is exactly the case this authoritative announcement fixes.
      if (
        codecString.startsWith("av01") &&
        entry &&
        entry.codec === "av1" &&
        entry.lastCodecString !== codecString &&
        entry.lastConfiguredWidth > 0 &&
        entry.lastConfiguredHeight > 0
      ) {
        const replacement = this.replaceDecoder(
          surfaceId,
          "av1",
          entry.lastConfiguredWidth,
          entry.lastConfiguredHeight,
        );
        if (!replacement) {
          this.retryUnconfigured(surfaceId);
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

  handleSurfaceResized(
    surfaceId: number,
    width: number,
    height: number,
    logicalWidth = 0,
    logicalHeight = 0,
  ): void {
    const surface = this.surfaces.get(surfaceId);
    if (!surface) {
      // Unknown id: either this resize overtook its create (stash it — see
      // `_pendingResizes`) or it trails a destroy (ignore it, or it would
      // be replayed onto the next surface to reuse the id).
      if (!this._destroyedSurfaceIds.has(surfaceId)) {
        this._pendingResizes.set(surfaceId, {
          width,
          height,
          logicalWidth,
          logicalHeight,
        });
      }
      return;
    }
    // The logical size can move while the physical size holds still — a
    // high-DPI viewer joining or leaving rescales the window without
    // changing how many pixels it composites to — and that alone changes
    // how large every viewer should draw it, so it gates the update too.
    const logicalChanged =
      logicalWidth > 0 &&
      logicalHeight > 0 &&
      (surface.logicalWidth !== logicalWidth ||
        surface.logicalHeight !== logicalHeight);
    if (
      surface.width !== width ||
      surface.height !== height ||
      logicalChanged
    ) {
      // Only emit a change for significant resizes (> 1px) to avoid
      // triggering a BSP re-render → ResizeObserver → resize feedback loop
      // from sub-pixel rounding in the compositor's physical↔logical
      // conversion.  The initial 0x0 → real size always emits.
      const significant =
        surface.width === 0 ||
        surface.height === 0 ||
        Math.abs(surface.width - width) > 1 ||
        Math.abs(surface.height - height) > 1 ||
        (logicalChanged &&
          (surface.logicalWidth === 0 ||
            Math.abs(surface.logicalWidth - logicalWidth) > 1 ||
            Math.abs(surface.logicalHeight - logicalHeight) > 1));
      const resolutionChanged =
        surface.width !== width || surface.height !== height;
      surface.width = width;
      surface.height = height;
      // A server that predates the field sends nothing; leave the last
      // known value rather than clobbering it with a bogus 0.
      if (logicalWidth > 0 && logicalHeight > 0) {
        surface.logicalWidth = logicalWidth;
        surface.logicalHeight = logicalHeight;
      }
      // Only the physical size reaches the decoder.  A logical-only
      // change is a presentation change — the stream keeps arriving at
      // the same resolution, so tearing the presenter down and spending a
      // keyframe on it would cost a visible stall for nothing.
      if (resolutionChanged) {
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
    this.clearCursors();
    this.clearRemoteInputs();
    this.clearTextInputs();
    this.discardAllPresenters();
    for (const entry of this.decoders.values()) {
      safeClose(entry.decoder);
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    this._pendingResizes.clear();
    this._destroyedSurfaceIds.clear();
    this.encoderNames.clear();
    this.codecStrings.clear();
    this.av1CodecStrings.clear();
    this._decodeFailStreak.clear();
    this._unconfiguredRetry.clear();
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._surfaceDrops.clear();
    this._surfaceErrors.clear();
    this._pendingFrameSamples.clear();
    this._pendingFrameReceiveTimes.clear();
    this.clearServerClock();
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
    this.clearCursors();
    this.clearRemoteInputs();
    this.clearTextInputs();
    this.discardAllPresenters();
    for (const entry of this.decoders.values()) {
      safeClose(entry.decoder);
    }
    this.decoders.clear();
    this.canvases.clear();
    this.surfaces.clear();
    this._pendingResizes.clear();
    this._destroyedSurfaceIds.clear();
    this.encoderNames.clear();
    this.codecStrings.clear();
    this.av1CodecStrings.clear();
    this._decodeFailStreak.clear();
    this._unconfiguredRetry.clear();
    this._surfaceFrameSamples.clear();
    this._surfaceOutputSamples.clear();
    this._surfaceDrops.clear();
    this._surfaceErrors.clear();
    this._pendingFrameSamples.clear();
    this._pendingFrameReceiveTimes.clear();
    this.clearServerClock();
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

  private clearRemoteInput(surfaceId: number): void {
    if (!this.remoteInputs.delete(surfaceId)) return;
    for (const listener of this.remoteInputListeners) {
      try {
        listener(surfaceId, null);
      } catch {}
    }
  }

  private releaseCursorImage(
    previous: SurfaceCursorImage | undefined,
    next?: SurfaceCursorImage,
  ): void {
    if (
      previous?.kind !== "custom" ||
      (next?.kind === "custom" && next.url === previous.url)
    ) {
      return;
    }
    try {
      URL.revokeObjectURL(previous.url);
    } catch {}
  }

  private clearCursor(surfaceId: number): void {
    this.releaseCursorImage(this.cursorImages.get(surfaceId));
    this.cursorImages.delete(surfaceId);
    this.cursorShapes.delete(surfaceId);
  }

  private clearCursors(): void {
    for (const image of this.cursorImages.values()) {
      this.releaseCursorImage(image);
    }
    this.cursorImages.clear();
    this.cursorShapes.clear();
  }

  private clearRemoteInputs(): void {
    const surfaceIds = [...this.remoteInputs.keys()];
    this.remoteInputs.clear();
    for (const surfaceId of surfaceIds) {
      for (const listener of this.remoteInputListeners) {
        try {
          listener(surfaceId, null);
        } catch {}
      }
    }
  }

  private clearTextInput(surfaceId: number): void {
    const previous = this.textInputs.get(surfaceId);
    if (!previous) return;
    this.textInputs.delete(surfaceId);
    for (const listener of this.textInputListeners) {
      try {
        listener(surfaceId, {
          enabled: false,
          requested: false,
          hint: 0,
          purpose: 0,
        });
      } catch {}
    }
  }

  private clearTextInputs(): void {
    for (const surfaceId of [...this.textInputs.keys()]) {
      this.clearTextInput(surfaceId);
    }
  }

  // -----------------------------------------------------------------------
  // Private
  // -----------------------------------------------------------------------

  /** Push a decoded frame into the surface's presenter, paint the very
   *  first one synchronously, and schedule the next vsync tick. */
  private enqueueFrame(
    surfaceId: number,
    frame: VideoFrame,
    sampleToken: number = -1,
    receiveT?: number,
  ): void {
    let p = this.presenters.get(surfaceId);
    if (!p) {
      const presenter: SurfacePresenter = {
        queue: [],
        sampleTokens: [],
        rafId: null,
        rafCallback: (frameTimeMs) => {
          presenter.rafId = null;
          this.noteRafInterval(frameTimeMs);
          this.tickPresent(surfaceId, frameTimeMs);
        },
        initialized: false,
        offsets: new RollingQuantile(SurfaceStore.OFFSET_WINDOW_MAX),
        decodeDelays: new RollingQuantile(SurfaceStore.OFFSET_WINDOW_MAX),
        fastOffsetMs: 0,
        presentOffsetMs: NaN,
        lastPtsMs: null,
        steadyRun: 0,
        frameIntervalMs: SurfaceStore.DEFAULT_REFRESH_MS,
        smoothing: false,
      };
      p = presenter;
      this.presenters.set(surfaceId, p);
    }

    if (this.presentationSmoothingEnabled) {
      this.trackArrival(p, frame, receiveT);
    }

    // Low-latency mode and same-host paths both bypass the playout queue.
    // Drawing here can still make the browser's imminent composite; waiting
    // for a newly requested rAF adds up to a full refresh of pure input lag.
    if (!this.presentationSmoothingEnabled || this.isLocalFastPath()) {
      if (p.rafId !== null) {
        cancelAnimationFrame(p.rafId);
        p.rafId = null;
      }
      for (const queued of p.queue) {
        try {
          queued.close();
        } catch {
          /* already closed */
        }
      }
      if (p.queue.length > 0) this._diag.dropped += p.queue.length;
      p.queue.length = 0;
      p.sampleTokens.length = 0;
      p.initialized = true;
      p.smoothing = false;
      this.presentFrame(surfaceId, frame, sampleToken);
      return;
    }

    if (!p.initialized) {
      p.initialized = true;
      this.presentFrame(surfaceId, frame, sampleToken);
      return;
    }

    p.queue.push(frame);
    p.sampleTokens.push(sampleToken);

    // Hidden tabs never fire rAF, but frames already in the decoder can keep
    // arriving while the connection's unsubscribe crosses the wire (and a
    // standalone store may remain subscribed). Present immediately instead
    // of queueing so every frame is closed promptly and the backing canvas
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
      removePrefixInPlace(p.queue, excess);
      removePrefixInPlace(p.sampleTokens, excess);
      this._diag.dropped += excess;
    }

    this.schedulePresent(surfaceId);
  }

  /** Fold one arrival into the presenter's clock model and decide whether
   *  this surface is streaming continuously enough to schedule off PTS. */
  private trackArrival(
    p: SurfacePresenter,
    frame: VideoFrame,
    receiveT?: number,
  ): void {
    const nowMs = performance.now();
    // VideoFrame.timestamp is µs; negotiated frames carry a u16
    // microseconds-within-the-ms field after the base wire header.
    const ptsMs = frame.timestamp / 1000;

    // No usable PTS — stay on newest-wins.  Scheduling against a NaN due
    // time would mean no frame ever compares as due and the surface would
    // freeze outright, which is far worse than the judder being fixed here.
    if (!Number.isFinite(ptsMs)) {
      p.offsets.clear();
      p.decodeDelays.clear();
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
      p.offsets.clear();
      p.decodeDelays.clear();
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

    const hasReceiveT = Number.isFinite(receiveT);
    const receiveOffsetMs = hasReceiveT ? receiveT! - ptsMs : nowMs - ptsMs;
    const decodeDelayMs = hasReceiveT ? Math.max(0, nowMs - receiveT!) : 0;
    this.updateSchedule(p, receiveOffsetMs, decodeDelayMs);

    p.lastPtsMs = ptsMs;
    p.steadyRun++;
    if (p.steadyRun >= SurfaceStore.SMOOTHING_ENGAGE_FRAMES) p.smoothing = true;
  }

  /** Trim the offset window to ~{@link OFFSET_WINDOW_MS} of stream and map
   *  PTS onto a bounded late-arrival quantile.
   *
   *  The old fastest-path schedule added zero playout latency, but it also
   *  exposed every reliable-stream ACK/GC stall as a frozen canvas followed
   *  by a burst. A high quantile turns recurring jitter into a small steady
   *  delay. The low quantile remains the baseline so the added delay is
   *  observable and queue depth can be derived from it. */
  private updateSchedule(
    p: SurfacePresenter,
    receiveOffsetMs: number,
    decodeDelayMs: number,
  ): void {
    const interval = this.validFrameInterval(p);
    const maxPlayoutDelayMs = this.isLocalFastPath()
      ? 0
      : SurfaceStore.MAX_PLAYOUT_DELAY_MS;
    const window = Math.min(
      SurfaceStore.OFFSET_WINDOW_MAX,
      Math.max(
        SurfaceStore.OFFSET_WINDOW_MIN,
        Math.round(SurfaceStore.OFFSET_WINDOW_MS / interval),
      ),
    );
    p.offsets.push(receiveOffsetMs, window);
    p.decodeDelays.push(decodeDelayMs, window);
    const fastReceiveOffsetMs = p.offsets.quantile(SurfaceStore.FAST_QUANTILE);
    const fastDecodeDelayMs = p.decodeDelays.quantile(
      SurfaceStore.FAST_QUANTILE,
    );
    p.fastOffsetMs = fastReceiveOffsetMs + fastDecodeDelayMs;
    const lateReceiveOffsetMs = p.offsets.quantile(
      Math.max(0, 1 - SurfaceStore.PLAYOUT_OUTLIERS / p.offsets.length),
    );
    const targetOffsetMs =
      p.fastOffsetMs +
      Math.min(
        maxPlayoutDelayMs,
        Math.max(0, lateReceiveOffsetMs - fastReceiveOffsetMs),
      );
    if (
      !Number.isFinite(p.presentOffsetMs) ||
      targetOffsetMs >= p.presentOffsetMs
    ) {
      p.presentOffsetMs = targetOffsetMs;
    } else {
      // Clamp first in case the whole path got faster; an old absolute
      // offset must never turn into more than MAX_PLAYOUT_DELAY_MS of margin.
      const currentOffsetMs = Math.min(
        p.presentOffsetMs,
        p.fastOffsetMs + maxPlayoutDelayMs,
      );
      p.presentOffsetMs = Math.max(
        targetOffsetMs,
        currentOffsetMs - interval * SurfaceStore.PLAYOUT_SLEW_DOWN_PER_FRAME,
      );
    }
  }

  private isLocalFastPath(): boolean {
    const rttMs = this._serverClock?.rttMs;
    return rttMs !== undefined && rttMs <= SurfaceStore.LOCAL_RTT_MAX_MS;
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
   *  There is no high-rate ceiling: the cap grows directly from the learned
   *  positive interval.  A non-positive or non-finite interval is not a
   *  cadence and falls back to the initial refresh estimate. */
  private smoothedQueueCap(p: SurfacePresenter): number {
    const interval = this.validFrameInterval(p);
    const span = Math.ceil(this.playoutDelayMs(p) / interval);
    return Math.max(span + 2, SurfaceStore.PRESENT_QUEUE_MAX);
  }

  private validFrameInterval(p: SurfacePresenter): number {
    return Number.isFinite(p.frameIntervalMs) && p.frameIntervalMs > 0
      ? p.frameIntervalMs
      : SurfaceStore.DEFAULT_REFRESH_MS;
  }

  private schedulePresent(surfaceId: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.rafId !== null) return;
    p.rafId = requestAnimationFrame(p.rafCallback);
  }

  /** Track the display's refresh period from rAF deltas.  Accepts every
   *  positive cadence through {@link RAF_DELTA_MAX_MS} (10 Hz) and ignores
   *  longer gaps as a stalled or backgrounded tick.
   *
   *  Use the timestamp supplied by rAF, not `performance.now()`. Every rAF
   *  callback in one browser frame receives the same timestamp, while the
   *  wall clock advances as earlier surfaces draw. Measuring the latter
   *  made a multi-pane frame's draw time look like a 1–3 ms display period
   *  and corrupted the shared presentation clock. */
  private noteRafInterval(now: number): void {
    if (this.lastRafMs !== null) {
      const dt = now - this.lastRafMs;
      // A 10 Hz tick is a real cadence on a loaded machine or an
      // occluded window, and the rounding window should match whatever the
      // page is actually painting at.  The cost of admitting it is that a
      // transient stall drags the estimate up and presents slightly early
      // until it recovers — one 100 ms sample moves a 60 Hz estimate to
      // ~25 ms, about 4 ms of extra lookahead, gone within ten frames at
      // the 0.1 EWMA weight.  Cheaper than mistaking a slow display for a
      // fast one.
      if (dt > 0 && dt <= SurfaceStore.RAF_DELTA_MAX_MS) {
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
  private tickPresent(surfaceId: number, frameTimeMs: number): void {
    const p = this.presenters.get(surfaceId);
    if (!p || p.queue.length === 0) return;

    if (!p.smoothing || !Number.isFinite(p.presentOffsetMs)) {
      this.presentIndex(surfaceId, p, p.queue.length - 1);
      return;
    }

    // rAF fires just before the next composite, so what is drawn now lands
    // one refresh from here.  Rounding by half a refresh picks the nearest
    // vsync rather than always the later one.
    // The same rAF timestamp also gives every surface in this browser frame
    // one presentation deadline. `performance.now()` would make later panes
    // appear due several milliseconds later merely because earlier panes
    // took time to draw.
    const deadline = frameTimeMs + this.refreshMs / 2;
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
    const sampleToken = p.sampleTokens[idx];
    removePrefixInPlace(p.queue, idx + 1);
    removePrefixInPlace(p.sampleTokens, idx + 1);
    this.presentFrame(surfaceId, chosen, sampleToken);
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
  private presentFrame(
    surfaceId: number,
    frame: VideoFrame,
    sampleToken: number,
  ): void {
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
    // Frame listeners synchronously copy the shared backing canvas into
    // every visible canvas. Record after them, not after the backing draw,
    // so the client-side stage includes all CPU work required to submit the
    // visible frame. Physical scanout is estimated separately in the debug
    // pane because browsers expose no presentation timestamp for canvas.
    if (sampleToken >= 0)
      this._surfaceFrameSamples
        .get(surfaceId)
        ?.markPresented(sampleToken, performance.now());
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
      p.offsets.clear();
      p.decodeDelays.clear();
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

  /** Replace a decoder at a stream boundary.
   *
   * Chromium can accept an AV1 configure() that changes resolution,
   * profile, or level and then stop producing output.  A new instance is
   * the reliable boundary.  The old instance drains asynchronously, and
   * its output callback drops every frame once the map points elsewhere. */
  private replaceDecoder(
    surfaceId: number,
    codec: SurfaceCodec,
    width: number,
    height: number,
  ): DecoderEntry | undefined {
    const previous = this.decoders.get(surfaceId);
    if (previous) safeClose(previous.decoder);
    this.decoders.delete(surfaceId);
    this._pendingFrameSamples.delete(surfaceId);
    this._pendingFrameReceiveTimes.delete(surfaceId);
    this.discardPresenter(surfaceId);
    this.initDecoder(surfaceId, codec, width, height);
    return this.decoders.get(surfaceId);
  }

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
        const active = this.decoders.get(surfaceId);
        // safeClose() flushes before closing so Chromium releases every
        // VideoFrame cleanly.  Those outputs can arrive after a replacement
        // decoder is installed, and do not belong in the current presenter.
        // Decoder identity is the stream boundary.  Do not compare the
        // output dimensions with the configure() hint: WebCodecs is allowed
        // to derive display dimensions from the AV1 sequence header, and
        // rejecting that active decoder's output leaves the last thumbnail
        // painted forever even while native-size frames keep arriving.
        if (active?.decoder !== decoder) {
          this._diag.dropped++;
          this._pendingFrameReceiveTimes
            .get(surfaceId)
            ?.takeByPts(frame.timestamp);
          const staleSample = this._pendingFrameSamples
            .get(surfaceId)
            ?.takeByPts(frame.timestamp);
          if (staleSample !== undefined && staleSample >= 0) {
            this._surfaceDrops.set(
              surfaceId,
              (this._surfaceDrops.get(surfaceId) ?? 0) + 1,
            );
          }
          try {
            frame.close();
          } catch {
            /* already closed */
          }
          return;
        }
        this._diag.output++;
        // A decoded frame ends any failure streak — demotion is for
        // streams this platform cannot decode at all — and proves the
        // decoder is configured, so the retry budget starts fresh.
        this._decodeFailStreak.delete(surfaceId);
        this._unconfiguredRetry.delete(surfaceId);
        active.pendingKeyframe = false;
        active.keyframeRequested = false;

        const outputT = performance.now();
        const receiveT =
          this._pendingFrameReceiveTimes
            .get(surfaceId)
            ?.takeByPts(frame.timestamp) ?? -1;
        let sampleToken = -1;
        if (this.diagnosticsEnabled) {
          // Per-surface output sample for debug panel rate computation.
          let outputs = this._surfaceOutputSamples.get(surfaceId);
          if (!outputs) {
            outputs = new NumberRing(SurfaceStore.OUTPUT_SAMPLE_MAX);
            this._surfaceOutputSamples.set(surfaceId, outputs);
          }
          outputs.push(outputT);

          const pending = this._pendingFrameSamples.get(surfaceId);
          sampleToken = pending?.takeByPts(frame.timestamp) ?? -1;
          if (sampleToken >= 0)
            this._surfaceFrameSamples
              .get(surfaceId)
              ?.markDecoded(sampleToken, outputT);
        }

        // Queue + paced presentation absorbs network/decoder jitter and
        // prevents 30 fps content from juddering on a 120 Hz display.
        // The first frame paints synchronously inside enqueueFrame to
        // minimise time-to-first-pixel.
        this.enqueueFrame(
          surfaceId,
          frame,
          sampleToken,
          receiveT >= 0 ? receiveT : undefined,
        );
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
        this.noteDecodeFailure(surfaceId, codec);
        // Ask the server for a keyframe so the next decoder gets a clean
        // reference point. Decoder errors can repeat once per incoming
        // frame, so share the same retry budget as configuration failures.
        this.retryUnconfigured(surfaceId);
      },
    });
    const entry: DecoderEntry = {
      decoder,
      codec,
      pendingKeyframe: true,
      keyframeRequested: false,
      lastCodecString: null,
      lastDescription: null,
      lastConfiguredWidth: 0,
      lastConfiguredHeight: 0,
    };
    this.decoders.set(surfaceId, entry);
    // Defer configure() until the first keyframe provides the codec
    // description (AVCC for H.264).  Configuring without a description
    // then reconfiguring with one causes VideoToolbox on macOS to drop
    // the first decoded frame.
    // AV1 has no description — configure it eagerly, from the announced
    // codec string when that describes AV1 and from the frames' own codec
    // otherwise.  {@link av1CodecString} explains the difference; a
    // decoder left unconfigured drops every frame it is handed.
    if (
      codec === "av1" &&
      !this.configureAv1Decoder(entry, surfaceId, width, height)
    ) {
      this.decoders.delete(surfaceId);
    }
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
