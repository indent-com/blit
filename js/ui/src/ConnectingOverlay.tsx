import { For, Show, onCleanup, onMount } from "solid-js";
import type { BlitConnectionSnapshot, TerminalPalette } from "@blit-sh/core";
import { themeFor, uiScale, ui } from "./theme";
import { t } from "./i18n";

function rgba([r, g, b]: [number, number, number], alpha: number): string {
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}

function Spinner(props: { color: string; size: number }) {
  return (
    <span
      style={{
        display: "inline-block",
        width: `${props.size}px`,
        height: `${props.size}px`,
        border: `${Math.max(1, Math.round(props.size / 8))}px solid ${props.color}`,
        "border-top-color": "transparent",
        "border-radius": "50%",
        animation: "blit-spin 0.7s linear infinite",
        "flex-shrink": 0,
      }}
    />
  );
}

function StatusDot(props: {
  status: "pending" | "busy" | "ok" | "error";
  color: string;
  size: number;
}) {
  return (
    <Show
      when={props.status === "busy"}
      fallback={
        <span
          style={{
            display: "inline-block",
            width: `${props.size}px`,
            height: `${props.size}px`,
            "border-radius": "50%",
            background:
              props.status === "ok"
                ? props.color
                : props.status === "error"
                  ? props.color
                  : "transparent",
            border:
              props.status === "pending"
                ? `${Math.max(1, Math.round(props.size / 8))}px solid ${props.color}`
                : "none",
            opacity: props.status === "pending" ? 0.4 : 1,
            "flex-shrink": 0,
          }}
        />
      }
    >
      <Spinner color={props.color} size={props.size} />
    </Show>
  );
}

export function ConnectingOverlay(props: {
  gatewayStatus: "connecting" | "connected" | "unavailable";
  connections: readonly BlitConnectionSnapshot[];
  connectionLabels?: Map<string, string>;
  palette: TerminalPalette;
  fontSize: number;
  onDismiss: () => void;
  onReconnect?: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const dark = () => props.palette.dark;
  const scale = () => uiScale(props.fontSize);

  const dotSize = () => Math.round(scale().sm * 0.75);

  const gatewayRowStatus = (): "busy" | "ok" | "error" => {
    if (props.gatewayStatus === "connected") return "ok";
    if (props.gatewayStatus === "connecting") return "busy";
    return "error";
  };

  const gatewayDotColor = () => {
    if (props.gatewayStatus === "connected") return theme().success;
    if (props.gatewayStatus === "connecting") return theme().warning;
    return theme().error;
  };

  const connRowStatus = (
    c: BlitConnectionSnapshot,
  ): "pending" | "busy" | "ok" | "error" => {
    if (c.status === "connected") return "ok";
    if (c.status === "connecting" || c.status === "authenticating")
      return "busy";
    if (c.status === "error" || c.status === "closed") return "error";
    // "disconnected" while gateway still connecting → not started yet
    if (props.gatewayStatus !== "connected") return "pending";
    return "busy";
  };

  const connDotColor = (c: BlitConnectionSnapshot) => {
    const s = connRowStatus(c);
    if (s === "ok") return theme().success;
    if (s === "error") return theme().error;
    if (s === "pending") return theme().dimFg;
    return theme().warning;
  };

  const connLabel = (c: BlitConnectionSnapshot) =>
    props.connectionLabels?.get(c.id) ?? c.id;

  const connStatusText = (c: BlitConnectionSnapshot) => {
    switch (c.status) {
      case "connected":
        return t("remotes.status.connected");
      case "connecting":
        return t("status.connecting");
      case "authenticating":
        return t("status.authenticating");
      case "error":
        return t("status.connectionFailed");
      default: {
        const rs = connRowStatus(c);
        if (rs === "pending") return "Waiting…";
        return t("status.connecting");
      }
    }
  };

  const rowStyle = () => ({
    display: "flex",
    "align-items": "center",
    gap: `${scale().gap}px`,
    padding: `${scale().tightGap + 1}px 0`,
  });

  const labelStyle = () => ({
    flex: 1,
    overflow: "hidden",
    "text-overflow": "ellipsis",
    "white-space": "nowrap" as const,
    "font-weight": "500",
  });

  const statusTextStyle = () => ({
    "font-size": `${scale().xs}px`,
    color: theme().dimFg,
    "white-space": "nowrap" as const,
    "flex-shrink": 0,
  });

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.stopPropagation();
        props.onDismiss();
      }
    };
    window.addEventListener("keydown", onKey, { capture: true });
    onCleanup(() =>
      window.removeEventListener("keydown", onKey, { capture: true }),
    );
  });

  return (
    <>
      {/* Inject keyframes once */}
      <style>{`@keyframes blit-spin { to { transform: rotate(360deg); } }`}</style>

      <div
        style={{
          position: "absolute",
          inset: 0,
          display: "flex",
          "align-items": "center",
          "justify-content": "center",
          "pointer-events": "none",
          "z-index": 5,
        }}
      >
        <div
          style={{
            "background-color": dark()
              ? rgba(props.palette.bg, 0.92)
              : rgba(props.palette.bg, 0.96),
            "backdrop-filter": "blur(6px)",
            "-webkit-backdrop-filter": "blur(6px)",
            border: `1px solid ${theme().border}`,
            "box-shadow": dark()
              ? "0 8px 32px rgba(0,0,0,0.5)"
              : "0 8px 32px rgba(0,0,0,0.12)",
            padding: `${scale().panelPadding}px ${scale().panelPadding + scale().gap}px`,
            "min-width": "min(18em, calc(100vw - 3em))",
            "max-width": "calc(100vw - 3em)",
            color: theme().fg,
            "font-size": `${scale().sm}px`,
            "pointer-events": "auto",
          }}
        >
          {/* Header row with dismiss button */}
          <div
            style={{
              display: "flex",
              "align-items": "center",
              "justify-content": "flex-end",
              "margin-bottom": `${scale().tightGap}px`,
            }}
          >
            <button
              onClick={props.onDismiss}
              title="Dismiss (Esc)"
              style={{
                ...ui.btn,
                "font-size": `${scale().xs}px`,
                padding: `0 ${scale().tightGap}px`,
                opacity: 0.5,
              }}
            >
              {"\u00D7"}
            </button>
          </div>

          {/* Gateway row */}
          <div style={rowStyle()}>
            <StatusDot
              status={gatewayRowStatus()}
              color={gatewayDotColor()}
              size={dotSize()}
            />
            <span style={labelStyle()}>Gateway</span>
            <span style={statusTextStyle()}>
              {props.gatewayStatus === "connected"
                ? t("remotes.status.connected")
                : props.gatewayStatus === "connecting"
                  ? t("status.connecting")
                  : t("status.connectionFailed")}
            </span>
          </div>

          {/* Per-connection rows */}
          <For each={props.connections as BlitConnectionSnapshot[]}>
            {(conn) => (
              <div
                style={{
                  ...rowStyle(),
                  opacity: connRowStatus(conn) === "pending" ? 0.5 : 1,
                }}
              >
                <StatusDot
                  status={connRowStatus(conn)}
                  color={connDotColor(conn)}
                  size={dotSize()}
                />
                <span style={labelStyle()}>{connLabel(conn)}</span>
                <span style={statusTextStyle()}>{connStatusText(conn)}</span>
              </div>
            )}
          </For>
        </div>
      </div>
    </>
  );
}
