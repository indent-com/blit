import { Show, For, type JSX } from "solid-js";
import { onMount, onCleanup } from "solid-js";
import type {
  BlitSession,
  ConnectionStatus,
  TerminalPalette,
} from "@blit-sh/core";
import { formatBw } from "./createMetrics";
import type { Metrics, RenderSample, NetSample } from "./createMetrics";
import { sessionName, themeFor, ui, uiScale, z } from "./theme";
import type { Theme } from "./theme";
import { t, tp } from "./i18n";

type SurfaceDebugInfo = {
  surfaceId: number;
  codec: string;
  width: number;
  height: number;
};

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
  surfaces?: SurfaceDebugInfo[];
} | null;

function rgba([r, g, b]: [number, number, number], alpha: number): string {
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

export function statusBarBg(status: ConnectionStatus, theme: Theme): string {
  if (status === "connected") return theme.bg;
  if (status === "connecting" || status === "authenticating")
    return theme.warning;
  return theme.error;
}

export function statusBarFg(status: ConnectionStatus, theme: Theme): string {
  if (status === "connected") return theme.fg;
  return theme.bg;
}

function retryLabel(retryCount: number): string {
  return retryCount === 1
    ? t("disconnected.retryOne")
    : tp("disconnected.retryMany", { count: retryCount });
}

function statusText(
  status: ConnectionStatus,
  retryCount: number,
  error: string | null,
): string | null {
  if (status === "connected") return null;

  let base: string;
  switch (status) {
    case "connecting":
      base = t("status.connecting");
      break;
    case "authenticating":
      base = t("status.authenticating");
      break;
    case "disconnected":
    case "closed":
      base = t("status.disconnected");
      break;
    case "error":
      base = t("status.connectionFailed");
      break;
  }

  const parts: string[] = [];
  if (retryCount > 0) parts.push(retryLabel(retryCount));
  if (error && error !== "auth") parts.push(error);
  return parts.length > 0 ? `${base} (${parts.join(" \u2014 ")})` : base;
}

export function StatusBar(props: {
  sessions: readonly BlitSession[];
  surfaceCount: number;
  focusedSession: BlitSession | null;
  status: ConnectionStatus;
  retryCount: number;
  error: string | null;
  onReconnect: () => void;
  metrics: Metrics;
  palette: TerminalPalette;
  fontSize: number;
  termSize: string | null;
  fontLoading: boolean;
  debug: boolean;
  toggleDebug: () => void;
  previewPanelOpen: boolean;
  onPreviewPanel: () => void;
  debugStats: DebugStats;
  timeline: RenderSample[];
  net: NetSample[];
  onSwitcher: () => void;
  onPalette: () => void;
  onFont: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);
  const connectionLabel = () =>
    statusText(props.status, props.retryCount, props.error);
  const visible = () =>
    props.sessions.filter((session) => session.state !== "closed");
  const exited = () =>
    visible().filter((session) => session.state === "exited").length;
  const buttonStyle = (): JSX.CSSProperties => ({
    ...ui.btn,
    "font-size": `${scale().sm}px`,
  });

  return (
    <>
      <button
        onClick={props.onSwitcher}
        style={buttonStyle()}
        title={t("statusbar.menuTitle")}
      >
        {visible().length === 1
          ? tp("statusbar.terminalOne", { count: 1 })
          : tp("statusbar.terminalMany", { count: visible().length })}
        <Show when={props.surfaceCount > 0}>
          <span style={{ opacity: 0.7 }}>
            {" \u00B7 "}
            {props.surfaceCount === 1
              ? tp("statusbar.surfaceOne", { count: 1 })
              : tp("statusbar.surfaceMany", { count: props.surfaceCount })}
          </span>
        </Show>
        <Show when={exited() > 0}>
          <span style={{ opacity: 0.5 }}>
            {" "}
            {tp("statusbar.exited", { count: exited() })}
          </span>
        </Show>
      </button>
      <span
        style={{
          flex: 1,
          overflow: "hidden",
          "text-overflow": "ellipsis",
          "white-space": "nowrap",
          opacity: 0.7,
        }}
      >
        <Show when={props.focusedSession}>
          {(session) => <>{sessionName(session())}</>}
        </Show>
      </span>
      <span
        style={{
          "font-size": `${scale().xs}px`,
          opacity: 0.5,
          "flex-shrink": 0,
          "white-space": "nowrap",
        }}
      >
        {props.termSize ? `${props.termSize}@` : ""}
        {props.metrics.fps}/{props.metrics.ups}
      </span>
      <button
        onClick={props.toggleDebug}
        style={{ ...buttonStyle(), opacity: props.debug ? 1 : 0.3 }}
        title={t("statusbar.debugStats")}
      >
        {"\u{1F41B}"}
      </button>
      <button
        onClick={props.onPreviewPanel}
        style={{
          ...buttonStyle(),
          opacity: props.previewPanelOpen ? 1 : 0.3,
        }}
        title={t("statusbar.previewPanel")}
      >
        {"\u25EB"}
      </button>
      <button
        onClick={props.onPalette}
        style={buttonStyle()}
        title={tp("statusbar.paletteTitle", { name: props.palette.name })}
      >
        {props.palette.dark ? "\u25D1" : "\u25D0"}
      </button>
      <button
        onClick={props.onFont}
        style={buttonStyle()}
        title={t("statusbar.fontTitle")}
      >
        <Show
          when={!props.fontLoading}
          fallback={
            <span
              style={{
                opacity: 0.5,
                "font-size": `${scale().xs}px`,
              }}
            >
              {t("statusbar.loadingFont")}
            </span>
          }
        >
          Aa
        </Show>
      </button>
      <Show when={connectionLabel()}>
        {(label) => (
          <span
            role="status"
            aria-label={props.status}
            style={{
              "font-size": `${scale().xs}px`,
              opacity: 0.85,
              "flex-shrink": 0,
              "white-space": "nowrap",
            }}
          >
            {label()}
          </span>
        )}
      </Show>
      <Show when={connectionLabel()}>
        <button
          onClick={props.onReconnect}
          style={{
            ...buttonStyle(),
            "font-size": `${scale().xs}px`,
            opacity: 0.85,
          }}
        >
          {t("disconnected.reconnectNow")}
        </button>
      </Show>
      <Show when={props.debug}>
        <DebugPanel
          metrics={props.metrics}
          debugStats={props.debugStats}
          palette={props.palette}
          fontSize={props.fontSize}
          timeline={props.timeline}
          net={props.net}
        />
      </Show>
    </>
  );
}

function DebugPanel(props: {
  metrics: Metrics;
  debugStats: DebugStats;
  palette: TerminalPalette;
  fontSize: number;
  timeline: RenderSample[];
  net: NetSample[];
}) {
  const stats = () =>
    props.debugStats ?? {
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
  const theme = () => themeFor(props.palette);
  const dark = () => props.palette.dark;
  const scale = () => uiScale(props.fontSize);

  return (
    <div
      style={{
        position: "fixed",
        top: 0,
        right: 0,
        "background-color": rgba(props.palette.bg, dark() ? 0.78 : 0.84),
        "backdrop-filter": "blur(6px)",
        "-webkit-backdrop-filter": "blur(6px)",
        color: theme().fg,
        "border-left": `1px solid ${theme().subtleBorder}`,
        "border-bottom": `1px solid ${theme().subtleBorder}`,
        padding: "0.4em 0.7em",
        "font-size": `${scale().sm}px`,
        "font-family": "ui-monospace, monospace",
        "line-height": 1.6,
        "z-index": z.debugPanel,
        "white-space": "pre",
        "pointer-events": "none",
      }}
    >
      <Row
        label="FPS / UPS"
        value={`${props.metrics.fps} / ${props.metrics.ups}`}
      />
      <Row label="Bandwidth" value={formatBw(props.metrics.bw)} />
      <Row
        label="Render"
        value={`${props.metrics.renderMs.toFixed(1)} ms avg, ${props.metrics.maxRenderMs.toFixed(1)} ms max`}
      />
      <Row label="Display Hz" value={stats().displayFps} />
      <Row label="Backlog" value={stats().pendingApplied} />
      <Row label="Ack ahead" value={stats().ackAhead} />
      <Row label="Apply" value={`${stats().applyMs.toFixed(1)} ms`} />
      <Row
        label="Mouse"
        value={`mode=${stats().mouseMode} enc=${stats().mouseEncoding}`}
      />
      <Row
        label="Queued"
        value={`${stats().totalPendingFrames} frames in ${stats().pendingFrameQueues} queues`}
      />
      <Row
        label="Terminals"
        value={`${stats().terminals} live, ${stats().staleTerminals} stale, ${stats().frozenPtys} frozen`}
      />
      <Show when={(stats().surfaces?.length ?? 0) > 0}>
        <For each={stats().surfaces}>
          {(s) => (
            <Row
              label={`Surface ${s.surfaceId}`}
              value={`${s.codec} ${s.width}x${s.height}`}
            />
          )}
        </For>
      </Show>
      <div
        style={{
          "border-top": `1px solid ${theme().subtleBorder}`,
          "margin-top": "4px",
          "padding-top": "2px",
        }}
      >
        <span style={{ opacity: 0.6, "font-size": `${scale().xs}px` }}>
          Render
        </span>
        <RenderTimeline
          timeline={props.timeline}
          palette={props.palette}
          displayFps={stats().displayFps}
          fontSize={scale().xs}
        />
      </div>
      <div
        style={{
          "border-top": `1px solid ${theme().subtleBorder}`,
          "margin-top": "4px",
          "padding-top": "2px",
        }}
      >
        <span style={{ opacity: 0.6, "font-size": `${scale().xs}px` }}>
          Network
        </span>
        <NetTimeline
          net={props.net}
          palette={props.palette}
          fontSize={scale().xs}
        />
      </div>
    </div>
  );
}

function Row(props: { label: string; value: string | number }) {
  return (
    <div
      style={{
        display: "flex",
        "justify-content": "space-between",
        gap: "1em",
      }}
    >
      <span style={{ opacity: 0.6 }}>{props.label}</span>
      <span>{props.value}</span>
    </div>
  );
}

function RenderTimeline(props: {
  timeline: RenderSample[];
  palette: TerminalPalette;
  displayFps: number;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let raf = 0;
  const W = 300;
  const H = 80;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  onMount(() => {
    const draw = () => {
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.timeline;
      if (!samples || samples.length < 2) {
        raf = requestAnimationFrame(draw);
        return;
      }

      const fg = props.palette.fg;
      const success = props.palette.ansi[2] ?? props.palette.fg;
      const warning = props.palette.ansi[3] ?? props.palette.fg;
      const error = props.palette.ansi[1] ?? props.palette.fg;

      const now = performance.now();
      const windowMs = 2000;
      const maxMs = 20;
      const budgetMs = props.displayFps > 0 ? 1000 / props.displayFps : 16.67;

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
      ctx.font = `${props.fontSize * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(`${maxMs}ms`, 2 * dpr, 2 * dpr);
      ctx.textAlign = "right";
      ctx.fillText(
        `budget ${budgetMs.toFixed(1)}ms`,
        (W - 2) * dpr,
        budgetY - 10 * dpr,
      );
      ctx.textAlign = "left";
      ctx.textBaseline = "bottom";
      ctx.fillText("0ms", 2 * dpr, H * dpr - 2 * dpr);

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    onCleanup(() => cancelAnimationFrame(raf));
  });

  return (
    <canvas
      ref={canvas}
      width={W * dpr}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}

function NetTimeline(props: {
  net: NetSample[];
  palette: TerminalPalette;
  fontSize: number;
}) {
  let canvas!: HTMLCanvasElement;
  let raf = 0;
  const W = 300;
  const H = 50;
  const dpr = typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1;

  onMount(() => {
    const draw = () => {
      const ctx = canvas.getContext("2d");
      if (!ctx) return;
      ctx.clearRect(0, 0, W * dpr, H * dpr);

      const samples = props.net;
      if (!samples || samples.length === 0) {
        raf = requestAnimationFrame(draw);
        return;
      }

      const fg = props.palette.fg;
      const rx =
        props.palette.ansi[12] ?? props.palette.ansi[6] ?? props.palette.fg;
      const tx =
        props.palette.ansi[11] ?? props.palette.ansi[3] ?? props.palette.fg;

      const now = performance.now();
      const windowMs = 2000;
      let maxBytes = 256;
      for (const sample of samples) {
        if (now - sample.t <= windowMs)
          maxBytes = Math.max(maxBytes, sample.bytes);
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
        ctx.fillStyle = sample.dir === "rx" ? rgba(rx, 0.82) : rgba(tx, 0.82);
        ctx.fillRect(x, y, Math.max(1, dpr), barH);
      }

      ctx.fillStyle = rgba(fg, 0.45);
      ctx.font = `${props.fontSize * dpr}px ui-monospace, monospace`;
      ctx.textBaseline = "top";
      ctx.fillText(formatBw(maxBytes).replace("/s", ""), 2 * dpr, 2 * dpr);
      ctx.textBaseline = "bottom";
      ctx.fillText("rx", 2 * dpr, midY - 2 * dpr);
      ctx.fillText("tx", 2 * dpr, H * dpr - 2 * dpr);

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    onCleanup(() => cancelAnimationFrame(raf));
  });

  return (
    <canvas
      ref={canvas}
      width={W * dpr}
      height={H * dpr}
      style={{ width: `${W}px`, height: `${H}px`, "margin-top": "2px" }}
    />
  );
}
