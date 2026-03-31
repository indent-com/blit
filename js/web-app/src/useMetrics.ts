import { useCallback, useEffect, useRef, useState } from "react";
import type { BlitTransport } from "blit-react";

export interface RenderSample {
  t: number; // timestamp (ms, relative to page load)
  ms: number; // render duration
}

export interface NetSample {
  t: number; // timestamp (ms, relative to page load)
  bytes: number; // message size
  dir: "rx" | "tx"; // direction
}

export interface Metrics {
  bw: number;
  fps: number;
  ups: number;
  renderMs: number;
  maxRenderMs: number;
}

const INTERVAL = 1000;

export function useMetrics(transport: BlitTransport): Metrics & {
  countFrame: (renderMs?: number) => void;
  timelineRef: React.RefObject<RenderSample[]>;
  netRef: React.RefObject<NetSample[]>;
} {
  const [metrics, setMetrics] = useState<Metrics>({
    bw: 0,
    fps: 0,
    ups: 0,
    renderMs: 0,
    maxRenderMs: 0,
  });
  const bytesRef = useRef(0);
  const framesRef = useRef(0);
  const updatesRef = useRef(0);
  const renderMsSumRef = useRef(0);
  const renderMsMaxRef = useRef(0);
  const timelineRef = useRef<RenderSample[]>([]);
  const netRef = useRef<NetSample[]>([]);
  const TIMELINE_MAX = 500;
  const NET_MAX = 2000;

  const countFrame = useCallback((renderMs?: number) => {
    framesRef.current++;
    if (renderMs != null) {
      renderMsSumRef.current += renderMs;
      renderMsMaxRef.current = Math.max(renderMsMaxRef.current, renderMs);
      const tl = timelineRef.current;
      tl.push({ t: performance.now(), ms: renderMs });
      if (tl.length > TIMELINE_MAX) tl.splice(0, tl.length - TIMELINE_MAX);
    }
  }, []);

  useEffect(() => {
    const onMessage = (data: ArrayBuffer) => {
      bytesRef.current += data.byteLength;
      const view = new Uint8Array(data);
      if (view[0] === 0x00) {
        updatesRef.current++;
      }
      const nl = netRef.current;
      nl.push({ t: performance.now(), bytes: data.byteLength, dir: "rx" });
      if (nl.length > NET_MAX) nl.splice(0, nl.length - NET_MAX);
    };
    transport.addEventListener("message", onMessage);

    const timer = setInterval(() => {
      const frames = framesRef.current;
      setMetrics({
        bw: bytesRef.current,
        fps: frames,
        ups: updatesRef.current,
        renderMs: frames > 0 ? renderMsSumRef.current / frames : 0,
        maxRenderMs: renderMsMaxRef.current,
      });
      bytesRef.current = 0;
      framesRef.current = 0;
      updatesRef.current = 0;
      renderMsSumRef.current = 0;
      renderMsMaxRef.current = 0;
    }, INTERVAL);

    return () => {
      transport.removeEventListener("message", onMessage);
      clearInterval(timer);
    };
  }, [transport]);

  return { ...metrics, countFrame, timelineRef, netRef };
}

export function formatBw(bytes: number): string {
  if (bytes < 1024) return `${bytes} B/s`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB/s`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB/s`;
}
