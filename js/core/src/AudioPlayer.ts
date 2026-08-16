/**
 * Audio playback pipeline with A/V sync: receives Opus frames from the
 * server, decodes via WebCodecs AudioDecoder, and plays through an
 * AudioContext with rate-adjusted resampling to stay in sync with video.
 *
 * Decode runs in a dedicated Worker that also owns the worklet's
 * MessagePort (transferred), so decoded PCM reaches the audio thread
 * without a main-thread hop — heavy video work (decode callbacks,
 * full-screen draws, multi-megabyte WebSocket frames) can no longer
 * starve the jitter buffer.  The main thread only relays the ~50 tiny
 * encoded frames per second and keeps the AudioContext lifecycle, rate
 * servo, and health checks.  Falls back to inline (main-thread) decode
 * when Workers or in-worker WebCodecs are unavailable.
 *
 * Audio and video frames share a common server-side wall-clock timestamp
 * (milliseconds since compositor creation).  The worklet performs linear-
 * interpolation resampling at a variable rate (±MAX_RATE_OFFSET) so audio
 * can speed up or slow down to track video.  Video is never delayed.
 *
 * Playback uses an AudioWorkletNode with an inline processor registered
 * from a Blob URL — no external file needed.
 */

import { claimPlaybackAudioSession } from "./audioSession";

/**
 * Maximum pre-worklet staging depth in decoded frames (~20 ms each).
 *
 * Audio decoded while AudioWorklet.addModule() is still loading must be
 * staged somewhere, but keeping half a second here defeats the worklet's
 * lower latency bound before its servo even starts.  Keep only the newest
 * 400 ms (20 whole Opus frames), enough to fill the adaptive-buffer
 * ceiling below.
 */
export const MAX_STAGING_FRAMES = 20; // 400 ms

/**
 * Adaptive jitter buffer: the worklet starts at MIN_BUFFER_SAMPLES, grows
 * on the leading edge of each underrun event, and shrinks back one frame
 * at a time after DECAY_STABLE_SAMPLES of underrun-free playback.
 * Hysteresis is provided by the MIN floor: once bufferTarget hits it,
 * shrinking stops.  Floor is three frames (60 ms) to absorb two
 * back-to-back late arrivals before the buffer empties; stable
 * connections steady-state at 60 ms while jittery ones self-size to
 * whatever headroom they need, up to MAX_BUFFER_TARGET_SAMPLES.
 */
export const MIN_BUFFER_SAMPLES = 2880; // 3 frames = 60 ms at 48 kHz

/**
 * Hard ceiling on the adaptive jitter buffer.
 *
 * Growth is per-underrun and decay is per-several-seconds-of-calm, so
 * without a ceiling the target ratchets: an underrun buys 100 ms of
 * latency back in one event and gives it up over tens of seconds.  A
 * client that underruns faster than it decays (Safari on iPadOS missing
 * render-quantum deadlines, or a server driving several concurrent video
 * streams) walks the target into the seconds, and the servo then *holds*
 * it there — `drift` is measured against the target, so a large target
 * is defended by slowing playback down to refill it.  That is the
 * "audio falls further and further behind" failure.
 *
 * 400 ms leaves 340 ms of adaptive headroom above the 60 ms floor.  That
 * covers browser scheduling stalls and transport batching that can exceed
 * 250 ms even when protocol RTT and server-side queues look healthy.  The
 * servo must preserve that adaptive target; forcing every client back to the
 * floor causes repeated gaps.  The ceiling still prevents a stressed client
 * from ratcheting into seconds of latency.
 */
export const MAX_BUFFER_TARGET_SAMPLES = 19200; // 400 ms at 48 kHz

/**
 * Samples of uninterrupted, non-buffering playback required before
 * bufferTarget shrinks by one frame.  Short enough that a link which has
 * actually recovered gets its latency back in tens of seconds rather than
 * minutes.  It was also meant to be long enough that recurring jitter never
 * decayed the buffer back toward the floor *between* events, which held only
 * for jitter more often than every ~25 s; anything rarer than that unwound
 * completely and glitched again.  Keeping headroom across quiet gaps is now
 * FLOOR_DECAY_STABLE_SAMPLES' job, and this constant is free to stay brisk.
 * At 5 s per 20 ms frame,
 * one underrun's worth of growth (100 ms) unwinds in 25 s of calm, and
 * any link underrunning more often than every 5 s still ratchets up to
 * the ceiling and stays there — which is the correct answer for a link
 * that bad.  Shrinking stays slow by design; growth reacts within one
 * event.
 */
const DECAY_STABLE_SAMPLES = 240000; // 5 s at 48 kHz

/**
 * How long the buffer remembers what a link turned out to need.
 *
 * Growth is per-underrun and decay is per-DECAY_STABLE_SAMPLES, so on its own
 * the target is back at MIN within half a minute of calm and meets the next
 * jitter spike with no headroom — a link that glitches every few minutes
 * glitches every few minutes forever, re-learning the same lesson each time.
 * The learned floor is the memory the fast decay lacks: an underrun raises it
 * to whatever was needed, the target may not shrink below it, and it fades on
 * its own much slower timescale.
 *
 * That leaves two rates doing separate jobs. The target still falls quickly,
 * so a burst of jitter does not cost latency for long. The floor falls slowly,
 * so recurring jitter does not cost audio at all.
 */
const FLOOR_DECAY_STABLE_SAMPLES = 1_440_000; // 30 s at 48 kHz

/**
 * Ceiling on the *learned* floor, well under MAX_BUFFER_TARGET_SAMPLES.
 *
 * The target may still spike to the maximum to ride out something awful; the
 * floor is what a link is held at afterwards, and a bad minute should not pin
 * playback 400 ms behind live for the rest of the session.
 */
export const MAX_LEARNED_FLOOR_SAMPLES = 9600; // 200 ms at 48 kHz

// -- A/V sync constants ----------------------------------------------------

/** How often the worklet reports its consumed-sample position (in samples). */
const POS_REPORT_INTERVAL = 4800; // ~100 ms at 48 kHz

/*
 * The steady-state target is the worklet's current adaptive bufferTarget,
 * reported alongside each position.  Treating it as the equilibrium keeps
 * the safety margin learned from real underruns; MAX_BUFFER_TARGET_SAMPLES
 * bounds the extra latency instead of discarding the margin after every
 * rebuffer.
 */

/**
 * Drift dead-zone: don't adjust rate if |drift| is below this (ms).
 * Drift is measured relative to the adaptive target, so zero means "buffer
 * at the learned safe depth".  Avoids oscillation when sync is good.
 */
const DRIFT_DEADZONE_MS = 10;

/**
 * Drift threshold for maximum correction (ms).  Beyond this we apply the
 * full ±MAX_RATE_OFFSET.  Between DEADZONE and this, we interpolate.
 */
const DRIFT_FULL_CORRECTION_MS = 300;

/** Maximum rate offset from 1.0 in either direction. */
const MAX_RATE_OFFSET = 0.02; // ±2%

/** Minimum number of audio frames received before we start sync adjustment. */
export const SYNC_WARMUP_FRAMES = 10;

/**
 * Exponential smoothing factor for rate changes.  Each update blends
 * α·target + (1−α)·previous.  Lower values smooth more aggressively
 * at the cost of slower convergence.  0.15 converges within ~1 s while
 * eliminating the wow-and-flutter artifacts from jittery drift readings.
 */
const RATE_SMOOTHING_ALPHA = 0.15;

/**
 * Continuous starvation before the worklet re-enters full buffering mode.
 *
 * Rebuffering is not a small correction: playback stops until the buffer has
 * refilled to `bufferTarget`, so it converts a gap into a silence of at least
 * MIN_BUFFER_SAMPLES and, on a link that has learned it needs headroom, up to
 * MAX_LEARNED_FLOOR_SAMPLES. It is only worth that when continuing would
 * produce a stutter train instead of one dip.
 *
 * This used to be three render blocks — 8 ms — which is a scheduling hiccup,
 * not a broken stream. Measured on a real connection, 33 of 37 underruns
 * escalated to a rebuffer, so almost every brief gap was answered with up to
 * 200 ms of deliberate silence. That silence *is* the audible pause; the gap
 * that triggered it would have been inaudible under the fade envelope.
 *
 * At 60 ms the short gaps ride through as their own fade-masked dip, which is
 * strictly less silence than stopping to refill, and a genuinely broken stream
 * still gets the full treatment.
 */
const UNDERRUN_REBUFFER_MS = 60;
const RENDER_QUANTUM_SAMPLES = 128;
export const UNDERRUN_REBUFFER_THRESHOLD = Math.ceil(
  (UNDERRUN_REBUFFER_MS * 48) / RENDER_QUANTUM_SAMPLES,
);

/** Samples per 20 ms Opus frame at 48 kHz (per-channel). */
export const SAMPLES_PER_20_MS = 960;

/**
 * How many 20 ms frames to grow bufferTarget by on each underrun event.
 * Transport head-of-line blocking (audio serialized behind video bulk
 * writes on the same TCP stream) produces arrival gaps proportional to
 * the video bulk-write time — typically 100–200 ms on keyframes.  Growing
 * by a single frame makes convergence take dozens of audible underruns;
 * 5 frames (100 ms per event) reaches a buffer depth that absorbs those
 * bursts within a handful of events.  Decay (DECAY_STABLE_SAMPLES of
 * clean playback per frame shrunk) claws back any overshoot, and
 * MAX_BUFFER_TARGET_SAMPLES bounds how far it can run.
 */
export const GROW_FRAMES_PER_UNDERRUN = 5;

/**
 * Excess buffered depth over the current adaptive target that triggers a
 * hard `skip` instead of waiting for the rate servo to drain it.
 *
 * The servo can only drain at MAX_RATE_OFFSET, so reclaiming a second of
 * accumulated latency by rate alone takes about a minute — and latency
 * arrives in bursts (a backlogged server queue flushing, a catch-up
 * replay on resubscribe, the tab being unthrottled after a stall) far
 * faster than that.  Above this threshold we drop samples outright:
 * a single ~1.3 ms fade (see FADE_SAMPLES) beats staying seconds behind.
 *
 * Keep this above the 100 ms position-report cadence and ordinary transport
 * batching.  A smaller threshold can turn a stale depth report into a skip
 * that consumes the current safety margin and induces an underrun.
 */
export const SKIP_EXCESS_MS = 200;

/**
 * Minimum interval between `skip` messages.
 *
 * The worklet's buffered depth reaches us via ~100 ms position reports,
 * so the report following a skip can still describe the pre-skip depth.
 * Without a cooldown that stale reading triggers a second skip and we
 * discard twice what we meant to.  One second is far longer than the
 * port round-trip and still bounds a genuinely runaway buffer quickly.
 */
export const SKIP_COOLDOWN_MS = 1000;

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
export const WORKLET_SRC = /* js */ `
class BlitAudioProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.buffer = [];       // queue of Float32Array PCM chunks [L..L, R..R]
    this.offset = 0;        // integer sample offset into current chunk
    this.frac = 0;          // fractional sample position [0, 1)
    this.rate = 1.0;        // playback rate (1.0 = normal)
    this.consumed = 0;      // total source samples from fully-consumed chunks
    this.lastReport = 0;    // consumed+offset at last report
    this.buffered = 0;      // queued samples, counting the head chunk whole
    this.buffering = true;  // true while accumulating the jitter buffer
    this.bufferTarget = ${MIN_BUFFER_SAMPLES}; // adaptive: grows on underrun, shrinks on stability
    this.stableSamples = 0; // consumed samples of underrun-free playback (drives shrinking)
    this.learnedFloor = ${MIN_BUFFER_SAMPLES}; // slow-decaying memory of the headroom this link needed
    this.floorStableSamples = 0; // drives the floor's own, much slower decay
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
        this.learnedFloor = ${MIN_BUFFER_SAMPLES};
        this.floorStableSamples = 0;
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
            // Advancing the read head is all that is needed: depth() nets
            // offset out of buffered, so decrementing buffered here too
            // would drop twice what was asked for.
            this.offset += toSkip;
            toSkip = 0;
          }
        }
        this.frac = 0;
        // Jumping the read position is a waveform discontinuity just like
        // an underrun boundary.  Drop the envelope so the existing fade
        // ramps back up over FADE_SAMPLES and the splice stays inaudible.
        this.fadeGain = 0;
        this.port.postMessage({
          type: "event",
          kind: "skip",
          requested,
          skipped: requested - toSkip,
          buffered: this.depth(),
        });
      } else if (e.data && e.data.type === "rate") {
        this.rate = e.data.value;
      } else {
        this.buffer.push(e.data);
        this.buffered += e.data.length / 2; // half = per-channel sample count
        // No cap enforced here: dropping on arrival would discard the
        // newest audio and keep the stalest.  The main thread watches
        // the buffered depth in the position reports and posts "skip"
        // when it runs past SKIP_EXCESS_MS over target, which drops
        // from the front instead.
      }
    };
  }

  // Playable samples still ahead of the read head.  \`buffered\` counts the
  // head chunk in full regardless of how far into it we have played, so
  // it overstates true depth by up to one chunk (20 ms).  Everything that
  // makes a latency decision — the buffering gate here and every depth
  // reported to the main thread — wants the exact figure.
  depth() {
    return this.buffered - this.offset;
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
    if (this.buffering && this.depth() >= this.bufferTarget) {
      this.buffering = false;
      this.port.postMessage({
        type: "event",
        kind: "rebuffer_end",
        target: this.bufferTarget,
        buffered: this.depth(),
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
          this.bufferTarget = Math.min(
            this.bufferTarget + ${SAMPLES_PER_20_MS * GROW_FRAMES_PER_UNDERRUN},
            ${MAX_BUFFER_TARGET_SAMPLES}
          );
          this.port.postMessage({
            type: "event",
            kind: "grow",
            target: this.bufferTarget,
            buffered: this.depth(),
          });
        }
        if (this.underruns === 1) {
          // Remember what this link needed. The fast decay below may not go
          // under this, so the next spike is met with the headroom the last
          // one proved necessary.
          this.learnedFloor = Math.min(
            Math.max(this.learnedFloor, this.bufferTarget),
            ${MAX_LEARNED_FLOOR_SAMPLES}
          );
          this.floorStableSamples = 0;
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
        // The learned floor fades on its own, much slower clock, so a link
        // that has genuinely settled does get its latency back — it just
        // takes minutes of quiet rather than seconds.
        this.floorStableSamples += needed;
        if (
          this.floorStableSamples >= ${FLOOR_DECAY_STABLE_SAMPLES} &&
          this.learnedFloor > ${MIN_BUFFER_SAMPLES}
        ) {
          this.learnedFloor = Math.max(
            this.learnedFloor - ${SAMPLES_PER_20_MS},
            ${MIN_BUFFER_SAMPLES}
          );
          this.floorStableSamples = 0;
        }
        if (
          this.stableSamples >= ${DECAY_STABLE_SAMPLES} &&
          this.bufferTarget > this.learnedFloor
        ) {
          this.bufferTarget = Math.max(
            this.bufferTarget - ${SAMPLES_PER_20_MS},
            this.learnedFloor
          );
          this.stableSamples = 0;
          this.port.postMessage({
            type: "event",
            kind: "shrink",
            target: this.bufferTarget,
            floor: this.learnedFloor,
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
        buffered: this.depth(),
      });
    }

    // Keep processor alive even during silence.
    return true;
  }
}
registerProcessor("blit-audio", BlitAudioProcessor);
`;

/**
 * Inline dedicated-Worker source: owns the WebCodecs AudioDecoder AND the
 * worklet's MessagePort (transferred in), so decoded PCM flows
 * worker → audio thread without a main-thread hop.  This is what keeps
 * audio smooth while the main thread is saturated by video work
 * (multi-megabyte WebSocket frames, VideoDecoder output callbacks,
 * full-screen drawImage): with the decoder on the main thread, every
 * decoded frame had to wait for a main-thread task slot before reaching
 * the worklet, and stalls longer than the jitter buffer produced audible
 * underruns.
 *
 * Messages IN (from AudioPlayer):
 *   { type: "port", port }      — worklet MessagePort to feed (transferred);
 *                                 pending PCM is flushed into it
 *   { type: "detach" }          — worklet torn down: drop port + pending PCM
 *   { type: "opus", timestamp, data } — encoded frame (data transferred)
 *   { type: "worklet-msg", data }     — forward verbatim to the worklet port
 *   { type: "reset-decoder" }   — close the decoder; rebuilt on next frame
 *
 * Messages OUT (to AudioPlayer):
 *   { type: "worklet-msg", data }  — relayed worklet message (pos/event)
 *   { type: "stats", framesDecoded, lastDecodedAt } — decode-health
 *                                 counters, throttled to ~2 Hz
 *   { type: "fatal", reason }   — decoder unusable in this worker; the
 *                                 player falls back to inline decode
 */
const WORKER_SRC = /* js */ `
let decoder = null;
let port = null;      // worklet MessagePort, when the graph is up
let pending = [];     // decoded PCM waiting for a port
let framesDecoded = 0;
let lastDecodedAt = 0;
let lastStatsAt = 0;

function sendStats(now) {
  if (now - lastStatsAt < 500) return;
  lastStatsAt = now;
  self.postMessage({ type: "stats", framesDecoded, lastDecodedAt });
}

function initDecoder() {
  if (typeof AudioDecoder === "undefined") {
    self.postMessage({ type: "fatal", reason: "no AudioDecoder in worker" });
    return;
  }
  try {
    decoder = new AudioDecoder({
      output: onDecodedFrame,
      error: () => { decoder = null; },
    });
    decoder.configure({ codec: "opus", sampleRate: 48000, numberOfChannels: 2 });
  } catch (e) {
    decoder = null;
    self.postMessage({ type: "fatal", reason: String(e) });
  }
}

function onDecodedFrame(frame) {
  framesDecoded++;
  const now = Date.now();
  lastDecodedAt = now;
  // Extract f32-planar samples: [L...L, R...R] — the worklet's format.
  const n = frame.numberOfFrames;
  const pcm = new Float32Array(n * 2);
  try {
    const left = new Float32Array(n);
    const right = new Float32Array(n);
    frame.copyTo(left, { planeIndex: 0, format: "f32-planar" });
    frame.copyTo(right, { planeIndex: 1, format: "f32-planar" });
    pcm.set(left, 0);
    pcm.set(right, n);
  } catch {
    try {
      frame.copyTo(pcm, { planeIndex: 0 });
    } catch {
      frame.close();
      return;
    }
  }
  frame.close();
  if (port) {
    port.postMessage(pcm, [pcm.buffer]);
  } else {
    // AudioWorklet.addModule() can be slow on iPadOS.  Bound startup latency
    // by retaining the newest frames, not the oldest frames from when setup
    // began.
    if (pending.length >= ${MAX_STAGING_FRAMES}) pending.shift();
    pending.push(pcm);
  }
  sendStats(now);
}

self.onmessage = (e) => {
  const d = e.data;
  if (!d) return;
  if (d.type === "opus") {
    if (!decoder || decoder.state === "closed") initDecoder();
    if (!decoder || decoder.state !== "configured") return;
    try {
      decoder.decode(new EncodedAudioChunk({
        type: "key", // Opus frames are independently decodable
        timestamp: d.timestamp * 1000, // ms → µs
        data: d.data,
      }));
    } catch {
      try { decoder.close(); } catch {}
      decoder = null;
    }
  } else if (d.type === "transport-port") {
    // A port from the transport worker, delivering encoded frames straight
    // off the socket. Same messages as the main thread sends, so the same
    // handler takes them — the point is only that the main thread is not
    // involved in carrying them.
    const incoming = e.ports && e.ports[0];
    if (incoming) {
      incoming.onmessage = (ev) => self.onmessage({ data: ev.data });
    }
  } else if (d.type === "port") {
    port = d.port;
    port.onmessage = (ev) =>
      self.postMessage({ type: "worklet-msg", data: ev.data });
    for (const pcm of pending) port.postMessage(pcm, [pcm.buffer]);
    pending = [];
  } else if (d.type === "detach") {
    if (port) {
      port.onmessage = null;
      try { port.close(); } catch {}
    }
    port = null;
    pending = [];
  } else if (d.type === "worklet-msg") {
    if (d.data === "flush") pending = [];
    if (port) port.postMessage(d.data);
  } else if (d.type === "reset-decoder") {
    if (decoder && decoder.state !== "closed") {
      try { decoder.close(); } catch {}
    }
    decoder = null;
    framesDecoded = 0;
    lastDecodedAt = 0;
  }
};
`;

// -- Timeline entry for mapping samples → server timestamps ----------------

export class AudioPlayer {
  private ctx: AudioContext | null = null;
  private decoder: AudioDecoder | null = null;
  private worklet: AudioWorkletNode | null = null;
  private gain: GainNode | null = null;
  /**
   * Dedicated decode worker (see WORKER_SRC).  When non-null, the decoder
   * lives in the worker and the worklet's port is transferred there, so
   * the main thread only relays ~50 tiny Opus frames per second.  Null
   * means inline mode: decode + worklet feed on the main thread (either
   * Worker is unavailable, or the worker declared itself broken).
   */
  private worker: Worker | null = null;
  /** Set when the worker path failed — never try it again this player. */
  private workerBroken = false;
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
  // Audio runs a simple depth-based servo against the worklet's adaptive
  // bufferTarget: below it we slow consumption (rate < 1) to refill; above
  // it we speed up (rate > 1) to drain.  Keeping the learned target as the
  // steady depth is what lets jittery clients remain glitch-free; the hard
  // target ceiling bounds the resulting latency.

  /** Number of audio frames received (for warmup). */
  private framesReceived = 0;
  /** Current playback rate sent to the worklet. */
  private currentRate = 1.0;
  /** Smoothed rate — exponentially filtered to avoid wow/flutter. */
  private smoothedRate = 1.0;
  /** Worklet's current adaptive bufferTarget (samples), mirrored from reports. */
  private currentBufferTarget = MIN_BUFFER_SAMPLES;
  /** Last observed buffered depth (samples, from pos reports) — feeds the drift servo. */
  private lastBufferedSamples = 0;
  /** Timestamp (ms) of the last `skip` posted to the worklet; gates SKIP_COOLDOWN_MS. */
  private lastSkipAt = 0;
  /**
   * What the jitter buffer has been through this session.
   *
   * The worklet already decides everything about buffering and says so in its
   * events, but nothing kept the tally, so "the audio glitches occasionally"
   * had no number attached and no way to tell an over-tight buffer from a
   * genuinely bad link. `peakTargetSamples` is the interesting one: it is the
   * headroom this connection actually turned out to need, which the decay
   * gives back within half a minute of calm.
   */
  private stats = {
    underruns: 0,
    rebuffers: 0,
    shrinks: 0,
    skips: 0,
    peakTargetSamples: MIN_BUFFER_SAMPLES,
  };

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

  /**
   * Jitter-buffer health, in milliseconds and counts.
   *
   * `peakMs` against `targetMs` is the diagnosis: a peak well above the
   * current target means the link needed that headroom and the decay has
   * since given it back, which is why rare glitches keep recurring instead
   * of the buffer settling somewhere that survives them.
   */
  get bufferStats(): {
    targetMs: number;
    peakMs: number;
    received: number;
    decoded: number;
    underruns: number;
    rebuffers: number;
    shrinks: number;
    skips: number;
  } {
    return {
      targetMs: Math.round(this.currentBufferTarget / 48),
      peakMs: Math.round(this.stats.peakTargetSamples / 48),
      // Received counts headers arriving on this thread; decoded counts what
      // the worker actually produced. They track each other when frames reach
      // the decoder, and diverge when they are lost on the way — which is the
      // difference between "late" and "gone".
      received: this.framesReceived,
      decoded: this.framesDecoded,
      underruns: this.stats.underruns,
      rebuffers: this.stats.rebuffers,
      shrinks: this.stats.shrinks,
      skips: this.stats.skips,
    };
  }

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

  /** Whether this browser can route playback to a chosen output device. */
  static get outputSelectionSupported(): boolean {
    return (
      typeof AudioContext !== "undefined" &&
      "setSinkId" in AudioContext.prototype
    );
  }

  /**
   * Route playback to a specific output device — `""` is the system default.
   *
   * Remembered rather than applied once: the context is torn down and rebuilt
   * whenever the browser closes it (device removal, resource pressure), and
   * the choice has to survive that.
   */
  setOutputDevice(deviceId: string): void {
    this._outputDeviceId = deviceId;
    void this.applyOutputDevice();
  }

  private _outputDeviceId = "";

  private async applyOutputDevice(): Promise<void> {
    const ctx = this.ctx as
      | (AudioContext & {
          setSinkId?: (id: string) => Promise<void>;
        })
      | null;
    if (!ctx?.setSinkId) return;
    try {
      await ctx.setSinkId(this._outputDeviceId);
    } catch {
      // A device that has gone away leaves playback on the previous sink,
      // which is better than silence; the panel still shows the selection.
    }
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
      this.postToWorklet("flush");
      this.resetSync();
    }
    this.emit();
  }

  /**
   * A port the transport worker can push encoded frames into.
   *
   * Returns null when there is no decode worker to reach — no `Worker`, or it
   * failed to start — in which case the caller must keep feeding frames the
   * ordinary way rather than sending them somewhere that cannot decode them.
   *
   * The decoder is what moves off the main thread here; the AudioContext
   * cannot, so `handleAudioFrame` still runs for every frame, just without a
   * payload to carry.
   */
  /**
   * Called when the decode worker dies, for whoever is bypassing the main
   * thread to reach it. Set by the owner of the transport, since only that
   * side knows how to revoke the shortcut it asked for.
   */
  onDecodeWorkerLost?: () => void;

  transportAudioPort(): MessagePort | null {
    if (!this.worker && !this.workerBroken && typeof Worker !== "undefined") {
      this.initWorker();
    }
    if (!this.worker || typeof MessageChannel === "undefined") return null;
    const channel = new MessageChannel();
    this.worker.postMessage({ type: "transport-port" }, [channel.port1]);
    return channel.port2;
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

    // Prefer the worker decode path: it keeps decoded-PCM delivery to the
    // worklet independent of main-thread load (video decode + draw), which
    // otherwise starves the jitter buffer exactly when video is busiest.
    if (!this.worker && !this.workerBroken && typeof Worker !== "undefined") {
      this.initWorker();
    }

    this.framesReceived++;

    // A header with no payload means the transport worker already delivered
    // the encoded frame straight to the decoder. Everything above still had to
    // run — it owns the AudioContext, which no worker can touch — but there is
    // nothing left to decode here.
    if (data.length === 0) return;

    if (this.worker) {
      // `data` is a view into the transport's message buffer — copy the
      // frame so transferring doesn't detach unrelated bytes.  Opus
      // frames are ~100–300 B, so the copy is negligible.
      const copy = data.slice();
      this.worker.postMessage({ type: "opus", timestamp, data: copy }, [
        copy.buffer,
      ]);
      this.decodesRequested++;
      return;
    }

    if (!this.decoder || this.decoder.state === "closed") {
      this.initDecoder();
    }
    if (!this.decoder || this.decoder.state !== "configured") return;

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
    this.postToWorklet("flush");
    this.worker?.postMessage({ type: "reset-decoder" });
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
    this.worker?.postMessage({ type: "reset-decoder" });
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
    if (this.worker) {
      this.worker.terminate();
      this.worker = null;
    }
    this.listeners.clear();
  }

  // -- Internal: rate servo -------------------------------------------------

  private resetSync(): void {
    this.framesReceived = 0;
    this.currentRate = 1.0;
    this.smoothedRate = 1.0;
    this.currentBufferTarget = MIN_BUFFER_SAMPLES;
    this.lastBufferedSamples = 0;
    this.lastSkipAt = 0;
    this.resetDecoderState();
  }

  /**
   * Close and null the decoder, resetting all stall-detection counters.
   * The AudioContext and worklet are left intact — only the decode chain
   * is rebuilt.  This avoids the expensive teardown+async-reinit of the
   * full pipeline when only the decoder is broken.
   */
  private resetDecoder(): void {
    this.worker?.postMessage({ type: "reset-decoder" });
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
   * ±MAX_RATE_OFFSET to return there.  Excess too large for the rate servo
   * to absorb is dropped outright.
   */
  private onWorkletPosition(): void {
    const now = Date.now();
    this.lastWorkletReportAt = now;

    // Don't adjust during warmup — not enough samples to stabilise.
    if (this.framesReceived < SYNC_WARMUP_FRAMES) return;

    // Latency backstop, ahead of the servo: the rate servo trims at
    // MAX_RATE_OFFSET, so anything that arrives in a burst (a backlogged
    // server queue flushing, a catch-up replay, an unthrottled tab)
    // would otherwise take tens of seconds to drain and be audible as
    // lag the whole time.  Drop straight back to target instead.
    if (
      this.lastBufferedSamples >=
        this.currentBufferTarget + SKIP_EXCESS_MS * 48 &&
      now - this.lastSkipAt >= SKIP_COOLDOWN_MS
    ) {
      this.lastSkipAt = now;
      const excess = this.lastBufferedSamples - this.currentBufferTarget;
      // Assume the skip lands; the next report may still be pre-skip and
      // must not be read as "still too deep" once the cooldown expires.
      this.lastBufferedSamples = this.currentBufferTarget;
      this.postToWorklet({ type: "skip", samples: excess });
    }

    // Servo target: keep `buffered` at the learned adaptive target.
    //   buffered < target → drift > 0 → rate < 1 (slow down, refill)
    //   buffered > target → drift < 0 → rate > 1 (speed up, drain)
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
      this.postToWorklet({
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

    // When the tab returns from background, audio can be in a broken
    // state (context suspended, worklet stalled, decode chain dead) —
    // but usually it isn't: browsers exempt audibly-playing tabs from
    // background throttling, so the pipeline keeps rendering while
    // hidden, and a preemptive reset would audibly interrupt it.  Reset
    // only when the worklet stopped rendering while hidden or the
    // context died; otherwise run an immediate health check, which can
    // still escalate to a reset on real stalls.
    if (!this.visibilityHandler && typeof document !== "undefined") {
      let hiddenAt = 0;
      this.visibilityHandler = () => {
        if (document.visibilityState === "hidden") {
          hiddenAt = Date.now();
        } else if (document.visibilityState === "visible") {
          if (this._destroyed || this._muted || !this._subscribed) return;
          const now = Date.now();
          const wasHiddenMs = hiddenAt > 0 ? now - hiddenAt : 0;
          hiddenAt = 0;
          // The worklet reports its position every ~100 ms whenever the
          // context is running, so a stale report means the pipeline was
          // not rendering in the background.
          const workletStale =
            this.lastWorkletReportAt > 0 &&
            now - this.lastWorkletReportAt > 3_000;
          const ctxDead = this.ctx != null && this.ctx.state === "closed";
          if (wasHiddenMs > 3_000 && (workletStale || ctxDead)) {
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
    // The worklet's port lives in the worker (transferred) — tell it to
    // stop feeding the dead port and drop any pending PCM.
    this.worker?.postMessage({ type: "detach" });
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
    // Decide decode mode before the worklet exists: its port can only be
    // handed to the worker at creation time (transfer neuters the port).
    this.initWorker();
    try {
      // Declared before the context exists: iOS picks the Bluetooth profile
      // when the context is created, and its default "auto" can leave a
      // headset on the bidirectional HFP link long after any capture ended.
      claimPlaybackAudioSession();
      this.ctx = new AudioContext({ sampleRate: 48000 });
      this.gain = this.ctx.createGain();
      this.gain.gain.value = this._muted ? 0 : 1;
      this.gain.connect(this.ctx.destination);
      // A rebuilt context starts on the default sink, so the chosen output
      // has to be re-applied here rather than only when it is picked.
      void this.applyOutputDevice();

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

      if (this.worker) {
        // Hand the worklet's port to the decode worker: decoded PCM then
        // flows worker → audio thread with no main-thread hop, and the
        // worklet's outbound messages (pos/event) are relayed back to us
        // by the worker.  After transfer this side's `worklet.port` is
        // neutered — all worklet messaging goes through postToWorklet().
        this.worker.postMessage({ type: "port", port: this.worklet.port }, [
          this.worklet.port,
        ]);
      } else {
        // Inline mode: listen for position reports and buffer events
        // from the worklet directly.
        this.worklet.port.onmessage = (e: MessageEvent) => {
          this.handleWorkletMessage(e.data);
        };

        // Flush any frames that arrived before the worklet was ready.
        for (const pcm of this.buffer) {
          this.worklet.port.postMessage(pcm, [pcm.buffer]);
        }
        this.buffer = [];
      }
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

  /**
   * Send a control message to the worklet processor, routing through the
   * decode worker when it owns the worklet's (transferred) port.
   */
  private postToWorklet(msg: unknown): void {
    if (this.worker) {
      this.worker.postMessage({ type: "worklet-msg", data: msg });
    } else if (this.worklet) {
      this.worklet.port.postMessage(msg);
    }
  }

  /** Handle a worklet-originated message (direct or relayed by the worker). */
  private handleWorkletMessage(d: any): void {
    if (!d) return;
    if (d.type === "pos") {
      if (typeof d.target === "number") {
        this.currentBufferTarget = d.target;
      }
      if (typeof d.buffered === "number") {
        this.lastBufferedSamples = d.buffered;
      }
      this.onWorkletPosition();
    } else if (d.type === "event") {
      if (typeof d.target === "number") {
        this.currentBufferTarget = d.target;
        this.stats.peakTargetSamples = Math.max(
          this.stats.peakTargetSamples,
          d.target,
        );
      }
      // `grow` is posted on the leading edge of an underrun and nowhere else,
      // so it counts underrun *events* — a run of starved blocks is one, which
      // is the unit worth counting. It arrives even when the target is already
      // at the ceiling and cannot grow.
      if (d.kind === "grow") this.stats.underruns += 1;
      else if (d.kind === "rebuffer_start") this.stats.rebuffers += 1;
      else if (d.kind === "shrink") this.stats.shrinks += 1;
      else if (d.kind === "skip") this.stats.skips += 1;
      // The skip reply carries the post-skip depth — authoritative, and
      // it arrives before the next position report.  Adopt it so the
      // servo works from the real depth rather than our projection.
      if (d.kind === "skip" && typeof d.buffered === "number") {
        this.lastBufferedSamples = d.buffered;
      } else if (d.kind === "rebuffer_end" && typeof d.buffered === "number") {
        // Adopt the post-rebuffer depth and target immediately, before the
        // next ~100 ms position report.  At the target this is intentionally
        // a no-op: the newly learned safety margin must be preserved.
        this.lastBufferedSamples = d.buffered;
        this.onWorkletPosition();
      }
    }
  }

  /**
   * Spawn the decode worker (idempotent).  Called from both
   * handleAudioFrame() and initAudioContext() so that whichever runs
   * first decides the mode before the worklet is wired up — the worklet
   * port must be transferred at creation, not retrofitted.
   */
  private initWorker(): void {
    if (
      this.worker ||
      this.workerBroken ||
      this._destroyed ||
      typeof Worker === "undefined"
    ) {
      return;
    }
    try {
      const blob = new Blob([WORKER_SRC], { type: "application/javascript" });
      const url = URL.createObjectURL(blob);
      try {
        this.worker = new Worker(url);
      } finally {
        URL.revokeObjectURL(url);
      }
    } catch {
      this.worker = null;
      this.workerBroken = true;
      return;
    }

    const fail = () => {
      if (this.worker) {
        this.worker.terminate();
        this.worker = null;
      }
      this.workerBroken = true;
      // Anything routing frames straight to this worker is now feeding a
      // corpse, and the main thread cannot make up the difference: on that
      // route it receives headers with no payload, so there is nothing left
      // to decode inline. Tell it to stop before rebuilding.
      this.onDecodeWorkerLost?.();
      // If the worklet port was already transferred to the dead worker,
      // it's lost — rebuild the graph in inline mode.
      if (!this._destroyed) this.resetPipeline();
    };

    this.worker.onerror = fail;
    this.worker.onmessage = (e: MessageEvent) => {
      const d = e.data;
      if (!d) return;
      if (d.type === "worklet-msg") {
        this.handleWorkletMessage(d.data);
      } else if (d.type === "stats") {
        if (typeof d.framesDecoded === "number") {
          this.framesDecoded = d.framesDecoded;
        }
        if (typeof d.lastDecodedAt === "number" && d.lastDecodedAt > 0) {
          this.lastDecodedAt = d.lastDecodedAt;
        }
      } else if (d.type === "fatal") {
        fail();
      }
    };
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
    } else {
      // Worklet not ready yet.  Keep the newest bounded tail so a slow
      // AudioWorklet.addModule() does not start playback far behind live.
      if (this.buffer.length >= MAX_STAGING_FRAMES) this.buffer.shift();
      this.buffer.push(pcm);
    }
  }
}
