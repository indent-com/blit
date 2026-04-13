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
const MAX_BUFFER_FRAMES = 10; // 200 ms

/**
 * Target jitter buffer depth in samples at 48 kHz.  The worklet
 * accumulates this much audio before starting playback and re-buffers
 * after an underrun.  Absorbs network jitter and main-thread stalls
 * without adding perceptible latency.
 */
const JITTER_BUFFER_SAMPLES = 2400; // 50 ms at 48 kHz

// -- A/V sync constants ----------------------------------------------------

/** How often the worklet reports its consumed-sample position (in samples). */
const POS_REPORT_INTERVAL = 4800; // ~100 ms at 48 kHz

/**
 * Drift dead-zone: don't adjust rate if |drift| is below this (ms).
 * Avoids oscillation when sync is already good.
 */
const DRIFT_DEADZONE_MS = 10;

/**
 * Drift threshold for maximum correction (ms).  Beyond this we apply the
 * full ±MAX_RATE_OFFSET.  Between DEADZONE and this, we interpolate.
 */
const DRIFT_FULL_CORRECTION_MS = 150;

/** Maximum rate offset from 1.0 in either direction. */
const MAX_RATE_OFFSET = 0.02; // ±2%

/**
 * Hard ceiling on worklet buffer depth (samples at 48 kHz).  If the buffer
 * exceeds this, the worklet drops the oldest chunks until it is back at the
 * jitter-buffer target.  This caps the maximum latency that can accumulate
 * from network bursts, tab backgrounding, or decode stalls.
 */
const MAX_BUFFERED_SAMPLES = 7200; // 150 ms at 48 kHz

/**
 * Drift threshold for a hard jump (ms).  When audio is *ahead* of video by
 * more than this, the worklet skips forward (drops old samples) to close the
 * gap without a full flush.  When audio is *behind* by more than this, we
 * just reset sync and let rate steering or new frames catch up — flushing
 * would discard the only audio we have, making the gap worse.
 */
const DRIFT_JUMP_MS = 200;

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
    this.bufferTarget = ${JITTER_BUFFER_SAMPLES}; // samples to accumulate before playing
    this.underruns = 0;     // consecutive underruns, drives adaptive buffer growth

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
        this.bufferTarget = ${JITTER_BUFFER_SAMPLES};
      } else if (e.data && e.data.type === "skip") {
        // Drop samples from the front to reduce drift without a full
        // flush.  Keeps playback running (no re-buffering silence).
        let toSkip = e.data.samples | 0;
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
      } else if (e.data && e.data.type === "rate") {
        this.rate = e.data.value;
      } else {
        this.buffer.push(e.data);
        this.buffered += e.data.length / 2; // half = per-channel sample count

        // Hard buffer cap: if too much audio has accumulated (network burst,
        // tab was backgrounded, decode stall, etc.), drop the oldest chunks
        // to get back near the jitter-buffer target.  A brief discontinuity
        // is far less jarring than ever-growing latency.
        if (this.buffered > ${MAX_BUFFERED_SAMPLES}) {
          while (this.buffer.length > 1 && this.buffered > this.bufferTarget) {
            const dropped = this.buffer.shift();
            const droppedSamples = dropped.length / 2;
            this.buffered -= droppedSamples;
            this.consumed += droppedSamples;
            this.offset = 0;
          }
          this.frac = 0;
        }
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
    if (this.buffering) {
      if (this.buffered >= this.bufferTarget) {
        this.buffering = false;
      } else {
        // Output silence while buffering.
        for (let i = 0; i < needed; i++) {
          outL[i] = 0;
          outR[i] = 0;
        }
        return true;
      }
    }

    while (written < needed && this.buffer.length > 0) {
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

    // Underrun: output silence and re-enter buffering mode so we
    // accumulate a full jitter buffer before resuming playback.
    if (written < needed) {
      for (let i = written; i < needed; i++) {
        outL[i] = 0;
        outR[i] = 0;
      }
      if (this.consumed > 0) {
        // Only re-buffer after we've started playing (not on initial silence).
        this.underruns++;
        // Adaptive buffer: after repeated underruns, require more audio
        // before resuming to avoid stutter loops.  Grows up to 150 ms
        // (capped at MAX_BUFFERED_SAMPLES) then decays on clean playback.
        this.bufferTarget = Math.min(
          ${JITTER_BUFFER_SAMPLES} + this.underruns * 960, // +20 ms per consecutive underrun
          ${MAX_BUFFERED_SAMPLES}
        );
        this.buffering = true;
      }
    } else if (this.underruns > 0) {
      // Successful render with no underrun — slowly decay the underrun
      // counter so the buffer target returns to baseline.
      this.underruns = Math.max(0, this.underruns - 0.002);
    }

    // Report position periodically.  Include this.offset for accuracy
    // (consumed only counts fully-drained chunks).
    const totalPos = this.consumed + this.offset;
    if (totalPos - this.lastReport >= ${POS_REPORT_INTERVAL}) {
      this.lastReport = totalPos;
      this.port.postMessage({ type: "pos", value: totalPos });
    }

    // Keep processor alive even during silence.
    return true;
  }
}
registerProcessor("blit-audio", BlitAudioProcessor);
`;

// -- Timeline entry for mapping samples → server timestamps ----------------

interface TimelineEntry {
  /** Cumulative source sample offset at the start of this audio frame. */
  sampleOffset: number;
  /** Server timestamp (ms) of this audio frame. */
  serverMs: number;
}

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

  // -- A/V sync state ------------------------------------------------------

  /** Timeline: maps cumulative sample offsets to server timestamps. */
  private timeline: TimelineEntry[] = [];
  /** Cumulative source samples queued to the worklet. */
  private samplesQueued = 0;
  /** Last consumed-sample position reported by the worklet. */
  private samplesConsumed = 0;
  /** Number of audio frames received (for warmup). */
  private framesReceived = 0;
  /** Latest video frame server timestamp (ms), set externally. */
  private lastVideoTimestampMs = -1;
  /** Current playback rate sent to the worklet. */
  private currentRate = 1.0;
  /** Smoothed rate — exponentially filtered to avoid wow/flutter. */
  private smoothedRate = 1.0;

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

  /**
   * Notify the audio player of the latest video frame's server timestamp.
   * Called from the connection layer whenever a surface frame arrives.
   * Video is never delayed — this is used only to steer audio rate.
   */
  notifyVideoTimestamp(serverMs: number): void {
    this.lastVideoTimestampMs = serverMs;
  }

  /** Handle an incoming S2C_AUDIO_FRAME. */
  handleAudioFrame(timestamp: number, _flags: number, data: Uint8Array): void {
    if (this._destroyed || this._muted) return;

    const now = Date.now();
    this.lastFrameAt = now;
    this.startHealthCheck();

    // Inline decoder stall check: if we've been feeding the decoder for
    // > 2 s but it hasn't produced any output, the decode pipeline is
    // dead.  Catches failures within one frame interval (~20 ms) rather
    // than waiting for the next health-check tick.
    if (
      this.lastDecodedAt > 0 &&
      this.decodesRequested > 0 &&
      now - this.lastDecodedAt > 2_000
    ) {
      if (now - this.lastAutoResetAt > 10_000) {
        this.lastAutoResetAt = now;
        this.resetPipeline();
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
      // Decoder threw — close and null it immediately so the next
      // handleAudioFrame creates a fresh one instead of repeatedly
      // calling decode() on a broken decoder until the async error
      // callback fires.
      try {
        this.decoder.close();
      } catch {}
      this.decoder = null;
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

  // -- Internal: sync ------------------------------------------------------

  private resetSync(): void {
    this.timeline = [];
    this.samplesQueued = 0;
    this.samplesConsumed = 0;
    this.framesReceived = 0;
    this.lastVideoTimestampMs = -1;
    this.currentRate = 1.0;
    this.smoothedRate = 1.0;
    this.decodesRequested = 0;
    this.framesDecoded = 0;
    this.lastHealthDecodesRequested = 0;
    this.lastHealthFramesDecoded = 0;
    this.decoderSilentLastCheck = false;
    this.lastDecodedAt = 0;
  }

  /**
   * Given the worklet's consumed sample count, estimate the server
   * timestamp (ms) of the audio currently being played back.
   */
  private audioTimestampAtSample(consumed: number): number | null {
    const tl = this.timeline;
    if (tl.length === 0) return null;

    // Binary search for the last entry where sampleOffset <= consumed.
    let lo = 0;
    let hi = tl.length - 1;
    if (consumed < tl[0].sampleOffset) return tl[0].serverMs;
    if (consumed >= tl[hi].sampleOffset) return tl[hi].serverMs;

    while (lo < hi - 1) {
      const mid = (lo + hi) >> 1;
      if (tl[mid].sampleOffset <= consumed) lo = mid;
      else hi = mid;
    }

    // Linearly interpolate between lo and hi.
    const a = tl[lo];
    const b = tl[hi];
    const sampleSpan = b.sampleOffset - a.sampleOffset;
    if (sampleSpan <= 0) return a.serverMs;
    const t = (consumed - a.sampleOffset) / sampleSpan;
    return a.serverMs + t * (b.serverMs - a.serverMs);
  }

  /**
   * Called when the worklet reports its consumed-sample position.
   * Computes drift and adjusts playback rate.
   */
  private onWorkletPosition(consumed: number): void {
    this.samplesConsumed = consumed;
    this.lastWorkletReportAt = Date.now();

    // Don't adjust during warmup — not enough data to estimate drift.
    if (this.framesReceived < SYNC_WARMUP_FRAMES) return;
    if (this.lastVideoTimestampMs < 0) return;

    const audioMs = this.audioTimestampAtSample(consumed);
    if (audioMs === null) return;

    // drift > 0 → audio is ahead of video → slow down (rate < 1)
    // drift < 0 → audio is behind video → speed up (rate > 1)
    const drift = audioMs - this.lastVideoTimestampMs;

    // Hard jump: drift is too large for ±2% rate steering to close
    // quickly.  Handle ahead and behind differently.
    if (drift > DRIFT_JUMP_MS) {
      // Audio ahead of video — too much audio buffered.  Skip forward
      // in the worklet buffer to align, then reset sync to re-measure
      // from the new position.  Avoids the flush→silence→rebuffer
      // cycle that makes recovery so fragile.
      const skipSamples = ((drift - DRIFT_DEADZONE_MS) * 48) | 0;
      if (skipSamples > 0) {
        this.worklet?.port.postMessage({ type: "skip", samples: skipSamples });
      }
      this.resetSync();
      return;
    }
    // Audio behind video (drift < -DRIFT_JUMP_MS): frames were dropped
    // and audio is lagging.  Don't flush — that discards the only audio
    // we have.  Fall through to normal rate steering which applies the
    // full +2% correction.  At 2%, a 200 ms gap closes in ~10 s; not
    // instantaneous, but audio keeps playing instead of going silent.

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
      // Audio ahead → consume source slower (rate < 1) to let video catch up.
      // Audio behind → consume source faster (rate > 1) to catch up to video.
      rate = drift > 0 ? 1.0 - correction : 1.0 + correction;
    }

    // Exponential smoothing: blend toward target rate to avoid abrupt
    // pitch changes (wow/flutter) from jittery drift measurements.
    this.smoothedRate += RATE_SMOOTHING_ALPHA * (rate - this.smoothedRate);

    if (this.smoothedRate !== this.currentRate) {
      this.currentRate = this.smoothedRate;
      this.worklet?.port.postMessage({
        type: "rate",
        value: this.smoothedRate,
      });
    }

    // Prune old timeline entries (keep at most 100 behind consumed position).
    while (
      this.timeline.length > 2 &&
      this.timeline[1].sampleOffset < consumed
    ) {
      this.timeline.shift();
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
   *    hasn't reported a consumed-sample position in over 2 seconds.  The
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
   *    fails for > 2 s.  Tear down and rebuild from scratch.
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
      if (now - this.suspendedSince > 2_000 && canAutoReset) {
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

    // 1. Worklet stall: frames arriving but worklet silent for > 2 s.
    //    Also catches the case where the worklet was created and fed
    //    decoded audio but never produced a position report (e.g.
    //    processorerror before the first report, or stuck buffering).
    const workletSilent =
      this.lastWorkletReportAt > 0
        ? now - this.lastWorkletReportAt > 2_000
        : this.worklet != null && this.framesDecoded > 0;
    if (
      this.lastFrameAt > 0 &&
      now - this.lastFrameAt < 3000 &&
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
      this.resetPipeline();
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

      // If we were destroyed or the context was replaced while awaiting
      // the module load, bail out.
      if (this._destroyed || this.ctx.state === "closed") {
        this.ctx.close().catch(() => {});
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

      // Listen for position reports from the worklet.
      this.worklet.port.onmessage = (e: MessageEvent) => {
        if (e.data && e.data.type === "pos") {
          this.onWorkletPosition(e.data.value);
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

    // Record timeline entry: the server timestamp for this chunk of samples.
    // The frame's timestamp (µs) was set from serverMs * 1000 in handleAudioFrame.
    const serverMs = frame.timestamp / 1000;
    this.timeline.push({
      sampleOffset: this.samplesQueued,
      serverMs,
    });
    this.samplesQueued += n;

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
