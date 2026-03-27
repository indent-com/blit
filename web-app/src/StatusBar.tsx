import type { UseBlitSessionsReturn, TerminalPalette } from "blit-react";
import { formatBw } from "./useMetrics";
import type { Metrics } from "./useMetrics";
import { themeFor, ui } from "./theme";

export function StatusBar({
  sessions,
  metrics,
  palette,
  termSize,
  onExpose,
  onPalette,
  onFont,
}: {
  sessions: UseBlitSessionsReturn;
  metrics: Metrics;
  palette: TerminalPalette;
  termSize: string | null;
  onExpose: () => void;
  onPalette: () => void;
  onFont: () => void;
}) {
  const theme = themeFor(palette.dark);
  const visible = sessions.sessions.filter((s) => s.state !== "closed");
  const exited = visible.filter((s) => s.state === "exited").length;
  const focused = sessions.sessions.find((s) => s.ptyId === sessions.focusedPtyId);
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
      <span style={{
        fontSize: 11,
        opacity: 0.5,
        whiteSpace: "nowrap" as const,
        flexShrink: 0,
      }}>
        {termSize && <>{termSize} &middot; </>}
        {formatBw(metrics.bw)} &middot; {metrics.ups} UPS &middot; {metrics.fps} FPS
      </span>
      <button onClick={onPalette} style={ui.btn} title="Palette (Cmd+Shift+P)">
        <span style={{
          ...ui.swatch,
          backgroundColor: `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`,
          border: "1px solid rgba(128,128,128,0.3)",
          verticalAlign: "middle",
        }} />
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
    </>
  );
}
