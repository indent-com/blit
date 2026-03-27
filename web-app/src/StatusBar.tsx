import { useState, useCallback, useRef, useEffect, type RefObject } from "react";
import type { UseBlitSessionsReturn, TerminalPalette, TerminalStore } from "blit-react";
import { formatBw } from "./useMetrics";
import type { Metrics, RenderSample, NetSample } from "./useMetrics";

type TimelineRef = RefObject<RenderSample[]>;
type NetRef = RefObject<NetSample[]>;
import { themeFor, ui } from "./theme";

export function StatusBar({
  sessions,
  metrics,
  palette,
  termSize,
  store,
  timelineRef,
  netRef,
  onExpose,
  onPalette,
  onFont,
}: {
  sessions: UseBlitSessionsReturn;
  metrics: Metrics;
  palette: TerminalPalette;
  termSize: string | null;
  store: TerminalStore;
  timelineRef: TimelineRef;
  netRef: NetRef;
  onExpose: () => void;
  onPalette: () => void;
  onFont: () => void;
}) {
  const theme = themeFor(palette.dark);
  const visible = sessions.sessions.filter((s) => s.state !== "closed");
  const exited = visible.filter((s) => s.state === "exited").length;
  const focused = sessions.sessions.find((s) => s.ptyId === sessions.focusedPtyId);
  const [debug, setDebug] = useState(false);
  const toggleDebug = useCallback(() => setDebug((d) => !d), []);
  return (
    <>
      <button onClick={onExpose} style={ui.btn} title="Expose (Cmd+K)">
        {visible.length} PTY{visible.length !== 1 ? "s" : ""}
        {exited > 0 && <span style={{ opacity: 0.5 }}> ({exited} exited)</span>}
      </button>
      <span style={{
        flex: 1,
        overflow: "hidden",
        textOverflow: "ellipsis",
        whiteSpace: "nowrap",
        opacity: 0.7,
      }}>
        {focused && (
          <>
            {focused.title ?? `PTY ${focused.ptyId}`}
            {focused.state === "exited" && (
              <span style={{ color: theme.error, marginLeft: 6, fontSize: 11 }}>[exited]</span>
            )}
          </>
        )}
      </span>
      <span style={{ fontSize: 11, opacity: 0.5, flexShrink: 0, whiteSpace: "nowrap" }}>
        {termSize ? `${termSize}@` : ""}{metrics.fps}/{metrics.ups}
      </span>
      <button onClick={toggleDebug} style={{ ...ui.btn, opacity: debug ? 1 : 0.3 }} title="Debug stats">
        &#x1F41B;
      </button>
      <button onClick={onPalette} style={ui.btn} title={`Palette: ${palette.name} (Cmd+Shift+P)`}>
        {palette.dark ? "\u25D1" : "\u25D0"}
      </button>
      <button onClick={onFont} style={ui.btn} title="Font (Cmd+Shift+F)">
        Aa
      </button>
      <span
        role="status"
        aria-label={sessions.status}
        style={{
          width: 6,
          height: 6,
          borderRadius: "50%",
          flexShrink: 0,
          backgroundColor: sessions.status === "connected" ? theme.success : theme.error,
        }}
      />
      {debug && <DebugPanel metrics={metrics} store={store} dark={palette.dark} timelineRef={timelineRef} netRef={netRef} focusedPtyId={sessions.focusedPtyId} />}
    </>
  );
}

function DebugPanel({ metrics, store, dark, timelineRef, netRef, focusedPtyId }: { metrics: Metrics; store: TerminalStore; dark: boolean; timelineRef: TimelineRef; netRef: NetRef; focusedPtyId: number | null }) {
  const s = store.getDebugStats(focusedPtyId);
  const theme = themeFor(dark);
  return (
    <div style={{
      position: "fixed",
      bottom: 28,
      right: 4,
      backgroundColor: dark ? "rgba(30,30,30,0.85)" : "rgba(240,240,240,0.85)",
      backdropFilter: "blur(8px)",
      WebkitBackdropFilter: "blur(8px)",
      color: theme.fg,
      border: `1px solid ${dark ? "rgba(255,255,255,0.1)" : "rgba(0,0,0,0.1)"}`,
      borderRadius: 6,
      padding: "6px 10px",
      fontSize: 11,
      fontFamily: "ui-monospace, monospace",
      lineHeight: 1.6,
      zIndex: 200,
      whiteSpace: "pre",
      pointerEvents: "none",
      minWidth: 260,
    }}>
      <Row label="FPS / UPS" value={`${metrics.fps} / ${metrics.ups}`} />
      <Row label="Bandwidth" value={formatBw(metrics.bw)} />
      <Row label="Render" value={`${metrics.renderMs.toFixed(1)} ms avg, ${metrics.maxRenderMs.toFixed(1)} ms max`} />
      <Row label="Display Hz" value={s.displayFps} />
      <Row label="Backlog" value={s.pendingApplied} />
      <Row label="Ack ahead" value={s.ackAhead} />
      <Row label="Apply" value={`${s.applyMs.toFixed(1)} ms`} />
      <Row label="Mouse" value={`mode=${s.mouseMode} enc=${s.mouseEncoding}`} />
      <Row label="Queued" value={`${s.totalPendingFrames} frames in ${s.pendingFrameQueues} queues`} />
      <Row label="Terminals" value={`${s.terminals} live, ${s.staleTerminals} stale, ${s.frozenPtys} frozen`} />
      <div style={{ borderTop: `1px solid ${dark ? "rgba(255,255,255,0.1)" : "rgba(0,0,0,0.1)"}`, marginTop: 4, paddingTop: 2 }}>
        <span style={{ opacity: 0.6, fontSize: 10 }}>Render</span>
        <RenderTimeline timelineRef={timelineRef} dark={dark} displayFps={s.displayFps} />
      </div>
      <div style={{ borderTop: `1px solid ${dark ? "rgba(255,255,255,0.1)" : "rgba(0,0,0,0.1)"}`, marginTop: 4, paddingTop: 2 }}>
        <span style={{ opacity: 0.6, fontSize: 10 }}>Network</span>
        <NetTimeline netRef={netRef} dark={dark} />
      </div>
    </div>
  );
}

function Row({ label, value }: { label: string; value: string | number }) {
  return (
    <div style={{ display: "flex", justifyContent: "space-between", gap: 16 }}>
      <span style={{ opacity: 0.6 }}>{label}</span>
      <span>{value}</span>
    </div>
  );
}

function RenderTimeline({ timelineRef, dark, displayFps }: { timelineRef: TimelineRef; dark: boolean; displayFps: number }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rafRef = useRef(0);
  const W = 300;
  const H = 80;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  useEffect(() => {
    const draw = () => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = timelineRef.current;
      if (!samples || samples.length < 2) {
        rafRef.current = requestAnimationFrame(draw);
        return;
      }

      const now = performance.now();
      const windowMs = 2000;
      const maxMs = 20;
      const budgetMs = displayFps > 0 ? 1000 / displayFps : 16.67;

      // Budget line
      ctx.strokeStyle = dark ? "rgba(255,80,80,0.4)" : "rgba(200,0,0,0.3)";
      ctx.lineWidth = dpr;
      const budgetY = (1 - budgetMs / maxMs) * H * dpr;
      ctx.beginPath();
      ctx.moveTo(0, budgetY);
      ctx.lineTo(W * dpr, budgetY);
      ctx.stroke();

      // Render bars
      for (const s of samples) {
        const age = now - s.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.min(s.ms / maxMs, 1) * H * dpr;
        const y = H * dpr - barH;

        if (s.ms < budgetMs) ctx.fillStyle = dark ? "rgba(80,200,120,0.8)" : "rgba(40,160,80,0.8)";
        else if (s.ms < budgetMs * 2) ctx.fillStyle = dark ? "rgba(255,200,60,0.8)" : "rgba(200,160,0,0.8)";
        else ctx.fillStyle = dark ? "rgba(255,80,80,0.8)" : "rgba(200,40,40,0.8)";

        ctx.fillRect(x, y, Math.max(1, dpr), barH);
      }

      // Labels
      ctx.fillStyle = dark ? "rgba(255,255,255,0.4)" : "rgba(0,0,0,0.4)";
      ctx.font = `${9 * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(`${maxMs}ms`, 2 * dpr, 2 * dpr);
      ctx.textAlign = "right";
      ctx.fillText(`budget ${budgetMs.toFixed(1)}ms`, (W - 2) * dpr, budgetY - 10 * dpr);
      ctx.textAlign = "left";
      ctx.textBaseline = "bottom";
      ctx.fillText("0ms", 2 * dpr, H * dpr - 2 * dpr);

      rafRef.current = requestAnimationFrame(draw);
    };
    rafRef.current = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(rafRef.current);
  }, [timelineRef, dark, dpr, displayFps]);

  return (
    <canvas
      ref={canvasRef}
      width={W * dpr}
      height={H * dpr}
      style={{ width: W, height: H, marginTop: 2 }}
    />
  );
}

function NetTimeline({ netRef, dark }: { netRef: NetRef; dark: boolean }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rafRef = useRef(0);
  const W = 300;
  const H = 50;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  useEffect(() => {
    const draw = () => {
      const canvas = canvasRef.current;
      if (!canvas) return;
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = netRef.current;
      if (!samples || samples.length === 0) {
        rafRef.current = requestAnimationFrame(draw);
        return;
      }

      const now = performance.now();
      const windowMs = 2000;
      // Auto-scale: find max bytes in the window
      let maxBytes = 256;
      for (const s of samples) {
        if (now - s.t <= windowMs) maxBytes = Math.max(maxBytes, s.bytes);
      }

      const midY = (H * dpr) / 2;

      // Center line
      ctx.strokeStyle = dark ? "rgba(255,255,255,0.1)" : "rgba(0,0,0,0.1)";
      ctx.lineWidth = dpr;
      ctx.beginPath();
      ctx.moveTo(0, midY);
      ctx.lineTo(W * dpr, midY);
      ctx.stroke();

      // Draw events: rx above center, tx below
      for (const s of samples) {
        const age = now - s.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.max(1, (s.bytes / maxBytes) * midY * 0.9);

        if (s.dir === "rx") {
          ctx.fillStyle = dark ? "rgba(100,180,255,0.7)" : "rgba(40,100,200,0.7)";
          ctx.fillRect(x, midY - barH, Math.max(1, dpr), barH);
        } else {
          ctx.fillStyle = dark ? "rgba(255,160,80,0.7)" : "rgba(200,100,20,0.7)";
          ctx.fillRect(x, midY, Math.max(1, dpr), barH);
        }
      }

      // Labels
      ctx.fillStyle = dark ? "rgba(255,255,255,0.4)" : "rgba(0,0,0,0.4)";
      ctx.font = `${9 * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillStyle = dark ? "rgba(100,180,255,0.5)" : "rgba(40,100,200,0.5)";
      ctx.fillText("rx", 2 * dpr, 2 * dpr);
      ctx.textBaseline = "bottom";
      ctx.fillStyle = dark ? "rgba(255,160,80,0.5)" : "rgba(200,100,20,0.5)";
      ctx.fillText("tx", 2 * dpr, H * dpr - 2 * dpr);
      // Max scale
      ctx.textBaseline = "top";
      ctx.textAlign = "right";
      ctx.fillStyle = dark ? "rgba(255,255,255,0.3)" : "rgba(0,0,0,0.3)";
      ctx.fillText(formatBw(maxBytes), (W - 2) * dpr, 2 * dpr);
      ctx.textAlign = "left";

      rafRef.current = requestAnimationFrame(draw);
    };
    rafRef.current = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(rafRef.current);
  }, [netRef, dark, dpr]);

  return (
    <canvas
      ref={canvasRef}
      width={W * dpr}
      height={H * dpr}
      style={{ width: W, height: H, marginTop: 2 }}
    />
  );
}
