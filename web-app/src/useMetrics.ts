import { useEffect, useRef, useState } from "react";
import type { BlitTransport } from "blit-react";

export interface Metrics {
  bw: number;
  fps: number;
  ups: number;
}

const INTERVAL = 1000;
const C2S_DISPLAY_RATE = 0x04;
const INITIAL_TARGET_FPS = 120;

export function useMetrics(transport: BlitTransport): Metrics {
  const [metrics, setMetrics] = useState<Metrics>({ bw: 0, fps: 0, ups: 0 });
  const bytesRef = useRef(0);
  const framesRef = useRef(0);
  const updatesRef = useRef(0);

  useEffect(() => {
    const onMessage = (data: ArrayBuffer) => {
      bytesRef.current += data.byteLength;
      const view = new Uint8Array(data);
      if (view[0] === 0x00) {
        updatesRef.current++;
      }
    };
    transport.addEventListener("message", onMessage);

    let rafId = 0;
    let prevTime = 0;
    let fpsEwma = INITIAL_TARGET_FPS;
    let lastReported = 0;

    function sendDisplayRate(fps: number) {
      const msg = new Uint8Array(3);
      msg[0] = C2S_DISPLAY_RATE;
      msg[1] = fps & 0xff;
      msg[2] = (fps >> 8) & 0xff;
      transport.send(msg);
    }

    const onStatus = (status: string) => {
      if (status === "connected") {
        const fps = lastReported || INITIAL_TARGET_FPS;
        sendDisplayRate(fps);
        lastReported = fps;
      }
    };
    transport.addEventListener("statuschange", onStatus);
    if (transport.status === "connected") {
      sendDisplayRate(INITIAL_TARGET_FPS);
      lastReported = INITIAL_TARGET_FPS;
    }

    const countFrame = (now: number) => {
      framesRef.current++;
      if (prevTime > 0) {
        const dt = now - prevTime;
        if (dt > 0 && dt < 500) {
          fpsEwma = fpsEwma * 0.8 + (1000 / dt) * 0.2;
          const advertised = Math.max(10, Math.round(fpsEwma / 5) * 5);
          if (advertised !== lastReported && transport.status === "connected") {
            sendDisplayRate(advertised);
            lastReported = advertised;
          }
        }
      }
      prevTime = now;
      rafId = requestAnimationFrame(countFrame);
    };
    rafId = requestAnimationFrame(countFrame);

    const timer = setInterval(() => {
      setMetrics({
        bw: bytesRef.current,
        fps: framesRef.current,
        ups: updatesRef.current,
      });
      bytesRef.current = 0;
      framesRef.current = 0;
      updatesRef.current = 0;
    }, INTERVAL);

    return () => {
      transport.removeEventListener("message", onMessage);
      transport.removeEventListener("statuschange", onStatus);
      cancelAnimationFrame(rafId);
      clearInterval(timer);
    };
  }, [transport]);

  return metrics;
}

export function formatBw(bytes: number): string {
  if (bytes < 1024) return `${bytes} B/s`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB/s`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB/s`;
}
