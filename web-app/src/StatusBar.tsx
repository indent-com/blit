import { useRef, useEffect, type RefObject } from "react";
import type {
  BlitSession,
  ConnectionStatus,
  TerminalPalette,
} from "blit-react";
import { formatBw } from "./useMetrics";
import type { Metrics, RenderSample, NetSample } from "./useMetrics";
import { themeFor, ui, z } from "./theme";

type TimelineRef = RefObject<RenderSample[]>;
type NetRef = RefObject<NetSample[]>;
type DebugStats = {
  displayFps: number;
  pendingApplied: number;
  ackAhead: number;
  applyMs: number;
  mouseMode: number;
  mouseEncoding: number;
  terminals: number;
  staleTerminals: number;
  subscribed: number;
  frozenPtys: number;
  pendingFrameQueues: number;
  totalPendingFrames: number;
} | null;

function rgba([r, g, b]: [number, number, number], alpha: number): string {
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

export function StatusBar({
  sessions,
  focusedSession,
  status,
  metrics,
  palette,
  termSize,
  fontLoading,
  debug,
  toggleDebug,
  debugStats,
  timelineRef,
  netRef,
  onExpose,
  onPalette,
  onFont,
}: {
  sessions: readonly BlitSession[];
  focusedSession: BlitSession | null;
  status: ConnectionStatus;
  metrics: Metrics;
  palette: TerminalPalette;
  termSize: string | null;
  fontLoading: boolean;
  debug: boolean;
  toggleDebug: () => void;
  debugStats: DebugStats;
  timelineRef: TimelineRef;
  netRef: NetRef;
  onExpose: () => void;
  onPalette: () => void;
  onFont: () => void;
}) {
  const theme = themeFor(palette);
  const visible = sessions.filter((session) => session.state !== "closed");
  const exited = visible.filter((session) => session.state === "exited").length;

  return (
    <>
      <button onClick={onExpose} style={ui.btn} title="Expose (Cmd+K)">
        {visible.length} PTY{visible.length !== 1 ? "s" : ""}
        {exited > 0 && <span style={{ opacity: 0.5 }}> ({exited} exited)</span>}
      </button>
      <span
        style={{
          flex: 1,
          overflow: "hidden",
          textOverflow: "ellipsis",
          whiteSpace: "nowrap",
          opacity: 0.7,
        }}
      >
        {focusedSession && (
          <>
            {focusedSession.title ?? focusedSession.tag ?? `PTY ${focusedSession.ptyId}`}
          </>
        )}
      </span>
      <span style={{ fontSize: 11, opacity: 0.5, flexShrink: 0, whiteSpace: "nowrap" }}>
        {termSize ? `${termSize}@` : ""}
        {metrics.fps}/{metrics.ups}
      </span>
      <button onClick={toggleDebug} style={{ ...ui.btn, opacity: debug ? 1 : 0.3 }} title="Debug stats">
        &#x1F41B;
      </button>
      <button onClick={onPalette} style={ui.btn} title={`Palette: ${palette.name} (Cmd+Shift+P)`}>
        {palette.dark ? "\u25D1" : "\u25D0"}
      </button>
      <button onClick={onFont} style={ui.btn} title="Font (Cmd+Shift+F)">
        {fontLoading ? <span style={{ opacity: 0.5, fontSize: 10 }}>Loading font…</span> : "Aa"}
      </button>
      <span
        role="status"
        aria-label={status}
        title={status}
        style={{
          width: 6,
          height: 6,
          borderRadius: "50%",
          flexShrink: 0,
          backgroundColor:
            status === "connected"
              ? theme.success
              : status === "connecting" || status === "authenticating"
                ? theme.warning
                : theme.error,
        }}
      />
      {debug && (
        <DebugPanel
          metrics={metrics}
          debugStats={debugStats}
          palette={palette}
          timelineRef={timelineRef}
          netRef={netRef}
        />
      )}
    </>
  );
}

function DebugPanel({
  metrics,
  debugStats,
  palette,
  timelineRef,
  netRef,
}: {
  metrics: Metrics;
  debugStats: DebugStats;
  palette: TerminalPalette;
  timelineRef: TimelineRef;
  netRef: NetRef;
}) {
  const stats = debugStats ?? {
    displayFps: 0,
    pendingApplied: 0,
    ackAhead: 0,
    applyMs: 0,
    mouseMode: 0,
    mouseEncoding: 0,
    terminals: 0,
    staleTerminals: 0,
    subscribed: 0,
    frozenPtys: 0,
    pendingFrameQueues: 0,
    totalPendingFrames: 0,
  };
  const theme = themeFor(palette);
  const dark = palette.dark;

  return (
    <div
      style={{
        position: "fixed",
        top: 0,
        right: 0,
        backgroundColor: rgba(palette.bg, dark ? 0.78 : 0.84),
        backdropFilter: "blur(6px)",
        WebkitBackdropFilter: "blur(6px)",
        color: theme.fg,
        borderLeft: `1px solid ${theme.subtleBorder}`,
        borderBottom: `1px solid ${theme.subtleBorder}`,
        padding: "6px 10px",
        fontSize: 11,
        fontFamily: "ui-monospace, monospace",
        lineHeight: 1.6,
        zIndex: z.debugPanel,
        whiteSpace: "pre",
        pointerEvents: "none",
        minWidth: 260,
      }}
    >
      <Row label="FPS / UPS" value={`${metrics.fps} / ${metrics.ups}`} />
      <Row label="Bandwidth" value={formatBw(metrics.bw)} />
      <Row label="Render" value={`${metrics.renderMs.toFixed(1)} ms avg, ${metrics.maxRenderMs.toFixed(1)} ms max`} />
      <Row label="Display Hz" value={stats.displayFps} />
      <Row label="Backlog" value={stats.pendingApplied} />
      <Row label="Ack ahead" value={stats.ackAhead} />
      <Row label="Apply" value={`${stats.applyMs.toFixed(1)} ms`} />
      <Row label="Mouse" value={`mode=${stats.mouseMode} enc=${stats.mouseEncoding}`} />
      <Row label="Queued" value={`${stats.totalPendingFrames} frames in ${stats.pendingFrameQueues} queues`} />
      <Row label="Terminals" value={`${stats.terminals} live, ${stats.staleTerminals} stale, ${stats.frozenPtys} frozen`} />
      <div style={{ borderTop: `1px solid ${theme.subtleBorder}`, marginTop: 4, paddingTop: 2 }}>
        <span style={{ opacity: 0.6, fontSize: 10 }}>Render</span>
        <RenderTimeline timelineRef={timelineRef} palette={palette} displayFps={stats.displayFps} />
      </div>
      <div style={{ borderTop: `1px solid ${theme.subtleBorder}`, marginTop: 4, paddingTop: 2 }}>
        <span style={{ opacity: 0.6, fontSize: 10 }}>Network</span>
        <NetTimeline netRef={netRef} palette={palette} />
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

function RenderTimeline({
  timelineRef,
  palette,
  displayFps,
}: {
  timelineRef: TimelineRef;
  palette: TerminalPalette;
  displayFps: number;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rafRef = useRef(0);
  const W = 300;
  const H = 80;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;
  const fg = palette.fg;
  const success = palette.ansi[2] ?? palette.fg;
  const warning = palette.ansi[3] ?? palette.fg;
  const error = palette.ansi[1] ?? palette.fg;

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

      ctx.strokeStyle = rgba(error, 0.45);
      ctx.lineWidth = dpr;
      const budgetY = (1 - budgetMs / maxMs) * H * dpr;
      ctx.beginPath();
      ctx.moveTo(0, budgetY);
      ctx.lineTo(W * dpr, budgetY);
      ctx.stroke();

      for (const sample of samples) {
        const age = now - sample.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.min(sample.ms / maxMs, 1) * H * dpr;
        const y = H * dpr - barH;

        if (sample.ms < budgetMs) ctx.fillStyle = rgba(success, 0.82);
        else if (sample.ms < budgetMs * 2) ctx.fillStyle = rgba(warning, 0.82);
        else ctx.fillStyle = rgba(error, 0.82);

        ctx.fillRect(x, y, Math.max(1, dpr), barH);
      }

      ctx.fillStyle = rgba(fg, 0.45);
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
  }, [displayFps, dpr, error, fg, success, timelineRef, warning]);

  return (
    <canvas
      ref={canvasRef}
      width={W * dpr}
      height={H * dpr}
      style={{ width: W, height: H, marginTop: 2 }}
    />
  );
}

function NetTimeline({
  netRef,
  palette,
}: {
  netRef: NetRef;
  palette: TerminalPalette;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rafRef = useRef(0);
  const W = 300;
  const H = 50;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;
  const fg = palette.fg;
  const rx = palette.ansi[12] ?? palette.ansi[6] ?? palette.fg;
  const tx = palette.ansi[11] ?? palette.ansi[3] ?? palette.fg;

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
      let maxBytes = 256;
      for (const sample of samples) {
        if (now - sample.t <= windowMs) maxBytes = Math.max(maxBytes, sample.bytes);
      }

      const midY = (H * dpr) / 2;

      ctx.strokeStyle = rgba(fg, 0.12);
      ctx.lineWidth = dpr;
      ctx.beginPath();
      ctx.moveTo(0, midY);
      ctx.lineTo(W * dpr, midY);
      ctx.stroke();

      for (const sample of samples) {
        const age = now - sample.t;
        if (age > windowMs || age < 0) continue;
        const x = ((windowMs - age) / windowMs) * W * dpr;
        const barH = Math.min(sample.bytes / maxBytes, 1) * (H * dpr * 0.45);
        const y = sample.dir === "rx" ? midY - barH : midY;
        ctx.fillStyle = sample.dir === "rx"
          ? rgba(rx, 0.82)
          : rgba(tx, 0.82);
        ctx.fillRect(x, y, Math.max(1, dpr), barH);
      }

      ctx.fillStyle = rgba(fg, 0.45);
      ctx.font = `${9 * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(formatBw(maxBytes).replace("/s", ""), 2 * dpr, 2 * dpr);
      ctx.textBaseline = "bottom";
      ctx.fillText("rx", 2 * dpr, midY - 2 * dpr);
      ctx.fillText("tx", 2 * dpr, H * dpr - 2 * dpr);

      rafRef.current = requestAnimationFrame(draw);
    };
    rafRef.current = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(rafRef.current);
  }, [dpr, fg, netRef, rx, tx]);

  return (
    <canvas
      ref={canvasRef}
      width={W * dpr}
      height={H * dpr}
      style={{ width: W, height: H, marginTop: 2 }}
    />
  );
}
