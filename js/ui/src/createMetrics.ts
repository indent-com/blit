import { createSignal, createEffect, onCleanup, onMount } from "solid-js";
import type { BlitTransport } from "@blit-sh/core";

export interface RenderSample {
  t: number;
  ms: number;
}

export interface NetSample {
  t: number;
  bytes: number;
  dir: "rx" | "tx";
}

export interface Metrics {
  bw: number;
  fps: number;
  ups: number;
  renderMs: number;
  maxRenderMs: number;
}

const INTERVAL = 1000;

export function createMetrics(transports: () => readonly BlitTransport[]): {
  metrics: () => Metrics;
  countFrame: (renderMs?: number) => void;
  timeline: RenderSample[];
  net: NetSample[];
} {
  const TIMELINE_MAX = 500;
  const NET_MAX = 2000;

  const timeline: RenderSample[] = [];
  const net: NetSample[] = [];

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
      timeline.push({ t: performance.now(), ms: renderMs });
      if (timeline.length > TIMELINE_MAX)
        timeline.splice(0, timeline.length - TIMELINE_MAX);
    }
  }

  const onMessage = (data: ArrayBuffer) => {
    bytes += data.byteLength;
    const view = new Uint8Array(data);
    if (view[0] === 0x00) updates++;
    net.push({ t: performance.now(), bytes: data.byteLength, dir: "rx" });
    if (net.length > NET_MAX) net.splice(0, net.length - NET_MAX);
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
