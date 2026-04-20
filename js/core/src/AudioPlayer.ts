/**
 * Audio playback pipeline with A/V sync: receives Opus frames from the
 * server, decodes via WebCodecs AudioDecoder, and plays through an
 * AudioContext with rate-adjusted resampling to stay in sync with video.
 *
 * Audio and video frames share a common server-side wall-clock timestamp
 * (milliseconds since compositor creation).  The worklet performs linear-
 * interpolation resampling at a variable rate (±5%) so audio can speed up
 * or slow down to track video.  Video is never delayed.
 *
 * Playback uses an AudioWorkletNode with an inline processor registered
 * from a Blob URL — no external file needed.
 */

/** Maximum jitter buffer depth in decoded frames (~20 ms each). */
const MAX_BUFFER_FRAMES = 25; // 500 ms

/**
 * Adaptive jitter buffer: the worklet starts at MIN_BUFFER_SAMPLES, grows
 * by one 20 ms frame on the leading edge of each underrun event, and
 * shrinks back one frame at a time after DECAY_STABLE_SAMPLES of
 * underrun-free playback.  No hard upper bound — the DRIFT_JUMP_MS skip
 * path reclaims excess latency if the buffer ever runs away.  Hysteresis
 * is provided by the MIN floor: once bufferTarget hits it, shrinking
 * stops.  Floor is three frames (60 ms) to absorb two back-to-back late
 * arrivals before the buffer empties; stable connections steady-state at
 * 60 ms while jittery ones self-size to whatever headroom they need.
 */
const MIN_BUFFER_SAMPLES = 2880; // 3 frames = 60 ms at 48 kHz

/**
 * Samples of uninterrupted, non-buffering playback required before
 * bufferTarget shrinks by one frame.  Set long enough that recurring
 * jitter (underrun every few seconds) never decays the buffer back
 * toward the floor between events — otherwise the buffer oscillates
 * and glitches on every cycle.  Shrinking is slow by design; growth
 * reacts within one event.
 */
const DECAY_STABLE_SAMPLES = 720000; // 15 s at 48 kHz

// -- A/V sync constants ----------------------------------------------------

/** How often the worklet reports its consumed-sample position (in samples). */
const POS_REPORT_INTERVAL = 4800; // ~100 ms at 48 kHz

/*
 * The steady-state target for `audioMs - lastVideoTimestampMs` is computed
 * per-position from the worklet's current bufferTarget (reported alongside
 * each pos message).  Audio held in the jitter buffer lags video by that
 * many ms, so treating the current (adaptive) depth as the equilibrium
 * keeps drift=0 aligned with "buffer at desired depth" even as the target
 * grows or shrinks during the session.
 */

/**
 * Drift dead-zone: don't adjust rate if |drift| is below this (ms).
 * Drift is already measured relative to the adaptive target, so zero
 * means "buffer at target depth".  Avoids oscillation when sync is good.
 */
const DRIFT_DEADZONE_MS = 10;

/**
 * Drift threshold for maximum correction (ms).  Beyond this we apply the
 * full ±MAX_RATE_OFFSET.  Between DEADZONE and this, we interpolate.
 */
const DRIFT_FULL_CORRECTION_MS = 300;

/** Maximum rate offset from 1.0 in either direction. */
const MAX_RATE_OFFSET = 0.02; // ±2%

/**
 * Drift threshold for a hard jump (ms).  When audio is *ahead* of video by
 * more than this, the worklet skips forward (drops old samples) to close the
 * gap without a full flush.  When audio is *behind* by more than this, we
 * just reset sync and let rate steering or new frames catch up — flushing
 * would discard the only audio we have, making the gap worse.
 */
const DRIFT_JUMP_MS = 500;

/** Minimum number of audio frames received before we start sync adjustment. */
const SYNC_WARMUP_FRAMES = 10;

/**
 * Exponential smoothing factor for rate changes.  Each update blends
 * α·target + (1−α)·previous.  Lower values smooth more aggressively
 * at the cost of slower convergence.  0.15 converges within ~1 s while
 * eliminating the wow-and-flutter artifacts from jittery drift readings.
 */
const RATE_SMOOTHING_ALPHA = 0.15;

/**
 * Consecutive render-block underruns before the worklet re-enters full
 * buffering mode.  A single underrun is usually just a scheduling hiccup
 * where the next PCM chunk is already in the port queue; three
 * consecutive underruns indicate a real gap.
 */
const UNDERRUN_REBUFFER_THRESHOLD = 3;

/** Samples per 20 ms Opus frame at 48 kHz (per-channel). */
const SAMPLES_PER_20_MS = 960;

/**
 * How many 20 ms frames to grow bufferTarget by on each underrun event.
 * Transport head-of-line blocking (audio serialized behind video bulk
 * writes on the same TCP stream) produces arrival gaps proportional to
 * the video bulk-write time — typically 100–200 ms on keyframes.  Growing
 * by a single frame makes convergence take dozens of audible underruns;
 * 5 frames (100 ms per event) reaches a buffer depth that absorbs those
 * bursts within a handful of events.  Decay (15 s of clean playback per
 * frame shrunk) claws back any overshoot.
 */
const GROW_FRAMES_PER_UNDERRUN = 5;

/**
 * Fade-envelope length in samples used to mask the waveform discontinuity at
 * underrun boundaries (real audio → forced-zero output → real audio again).
 * A hard jump from a non-zero sample to 0 is an audible click; ramping the
 * output gain over ~1.3 ms turns the click into an inaudible soft fade.
 */
const FADE_SAMPLES = 64;

/**
 * Inline AudioWorkletProcessor source.
 *
 * Runs on the audio render thread.  Receives Float32Array PCM frames
 * (f32-planar: [L...L, R...R]) via the MessagePort and drains them into
 * the output buffers using linear-interpolation resampling at a variable
 * rate.  Silence is output on underrun.
 *
 * Messages IN:
 *   Float32Array        — PCM frame to enqueue
 *   "flush"             — clear buffer
 *   { type: "rate", value: number } — set playback rate (default 1.0)
 *
 * Messages OUT:
 *   { type: "pos", value: number } — cumulative source samples consumed
 *                                     (reported every ~100 ms)
 */
const WORKLET_SRC = /* js */ `
class BlitAudioProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buffer = [];       // queue of Float32Array PCM chunks [L..L, R..R]
    this.offset = 0;        // integer sample offset into current chunk
    this.frac = 0;          // fractional sample position [0, 1)
    this.rate = 1.0;        // playback rate (1.0 = normal)
    this.consumed = 0;      // total source samples from fully-consumed chunks
    this.lastReport = 0;    // consumed+offset at last report
    this.buffered = 0;      // total samples currently buffered
    this.buffering = true;  // true while accumulating the jitter buffer
    this.bufferTarget = ${MIN_BUFFER_SAMPLES}; // adaptive: grows on underrun, shrinks on stability
    this.stableSamples = 0; // consumed samples of underrun-free playback (drives shrinking)
    this.underruns = 0;     // consecutive underruns, drives adaptive buffer growth
    this.fadeGain = 0;      // applied output gain (0..1), ramps to mask underrun clicks
    this.fadeInc = 1 / ${FADE_SAMPLES}; // per-sample ramp rate

    this.port.onmessage = (e) => {
      if (e.data === "flush") {
        this.buffer = [];
        this.offset = 0;
        this.frac = 0;
        this.consumed = 0;
        this.lastReport = 0;
        this.buffered = 0;
        this.buffering = true;
        this.underruns = 0;
        this.bufferTarget = ${MIN_BUFFER_SAMPLES};
        this.stableSamples = 0;
        this.fadeGain = 0;
      } else if (e.data && e.data.type === "skip") {
        // Drop samples from the front to reduce drift without a full
        // flush.  Keeps playback running (no re-buffering silence).
        const requested = e.data.samples | 0;
        let toSkip = requested;
        while (toSkip > 0 && this.buffer.length > 0) {
          const pcm = this.buffer[0];
          const half = pcm.length / 2;
          const remaining = half - this.offset;
          if (remaining <= toSkip) {
            this.consumed += half;
            this.buffered -= half;
            this.buffer.shift();
            this.offset = 0;
            toSkip -= remaining;
          } else {
            this.offset += toSkip;
            this.buffered -= toSkip;
            toSkip = 0;
          }
        }
        this.frac = 0;
        this.port.postMessage({
          type: "event",
          kind: "skip",
          requested,
          skipped: requested - toSkip,
        });
      } else if (e.data && e.data.type === "rate") {
        this.rate = e.data.value;
      } else {
        this.buffer.push(e.data);
        this.buffered += e.data.length / 2; // half = per-channel sample count
        // No hard buffer cap: the DRIFT_JUMP_MS skip path (main thread)
        // reclaims excess latency if something pathological accumulates.
      }
    };
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    if (!out || out.length < 2) return true;
    const outL = out[0];
    const outR = out[1];
    const needed = outL.length; // typically 128
    let written = 0;

    // Jitter buffer: don't start playing until we've accumulated enough
    // audio.  This absorbs network jitter and main-thread stalls.
    if (this.buffering && this.buffered >= this.bufferTarget) {
      this.buffering = false;
      this.port.postMessage({
        type: "event",
        kind: "rebuffer_end",
        target: this.bufferTarget,
        buffered: this.buffered,
      });
    }

    if (!this.buffering) while (written < needed && this.buffer.length > 0) {
      const pcm = this.buffer[0];
      const half = pcm.length / 2;
      if (half <= 0) {
        this.buffer.shift();
        this.offset = 0;
        continue;
      }

      // Current integer position in this chunk
      const i0 = this.offset;
      const i1 = i0 + 1;

      if (i0 >= half) {
        // Exhausted this chunk
        this.consumed += half;
        this.buffered -= half;
        this.buffer.shift();
        this.offset = 0;
        continue;
      }

      // Get samples at i0
      const l0 = pcm[i0];
      const r0 = pcm[half + i0];

      if (i1 < half) {
        // Linear interpolation with next sample in same chunk
        const t = this.frac;
        outL[written] = l0 + t * (pcm[i1] - l0);
        outR[written] = r0 + t * (pcm[half + i1] - r0);
      } else if (this.buffer.length > 1) {
        // At chunk boundary — interpolate with first sample of next chunk
        const next = this.buffer[1];
        const nextHalf = next.length / 2;
        if (nextHalf > 0) {
          const t = this.frac;
          outL[written] = l0 + t * (next[0] - l0);
          outR[written] = r0 + t * (next[nextHalf] - r0);
        } else {
          outL[written] = l0;
          outR[written] = r0;
        }
      } else {
        // No next chunk available — use current sample
        outL[written] = l0;
        outR[written] = r0;
      }
      written++;

      // Advance fractional position by rate
      this.frac += this.rate;
      const advance = this.frac | 0; // integer part
      this.frac -= advance;
      this.offset += advance;
    }

    // Fill the remainder of the block with zeros (underrun or buffering).
    for (let i = written; i < needed; i++) {
      outL[i] = 0;
      outR[i] = 0;
    }

    // Apply fade envelope: ramp the output gain toward 1 for samples that
    // came from real audio and toward 0 for the silence-padded tail.  A
    // hard non-zero → 0 jump at an underrun boundary is an audible click;
    // ramping over ~1.3 ms makes the transition inaudible.  The gain
    // persists across blocks, so a brief 1-sample dip barely attenuates.
    const fadeInc = this.fadeInc;
    let g = this.fadeGain;
    for (let i = 0; i < needed; i++) {
      const target = i < written ? 1 : 0;
      if (g < target) {
        g += fadeInc;
        if (g > target) g = target;
      } else if (g > target) {
        g -= fadeInc;
        if (g < target) g = target;
      }
      outL[i] *= g;
      outR[i] *= g;
    }
    this.fadeGain = g;

    // Underrun handling has two jobs:
    //   1. Grow bufferTarget on the *leading edge* of any underrun event,
    //      so a single-block hiccup also buys headroom — not just
    //      multi-block gaps.  Without this, short-but-frequent jitter
    //      never grows the buffer and keeps producing ticks.
    //   2. Re-enter buffering mode only when the gap is sustained
    //      (>= UNDERRUN_REBUFFER_THRESHOLD consecutive blocks), since a
    //      single-block dip is usually just scheduling and rebuffering
    //      would be worse than the dip itself.
    // Underrun blocks while already buffering don't count toward either —
    // the silence is intentional while the buffer refills.
    if (written < needed) {
      this.stableSamples = 0;
      if (this.consumed > 0 && !this.buffering) {
        this.underruns++;
        if (this.underruns === 1) {
          this.bufferTarget += ${SAMPLES_PER_20_MS * GROW_FRAMES_PER_UNDERRUN};
          this.port.postMessage({
            type: "event",
            kind: "grow",
            target: this.bufferTarget,
            buffered: this.buffered,
          });
        }
        if (this.underruns >= ${UNDERRUN_REBUFFER_THRESHOLD}) {
          this.buffering = true;
          this.port.postMessage({
            type: "event",
            kind: "rebuffer_start",
            target: this.bufferTarget,
            consecutive: this.underruns,
          });
        }
      }
    } else {
      // End of any underrun event.
      this.underruns = 0;
      // Adaptive shrink: after DECAY_STABLE_SAMPLES of underrun-free,
      // non-buffering playback, drop bufferTarget by one frame toward
      // MIN_BUFFER_SAMPLES.  The MIN floor is the hysteresis — once there,
      // shrinking halts until the next underrun grows the target again.
      if (!this.buffering) {
        this.stableSamples += needed;
        if (
          this.stableSamples >= ${DECAY_STABLE_SAMPLES} &&
          this.bufferTarget > ${MIN_BUFFER_SAMPLES}
        ) {
          this.bufferTarget = Math.max(
            this.bufferTarget - ${SAMPLES_PER_20_MS},
            ${MIN_BUFFER_SAMPLES}
          );
          this.stableSamples = 0;
          this.port.postMessage({
            type: "event",
            kind: "shrink",
            target: this.bufferTarget,
          });
        }
      }
    }

    // Report position periodically.  Include this.offset for accuracy
    // (consumed only counts fully-drained chunks).
    const totalPos = this.consumed + this.offset;
    if (totalPos - this.lastReport >= ${POS_REPORT_INTERVAL}) {
      this.lastReport = totalPos;
      this.port.postMessage({
        type: "pos",
        value: totalPos,
        target: this.bufferTarget,
        buffered: this.buffered,
      });
    }

    // Keep processor alive even during silence.
    return true;
  }
}
registerProcessor("blit-audio", BlitAudioProcessor);
`;

// -- Timeline entry for mapping samples → server timestamps ----------------

export class AudioPlayer {
  private ctx: AudioContext | null = null;
  private decoder: AudioDecoder | null = null;
  private worklet: AudioWorkletNode | null = null;
  private gain: GainNode | null = null;
  private _muted = true;
  private _subscribed = false;
  private _destroyed = false;

  /** Pending decoded PCM frames waiting to be posted to the worklet. */
  private buffer: Float32Array[] = [];

  private listeners = new Set<() => void>();

  /**
   * True while an `initAudioContext()` call is in flight.  Guards against
   * concurrent re-init attempts (e.g. two rapid `handleAudioFrame` calls
   * both detecting a dead context).
   */
  private initializingContext = false;

  // -- Rate servo state ---------------------------------------------------
  //
  // Audio runs a simple depth-based servo: if the worklet buffer sits
  // below `bufferTarget` we slow consumption (rate < 1) to refill;
  // above target we speed up (rate > 1) to drain.  Video is played
  // back at its own real-time pace with no explicit A/V sync — both
  // media are delivered as fast as the transport allows, so they stay
  // aligned within the ppm-level clock skew of the sample-rate
  // converters, which is imperceptible for sub-hour sessions.

  /** Last consumed-sample position reported by the worklet. */
  private samplesConsumed = 0;
  /** Number of audio frames received (for warmup). */
  private framesReceived = 0;
  /** Current playback rate sent to the worklet. */
  private currentRate = 1.0;
  /** Smoothed rate — exponentially filtered to avoid wow/flutter. */
  private smoothedRate = 1.0;
  /** Worklet's current adaptive bufferTarget (samples), mirrored from pos reports. */
  private currentBufferTarget = MIN_BUFFER_SAMPLES;
  /** Last observed buffered depth (samples, from pos reports) — feeds the drift servo. */
  private lastBufferedSamples = 0;

  // -- Stall detection / auto-recovery ------------------------------------

  /** Timestamp (ms) of the last audio frame received via handleAudioFrame. */
  private lastFrameAt = 0;
  /** Timestamp (ms) of the last worklet position report. */
  private lastWorkletReportAt = 0;
  /** Periodic health-check timer for stall detection. */
  private healthTimer: ReturnType<typeof setInterval> | null = null;
  /** Timestamp (ms) of the last automatic pipeline reset. */
  private lastAutoResetAt = 0;

  // -- Decoder output health tracking -------------------------------------

  /** Number of frames sent to decoder.decode(). */
  private decodesRequested = 0;
  /** Number of decoded frames received from the decoder output callback. */
  private framesDecoded = 0;
  /** Snapshot of decodesRequested at the last health check. */
  private lastHealthDecodesRequested = 0;
  /** Snapshot of framesDecoded at the last health check. */
  private lastHealthFramesDecoded = 0;
  /**
   * Whether a health check saw the decoder receive frames but produce no
   * output.  A single silent check (2 s) triggers a reset.
   */
  private decoderSilentLastCheck = false;
  /** Timestamp (ms) of the last decoded audio frame output. */
  private lastDecodedAt = 0;
  /** Timestamp (ms) when the AudioContext entered "suspended" state. */
  private suspendedSince = 0;
  /** Registered visibilitychange handler, for cleanup. */
  private visibilityHandler: (() => void) | null = null;

  get muted(): boolean {
    return this._muted;
  }

  get subscribed(): boolean {
    return this._subscribed;
  }

  /** Whether the browser supports WebCodecs AudioDecoder for Opus. */
  static get supported(): boolean {
    return typeof AudioDecoder !== "undefined";
  }

  onChange(fn: () => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private emit(): void {
    for (const fn of this.listeners) {
      try {
        fn();
      } catch {}
    }
  }

  // -- Public API ----------------------------------------------------------

  /** Toggle mute.  When unmuting, creates the AudioContext (requires user gesture). */
  setMuted(muted: boolean): void {
    if (this._muted === muted) return;
    this._muted = muted;
    if (this.gain) {
      this.gain.gain.value = muted ? 0 : 1;
    }
    if (!muted) {
      if (this.ctx && this.ctx.state === "closed") {
        // Context died (device change, resource pressure, etc.) — rebuild.
        this.teardownAudioContext();
        this.initAudioContext();
      } else if (!this.ctx) {
        this.initAudioContext();
      } else if (this.ctx.state === "suspended") {
        this.resumeOnGesture(this.ctx);
      }
    }
    this.emit();
  }

  /**
   * Resume a suspended AudioContext.  Browsers block AudioContext.resume()
   * unless it happens inside a user-gesture event handler.  When called
   * outside a gesture (e.g. on page load from persisted config) we
   * install a one-shot listener for the first click/keydown/touchstart so
   * that audio starts as soon as the user interacts with the page.
   */
  private resumeOnGesture(ctx: AudioContext): void {
    ctx.resume().catch(() => {});
    // If resume() worked synchronously (user-gesture context), done.
    if (ctx.state === "running") return;
    // Otherwise wait for the first user interaction to retry.
    const handler = () => {
      if (this._muted || this._destroyed) return;
      ctx.resume().catch(() => {});
    };
    const events: (keyof DocumentEventMap)[] = [
      "click",
      "keydown",
      "touchstart",
    ];
    const cleanup = () => {
      for (const evt of events)
        document.removeEventListener(evt, once, {
          capture: true,
        } as EventListenerOptions);
    };
    const once = () => {
      handler();
      cleanup();
    };
    for (const evt of events)
      document.addEventListener(evt, once, { capture: true, once: true });
    // Also clean up if the context resumes by other means (e.g. another
    // setMuted call with a gesture, or destroy).
    const onStateChange = () => {
      if (ctx.state === "running") {
        cleanup();
        ctx.removeEventListener("statechange", onStateChange);
      }
    };
    ctx.addEventListener("statechange", onStateChange);
  }

  /** Mark as subscribed (called by connection after sending C2S_AUDIO_SUBSCRIBE). */
  setSubscribed(subscribed: boolean): void {
    if (this._subscribed === subscribed) return;
    this._subscribed = subscribed;
    if (!subscribed) {
      this.buffer = [];
      this.worklet?.port.postMessage("flush");
      this.resetSync();
    }
    this.emit();
  }

  /** Handle an incoming S2C_AUDIO_FRAME. */
  handleAudioFrame(timestamp: number, _flags: number, data: Uint8Array): void {
    if (this._destroyed) return;
    const now = Date.now();
    this.lastFrameAt = now;
    if (this._muted) return;
    this.startHealthCheck();

    // Inline decoder stall check: if we've been feeding the decoder for
    // > 5 s but it hasn't produced any output, the decoder is dead.
    // Only reset the decoder — the AudioContext and worklet are fine.
    // If this doesn't help, the health-check escalates to a full reset.
    if (
      this.lastDecodedAt > 0 &&
      this.decodesRequested > 0 &&
      now - this.lastDecodedAt > 5_000
    ) {
      if (now - this.lastAutoResetAt > 10_000) {
        this.lastAutoResetAt = now;
        this.resetDecoder();
      }
      return;
    }

    // Recover from a dead or missing AudioContext.  The browser can close
    // the context at any time (audio device change, resource pressure, GPU
    // process crash, etc.).  A null context means resetPipeline() tore it
    // down and we need to rebuild from scratch.
    if (this.ctx && this.ctx.state === "closed") {
      this.teardownAudioContext();
      this.initAudioContext();
    } else if (!this.ctx) {
      this.initAudioContext();
    } else if (this.ctx.state === "suspended") {
      // Eagerly try to resume on every incoming frame rather than waiting
      // for the health-check poll.  On active tabs (user typing or
      // clicking) this succeeds immediately.
      this.ctx.resume().catch(() => {});
    }

    if (!this.decoder || this.decoder.state === "closed") {
      this.initDecoder();
    }
    if (!this.decoder || this.decoder.state !== "configured") return;

    // Record the server timestamp for this frame (used by sync controller).
    // The timestamp is now wall-clock ms (same epoch as video).
    this.framesReceived++;

    try {
      this.decoder.decode(
        new EncodedAudioChunk({
          type: "key", // Opus frames are independently decodable
          // WebCodecs wants microseconds; server sends wall-clock ms.
          timestamp: timestamp * 1000,
          data,
        }),
      );
      this.decodesRequested++;
    } catch {
      // Decoder threw — reset it so the next handleAudioFrame creates a
      // fresh one.  resetDecoder also clears stall-detection counters to
      // prevent the inline stall check from immediately nuking the
      // replacement decoder on the very next frame.
      this.resetDecoder();
    }
  }

  /** Called on connection reset / disconnect. */
  reset(): void {
    this._subscribed = false;
    this.buffer = [];
    this.worklet?.port.postMessage("flush");
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {}
    }
    this.decoder = null;
    this.lastFrameAt = 0;
    this.lastWorkletReportAt = 0;
    this.stopHealthCheck();
    this.resetSync();
    this.emit();
  }

  /**
   * Full pipeline reset: tears down the AudioContext, decoder, and all
   * state.  Everything rebuilds automatically on the next incoming audio
   * frame.  Use this to recover from stalled or broken audio without
   * reconnecting.  Unlike {@link reset}, this keeps the server subscription
   * intact — no re-subscribe round-trip is needed.
   */
  resetPipeline(): void {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {}
    }
    this.decoder = null;
    this.buffer = [];
    this.teardownAudioContext();
    this.lastFrameAt = 0;
    this.lastWorkletReportAt = 0;
    this.stopHealthCheck();
    // Don't touch _subscribed — the server subscription is still valid.
    // handleAudioFrame() will rebuild the context and decoder on the
    // next incoming frame.
    this.emit();
  }

  /** Permanently destroy the player. */
  destroy(): void {
    this._destroyed = true;
    this.stopHealthCheck();
    this.reset();
    this.teardownAudioContext();
    this.listeners.clear();
  }

  // -- Internal: rate servo -------------------------------------------------

  private resetSync(): void {
    this.samplesConsumed = 0;
    this.framesReceived = 0;
    this.currentRate = 1.0;
    this.smoothedRate = 1.0;
    this.currentBufferTarget = MIN_BUFFER_SAMPLES;
    this.resetDecoderState();
  }

  /**
   * Close and null the decoder, resetting all stall-detection counters.
   * The AudioContext and worklet are left intact — only the decode chain
   * is rebuilt.  This avoids the expensive teardown+async-reinit of the
   * full pipeline when only the decoder is broken.
   */
  private resetDecoder(): void {
    if (this.decoder && this.decoder.state !== "closed") {
      try {
        this.decoder.close();
      } catch {}
    }
    this.decoder = null;
    this.resetDecoderState();
  }

  /** Reset decoder-related counters without touching the decoder itself. */
  private resetDecoderState(): void {
    this.decodesRequested = 0;
    this.framesDecoded = 0;
    this.lastHealthDecodesRequested = 0;
    this.lastHealthFramesDecoded = 0;
    this.decoderSilentLastCheck = false;
    this.lastDecodedAt = 0;
  }

  /**
   * Called when the worklet reports its consumed-sample position.
   * Runs the buffer-depth servo: compares actual buffered depth against
   * the adaptive target and nudges the worklet's playback rate within
   * ±5 % to push the buffer back toward target.
   */
  private onWorkletPosition(consumed: number): void {
    const now = Date.now();
    this.samplesConsumed = consumed;
    this.lastWorkletReportAt = now;

    // Don't adjust during warmup — not enough samples to stabilise.
    if (this.framesReceived < SYNC_WARMUP_FRAMES) return;

    // Servo target: keep `buffered` at `bufferTarget`.
    //   buffered < target → drift > 0 → rate < 1 (slow down, refill)
    //   buffered > target → drift < 0 → rate > 1 (speed up, drain)
    // No A/V sync: video is played as it arrives and audio targets a
    // small buffer; both ride the same real-time network pacing, so
    // they stay aligned to within ppm-level clock skew which is
    // imperceptible over typical session lengths.
    const targetMs = this.currentBufferTarget / 48;
    const bufferedMs = this.lastBufferedSamples / 48;
    const drift = targetMs - bufferedMs;

    let rate = 1.0;
    const absDrift = Math.abs(drift);
    if (absDrift > DRIFT_DEADZONE_MS) {
      // Linear ramp from 0 to MAX_RATE_OFFSET over [DEADZONE, FULL_CORRECTION]
      const correction =
        Math.min(
          (absDrift - DRIFT_DEADZONE_MS) /
            (DRIFT_FULL_CORRECTION_MS - DRIFT_DEADZONE_MS),
          1.0,
        ) * MAX_RATE_OFFSET;
      rate = drift > 0 ? 1.0 - correction : 1.0 + correction;
    }

    // Exponential smoothing: avoids abrupt pitch changes from jittery
    // per-100 ms drift measurements.
    this.smoothedRate += RATE_SMOOTHING_ALPHA * (rate - this.smoothedRate);

    if (this.smoothedRate !== this.currentRate) {
      this.currentRate = this.smoothedRate;
      this.worklet?.port.postMessage({
        type: "rate",
        value: this.smoothedRate,
      });
    }
  }

  // -- Internal: stall detection / auto-recovery ----------------------------

  private startHealthCheck(): void {
    if (this.healthTimer || this._destroyed || this._muted || !this._subscribed)
      return;
    this.healthTimer = setInterval(() => this.checkHealth(), 2000);

    // When the tab returns from background, audio is often in a broken
    // state (context suspended, worklet stalled, decode chain dead).
    // Preemptively reset the pipeline so the user never has to do it
    // manually.  The reset is cheap — everything rebuilds on the next
    // incoming frame with only ~100-200 ms of imperceptible silence.
    // Quick tab switches (< 3 s) just get an immediate health check.
    if (!this.visibilityHandler && typeof document !== "undefined") {
      let hiddenAt = 0;
      this.visibilityHandler = () => {
        if (document.visibilityState === "hidden") {
          hiddenAt = Date.now();
        } else if (document.visibilityState === "visible") {
          if (this._destroyed || this._muted || !this._subscribed) return;
          const wasHiddenMs = hiddenAt > 0 ? Date.now() - hiddenAt : 0;
          hiddenAt = 0;
          if (wasHiddenMs > 3_000) {
            this.resetPipeline();
          } else {
            this.checkHealth();
          }
        }
      };
      document.addEventListener("visibilitychange", this.visibilityHandler);
    }
  }

  private stopHealthCheck(): void {
    if (this.healthTimer) {
      clearInterval(this.healthTimer);
      this.healthTimer = null;
    }
    if (this.visibilityHandler) {
      document.removeEventListener("visibilitychange", this.visibilityHandler);
      this.visibilityHandler = null;
    }
  }

  /**
   * Periodic health check (every 2 s): detects stalled or silently broken
   * audio and recovers by rebuilding the pipeline.
   *
   * Checks for four failure modes:
   * 1. **Worklet stall** — frames arrive from the server but the worklet
   *    hasn't reported a consumed-sample position in over 5 seconds.  The
   *    decode → worklet chain has silently broken.
   * 2. **Decoder stall** — frames are being sent to the decoder but no
   *    decoded output arrives for two consecutive checks (4 s).  The
   *    WebCodecs AudioDecoder has silently stopped producing output
   *    without transitioning to the "closed" state.  (Most decoder
   *    stalls are caught earlier by the inline check in handleAudioFrame.)
   * 3. **AudioContext death** — context is "closed" (resource pressure,
   *    device removal, GPU process crash).  The statechange listener
   *    handles this immediately, but this is a safety net.
   * 4. **Persistent suspension** — context is "suspended" and resume()
   *    fails for > 5 s.  Tear down and rebuild from scratch.
   *
   * Also resumes a suspended AudioContext (can happen after device
   * changes or resource pressure without transitioning to "closed").
   */
  private checkHealth(): void {
    if (this._destroyed || this._muted || !this._subscribed) {
      this.stopHealthCheck();
      return;
    }

    // Skip checks when the tab is backgrounded — the browser throttles
    // both the worklet and the timer, creating false stalls.
    if (
      typeof document !== "undefined" &&
      document.visibilityState === "hidden"
    ) {
      return;
    }

    const now = Date.now();

    // Check if the auto-reset rate limit allows a reset right now.
    const canAutoReset = now - this.lastAutoResetAt > 10_000;

    // Resume a suspended AudioContext (device change, resource pressure).
    // If it stays suspended despite repeated resume() attempts, tear it
    // down and rebuild — the context may be permanently stuck.
    if (this.ctx && this.ctx.state === "suspended") {
      if (this.suspendedSince === 0) this.suspendedSince = now;
      this.ctx.resume().catch(() => {});
      if (now - this.suspendedSince > 5_000 && canAutoReset) {
        this.suspendedSince = 0;
        this.lastAutoResetAt = now;
        this.resetPipeline();
        return;
      }
    } else {
      this.suspendedSince = 0;
    }

    // Safety net: catch a closed AudioContext even if the statechange
    // listener didn't fire (race during init, listener removed, etc.).
    if (this.ctx && this.ctx.state === "closed") {
      this.teardownAudioContext();
      // Will rebuild on next handleAudioFrame().
    }

    // 1. Worklet stall: frames arriving but worklet silent for > 5 s.
    //    Also catches the case where the worklet was created and fed
    //    decoded audio but never produced a position report (e.g.
    //    processorerror before the first report, or stuck buffering).
    const workletSilent =
      this.lastWorkletReportAt > 0
        ? now - this.lastWorkletReportAt > 5_000
        : this.worklet != null && this.framesDecoded > 0;
    if (
      this.lastFrameAt > 0 &&
      now - this.lastFrameAt < 5000 &&
      workletSilent
    ) {
      if (canAutoReset) {
        this.lastAutoResetAt = now;
        this.resetPipeline();
        return;
      }
    }

    // 2. Decoder stall: decoder received frames but produced no output.
    //    Compare snapshots from the last health check.  A single silent
    //    interval (2 s) triggers a reset — Opus frames decode nearly
    //    instantly, so any gap this long is a real failure.
    const decodesGrew = this.decodesRequested > this.lastHealthDecodesRequested;
    const decodesProduced = this.framesDecoded > this.lastHealthFramesDecoded;
    const wasSilent = this.decoderSilentLastCheck;
    this.lastHealthDecodesRequested = this.decodesRequested;
    this.lastHealthFramesDecoded = this.framesDecoded;
    this.decoderSilentLastCheck = decodesGrew && !decodesProduced;

    if (wasSilent && decodesGrew && !decodesProduced && canAutoReset) {
      this.decoderSilentLastCheck = false;
      this.lastAutoResetAt = now;
      this.resetDecoder();
      return;
    }
  }

  // -- Internal: audio context + decoder -----------------------------------

  /**
   * Tear down the AudioContext and worklet without touching the decoder or
   * sync state.  Used when the context has died (state === "closed") and
   * needs to be rebuilt.
   */
  private teardownAudioContext(): void {
    if (this.worklet) {
      try {
        this.worklet.disconnect();
      } catch {}
      this.worklet = null;
    }
    if (this.ctx) {
      // If the context isn't already closed, close it.
      if (this.ctx.state !== "closed") {
        this.ctx.close().catch(() => {});
      }
      this.ctx = null;
    }
    this.gain = null;
    this.suspendedSince = 0;
    this.resetSync();
  }

  private async initAudioContext(): Promise<void> {
    if (this._destroyed || this.initializingContext) return;
    this.initializingContext = true;
    try {
      this.ctx = new AudioContext({ sampleRate: 48000 });
      this.gain = this.ctx.createGain();
      this.gain.gain.value = this._muted ? 0 : 1;
      this.gain.connect(this.ctx.destination);

      // Detect AudioContext state transitions during playback.  The browser
      // can suspend or close the context at any time (audio device removal,
      // resource pressure, GPU process crash, etc.).  Handling this via an
      // event listener gives us immediate recovery instead of waiting for
      // the 5-second health-check poll.
      this.ctx.addEventListener("statechange", () => {
        const ctx = this.ctx;
        if (!ctx || this._destroyed) return;
        if (ctx.state === "closed") {
          // Context died — tear down so handleAudioFrame() rebuilds.
          this.teardownAudioContext();
        } else if (ctx.state === "suspended" && !this._muted) {
          ctx.resume().catch(() => {});
        }
      });

      // Detect audio output device changes (headphones plugged/unplugged,
      // Bluetooth connect/disconnect, default device change, etc.).  The
      // AudioContext re-routes automatically, but in practice the worklet ↔
      // destination chain can break silently during the transition.  Rebuild
      // the entire pipeline to get a clean audio graph on the new device.
      // Rate-limited to avoid reset loops if the device is flapping.
      this.ctx.addEventListener("sinkchange", () => {
        if (this._destroyed || this._muted) return;
        const now = Date.now();
        if (now - this.lastAutoResetAt > 10_000) {
          this.lastAutoResetAt = now;
          this.resetPipeline();
        }
      });

      // Register the worklet processor from an inline Blob URL.
      const blob = new Blob([WORKLET_SRC], { type: "application/javascript" });
      const url = URL.createObjectURL(blob);
      try {
        await this.ctx.audioWorklet.addModule(url);
      } finally {
        URL.revokeObjectURL(url);
      }

      // If we were destroyed or the context was torn down while awaiting
      // the module load (e.g. resetPipeline fired during the await), bail.
      if (this._destroyed || !this.ctx || this.ctx.state === "closed") {
        if (this.ctx && this.ctx.state !== "closed") {
          this.ctx.close().catch(() => {});
        }
        this.ctx = null;
        return;
      }

      this.worklet = new AudioWorkletNode(this.ctx, "blit-audio", {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [2],
      });
      this.worklet.connect(this.gain);

      // Detect worklet processor crashes.  When process() throws, the
      // worklet fires processorerror and stops processing audio
      // permanently.  Reset the pipeline immediately.
      this.worklet.addEventListener("processorerror", () => {
        if (!this._destroyed) this.resetPipeline();
      });

      // Listen for position reports and buffer events from the worklet.
      this.worklet.port.onmessage = (e: MessageEvent) => {
        const d = e.data;
        if (!d) return;
        if (d.type === "pos") {
          if (typeof d.target === "number") {
            this.currentBufferTarget = d.target;
          }
          if (typeof d.buffered === "number") {
            this.lastBufferedSamples = d.buffered;
          }
          this.onWorkletPosition(d.value);
        } else if (d.type === "event") {
          if (typeof d.target === "number") {
            this.currentBufferTarget = d.target;
          }
        }
      };

      // Flush any frames that arrived before the worklet was ready.
      for (const pcm of this.buffer) {
        this.worklet.port.postMessage(pcm, [pcm.buffer]);
      }
      this.buffer = [];
    } catch {
      // Close the AudioContext if it was created — otherwise it leaks.
      // Browsers limit the number of live AudioContexts (typically 4–6);
      // leaking them on repeated init failures eventually exhausts the
      // quota and no new context can be created.
      if (this.ctx && this.ctx.state !== "closed") {
        this.ctx.close().catch(() => {});
      }
      this.ctx = null;
    } finally {
      this.initializingContext = false;
    }
  }

  private initDecoder(): void {
    if (this._destroyed) return;
    if (!AudioPlayer.supported) return;
    try {
      this.decoder = new AudioDecoder({
        output: (frame: AudioData) => {
          this.onDecodedFrame(frame);
        },
        error: () => {
          // The decoder has entered the "closed" state.  Null it out so
          // the next handleAudioFrame call recreates it immediately.
          this.decoder = null;
        },
      });
      this.decoder.configure({
        codec: "opus",
        sampleRate: 48000,
        numberOfChannels: 2,
      });
    } catch {
      this.decoder = null;
    }
  }

  private onDecodedFrame(frame: AudioData): void {
    this.framesDecoded++;
    this.lastDecodedAt = Date.now();
    // Extract f32-planar samples: [L...L, R...R].
    const n = frame.numberOfFrames;
    const pcm = new Float32Array(n * 2);
    try {
      // Copy each plane into its half of the buffer.
      const left = new Float32Array(n);
      const right = new Float32Array(n);
      frame.copyTo(left, { planeIndex: 0, format: "f32-planar" });
      frame.copyTo(right, { planeIndex: 1, format: "f32-planar" });
      pcm.set(left, 0);
      pcm.set(right, n);
    } catch {
      try {
        // Fallback: single-plane copy (mono or interleaved).
        frame.copyTo(pcm, { planeIndex: 0 });
      } catch {
        frame.close();
        return;
      }
    }

    frame.close();

    if (this.worklet) {
      // Transfer the buffer to the audio thread (zero-copy).
      this.worklet.port.postMessage(pcm, [pcm.buffer]);
    } else if (this.buffer.length < MAX_BUFFER_FRAMES) {
      // Worklet not ready yet — queue locally.
      this.buffer.push(pcm);
    }
  }
}
