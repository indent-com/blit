/**
 * Tests for the audio jitter buffer's latency bounds.
 *
 * The worklet runs on the audio render thread as source text in a Blob
 * URL, so nothing else typechecks or exercises it.  These tests
 * instantiate the processor directly against a stub
 * `AudioWorkletProcessor` and drive `process()` by hand.
 *
 * What they pin down is the *bound*, not the tuning: buffer growth is
 * one-way per underrun and decay is slow, so an unbounded target
 * ratchets audio permanently behind live — the failure that motivated
 * MAX_BUFFER_TARGET_SAMPLES and the skip path.  Tuning constants may
 * move; the ceiling must hold.
 */

import { describe, it, expect, beforeEach } from "vitest";
import {
  WORKLET_SRC,
  MIN_BUFFER_SAMPLES,
  MAX_BUFFER_TARGET_SAMPLES,
  SAMPLES_PER_20_MS,
  GROW_FRAMES_PER_UNDERRUN,
} from "../AudioPlayer";

const RENDER_QUANTUM = 128;

interface PortMessage {
  type: string;
  kind?: string;
  value?: number;
  target?: number;
  buffered?: number;
  requested?: number;
  skipped?: number;
}

interface Processor {
  buffered: number;
  bufferTarget: number;
  buffering: boolean;
  fadeGain: number;
  depth(): number;
  port: { onmessage: ((e: { data: unknown }) => void) | null };
  process(inputs: unknown[], outputs: Float32Array[][]): boolean;
}

/**
 * Evaluate WORKLET_SRC against a stub AudioWorkletProcessor and return a
 * fresh processor plus the messages it posts.
 */
function makeProcessor(): { proc: Processor; posted: PortMessage[] } {
  const posted: PortMessage[] = [];
  let Registered: new () => Processor;

  class StubProcessor {
    port = {
      onmessage: null as ((e: { data: unknown }) => void) | null,
      postMessage: (m: PortMessage) => posted.push(m),
    };
  }

  const factory = new Function(
    "AudioWorkletProcessor",
    "registerProcessor",
    `${WORKLET_SRC}\nreturn BlitAudioProcessor;`,
  );
  Registered = factory(StubProcessor, () => {});
  return { proc: new Registered(), posted };
}

/** A 20 ms stereo PCM frame ([L...L, R...R]) of constant amplitude. */
function pcmFrame(amplitude = 0.5): Float32Array {
  return new Float32Array(SAMPLES_PER_20_MS * 2).fill(amplitude);
}

function send(proc: Processor, data: unknown): void {
  proc.port.onmessage?.({ data });
}

/** Run `blocks` render quanta, returning the concatenated left channel. */
function render(proc: Processor, blocks: number): Float32Array {
  const out = new Float32Array(blocks * RENDER_QUANTUM);
  for (let b = 0; b < blocks; b++) {
    const l = new Float32Array(RENDER_QUANTUM);
    const r = new Float32Array(RENDER_QUANTUM);
    proc.process([], [[l, r]]);
    out.set(l, b * RENDER_QUANTUM);
  }
  return out;
}

/**
 * Drive one underrun event: fill past the target, play until the buffer
 * is dry, then keep rendering so the empty blocks register as underruns.
 */
function underrunOnce(proc: Processor): void {
  const framesToFill = Math.ceil(proc.bufferTarget / SAMPLES_PER_20_MS) + 1;
  for (let i = 0; i < framesToFill; i++) send(proc, pcmFrame());
  // Drain everything that was queued, then run dry.
  render(proc, Math.ceil((framesToFill * SAMPLES_PER_20_MS) / RENDER_QUANTUM));
  render(proc, 4);
}

describe("audio worklet jitter buffer", () => {
  let proc: Processor;
  let posted: PortMessage[];

  beforeEach(() => {
    ({ proc, posted } = makeProcessor());
  });

  it("starts at the minimum buffer target", () => {
    expect(proc.bufferTarget).toBe(MIN_BUFFER_SAMPLES);
  });

  it("grows the target on the leading edge of an underrun", () => {
    underrunOnce(proc);
    expect(proc.bufferTarget).toBe(
      MIN_BUFFER_SAMPLES + SAMPLES_PER_20_MS * GROW_FRAMES_PER_UNDERRUN,
    );
    expect(posted.some((m) => m.kind === "grow")).toBe(true);
  });

  it("never ratchets the target past the ceiling", () => {
    // Far more underrun events than it takes to reach the ceiling.
    const events =
      Math.ceil(
        (MAX_BUFFER_TARGET_SAMPLES - MIN_BUFFER_SAMPLES) /
          (SAMPLES_PER_20_MS * GROW_FRAMES_PER_UNDERRUN),
      ) + 20;
    for (let i = 0; i < events; i++) underrunOnce(proc);

    expect(proc.bufferTarget).toBe(MAX_BUFFER_TARGET_SAMPLES);
    // 400 ms is the contract the server and the servo are tuned against.
    expect(proc.bufferTarget / 48).toBeLessThanOrEqual(400);
  });

  describe("skip", () => {
    beforeEach(() => {
      // Park 20 frames (400 ms) in the buffer and start playing.
      for (let i = 0; i < 20; i++) send(proc, pcmFrame());
      render(proc, 1);
      posted.length = 0;
    });

    it("drops exactly what was asked for, from the front", () => {
      // Playback has already advanced into the head chunk, so this also
      // covers the partial-chunk arm where offset is non-zero.
      const before = proc.depth();
      send(proc, { type: "skip", samples: SAMPLES_PER_20_MS * 5 });

      expect(proc.depth()).toBe(before - SAMPLES_PER_20_MS * 5);
      const reply = posted.find((m) => m.kind === "skip");
      expect(reply).toBeDefined();
      expect(reply?.skipped).toBe(SAMPLES_PER_20_MS * 5);
      expect(reply?.buffered).toBe(proc.depth());
    });

    it("skips a non-multiple of the chunk size exactly", () => {
      const before = proc.depth();
      send(proc, { type: "skip", samples: 1234 });
      expect(proc.depth()).toBe(before - 1234);
    });

    it("reports a short skip when asked for more than is buffered", () => {
      const before = proc.depth();
      send(proc, { type: "skip", samples: before + SAMPLES_PER_20_MS * 10 });

      expect(proc.depth()).toBe(0);
      const reply = posted.find((m) => m.kind === "skip");
      expect(reply?.requested).toBe(before + SAMPLES_PER_20_MS * 10);
      expect(reply?.skipped).toBe(before);
    });

    it("fades back in so the splice is not a click", () => {
      // Reach full gain first, so a reset to 0 is observable.
      render(proc, 8);
      expect(proc.fadeGain).toBe(1);

      send(proc, { type: "skip", samples: SAMPLES_PER_20_MS });
      expect(proc.fadeGain).toBe(0);

      // The first sample after the splice must ramp, not jump to full
      // amplitude — that ramp is what makes the discontinuity inaudible.
      const out = render(proc, 1);
      expect(Math.abs(out[0])).toBeLessThan(0.5);
      expect(Math.abs(out[0])).toBeGreaterThan(0);
      expect(Math.abs(out[RENDER_QUANTUM - 1])).toBeGreaterThan(
        Math.abs(out[0]),
      );
    });

    it("leaves the target alone — it reclaims depth, not headroom", () => {
      const target = proc.bufferTarget;
      send(proc, { type: "skip", samples: SAMPLES_PER_20_MS * 5 });
      expect(proc.bufferTarget).toBe(target);
    });
  });
});
