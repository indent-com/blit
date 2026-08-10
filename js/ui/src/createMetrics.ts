import { createSignal, createEffect, onCleanup, onMount } from "solid-js";
import type { BlitTransport, BlitTransportMessage } from "@blit-sh/core";

export interface Metrics {
  bw: number;
  fps: number;
  ups: number;
  renderMs: number;
  maxRenderMs: number;
}

abstract class TypedSampleRing {
  private start = 0;
  length = 0;

  constructor(readonly capacity: number) {
    if (!Number.isInteger(capacity) || capacity < 1)
      throw new RangeError("capacity must be a positive integer");
  }

  protected pushIndex(): number {
    if (this.length < this.capacity) {
      const index = (this.start + this.length) % this.capacity;
      this.length++;
      return index;
    }
    const index = this.start;
    this.start = (this.start + 1) % this.capacity;
    return index;
  }

  protected physicalIndex(index: number): number {
    return index < 0 || index >= this.length
      ? -1
      : (this.start + index) % this.capacity;
  }
}

/** Struct-of-typed-arrays render history: no sample objects, shifts, or
 * backing-store allocations in the measurement path. */
export class RenderSampleRing extends TypedSampleRing {
  private readonly times: Float64Array;
  private readonly durations: Float64Array;

  constructor(capacity: number) {
    super(capacity);
    this.times = new Float64Array(capacity);
    this.durations = new Float64Array(capacity);
  }

  push(time: number, duration: number): void {
    const index = this.pushIndex();
    this.times[index] = time;
    this.durations[index] = duration;
  }

  time(index: number): number {
    const physical = this.physicalIndex(index);
    return physical < 0 ? NaN : this.times[physical];
  }

  duration(index: number): number {
    const physical = this.physicalIndex(index);
    return physical < 0 ? NaN : this.durations[physical];
  }
}

/** Struct-of-typed-arrays network history. */
export class NetSampleRing extends TypedSampleRing {
  private readonly times: Float64Array;
  private readonly sizes: Uint32Array;
  private readonly directions: Uint8Array;

  constructor(capacity: number) {
    super(capacity);
    this.times = new Float64Array(capacity);
    this.sizes = new Uint32Array(capacity);
    this.directions = new Uint8Array(capacity);
  }

  push(time: number, bytes: number, rx: boolean): void {
    const index = this.pushIndex();
    this.times[index] = time;
    this.sizes[index] = bytes;
    this.directions[index] = rx ? 1 : 0;
  }

  time(index: number): number {
    const physical = this.physicalIndex(index);
    return physical < 0 ? NaN : this.times[physical];
  }

  bytes(index: number): number {
    const physical = this.physicalIndex(index);
    return physical < 0 ? 0 : this.sizes[physical];
  }

  isRx(index: number): boolean {
    const physical = this.physicalIndex(index);
    return physical >= 0 && this.directions[physical] !== 0;
  }
}

const INTERVAL = 1000;

export function createMetrics(
  transports: () => readonly BlitTransport[],
  sampleTimelines: () => boolean = () => true,
): {
  metrics: () => Metrics;
  countFrame: (renderMs?: number) => void;
  timeline: RenderSampleRing;
  net: NetSampleRing;
} {
  const TIMELINE_MAX = 500;
  const NET_MAX = 2000;

  const timeline = new RenderSampleRing(TIMELINE_MAX);
  const net = new NetSampleRing(NET_MAX);

  let bytes = 0;
  let frames = 0;
  let updates = 0;
  let renderMsSum = 0;
  let renderMsMax = 0;

  const [metrics, setMetrics] = createSignal<Metrics>({
    bw: 0,
    fps: 0,
    ups: 0,
    renderMs: 0,
    maxRenderMs: 0,
  });

  function countFrame(renderMs?: number) {
    frames++;
    if (renderMs != null) {
      renderMsSum += renderMs;
      renderMsMax = Math.max(renderMsMax, renderMs);
      if (sampleTimelines()) timeline.push(performance.now(), renderMs);
    }
  }

  const onMessage = (data: BlitTransportMessage) => {
    bytes += data.byteLength;
    const first =
      data.byteLength === 0
        ? undefined
        : data instanceof Uint8Array
          ? data[0]
          : new Uint8Array(data, 0, 1)[0];
    if (first === 0x00) updates++;
    if (sampleTimelines()) net.push(performance.now(), data.byteLength, true);
  };

  // Re-register transport listeners whenever the transport list changes.
  createEffect(() => {
    const current = transports();
    for (const t of current) t.addEventListener("message", onMessage);
    onCleanup(() => {
      for (const t of current) t.removeEventListener("message", onMessage);
    });
  });

  onMount(() => {
    const timer = setInterval(() => {
      setMetrics({
        bw: bytes,
        fps: frames,
        ups: updates,
        renderMs: frames > 0 ? renderMsSum / frames : 0,
        maxRenderMs: renderMsMax,
      });
      bytes = 0;
      frames = 0;
      updates = 0;
      renderMsSum = 0;
      renderMsMax = 0;
    }, INTERVAL);

    onCleanup(() => clearInterval(timer));
  });

  return { metrics, countFrame, timeline, net };
}

export function formatBw(bytes: number): string {
  if (bytes < 1024) return `${bytes} B/s`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB/s`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB/s`;
}
