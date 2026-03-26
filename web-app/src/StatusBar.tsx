import type { UseBlitSessionsReturn, TerminalPalette } from "blit-react";
import { formatBw } from "./useMetrics";
import type { Metrics } from "./useMetrics";
import { styles } from "./styles";

export function StatusBar({
  sessions,
  metrics,
  palette,
  onExpose,
  onPalette,
  onFont,
}: {
  sessions: UseBlitSessionsReturn;
  metrics: Metrics;
  palette: TerminalPalette;
  onExpose: () => void;
  onPalette: () => void;
  onFont: () => void;
}) {
  const active = sessions.sessions.filter((s) => s.state !== "closed");
  return (
    <>
      <button onClick={onExpose} style={styles.statusBtn} title="Expose (Cmd+K)">
        {active.length} PTY{active.length !== 1 ? "s" : ""}
      </button>
      <span style={styles.statusTitle}>
        {sessions.focusedPtyId != null &&
          (sessions.sessions.find(
            (s) => s.ptyId === sessions.focusedPtyId,
          )?.title ??
            `PTY ${sessions.focusedPtyId}`)}
      </span>
      <span style={styles.statusMetrics}>
        {formatBw(metrics.bw)} &middot; {metrics.ups} UPS &middot; {metrics.fps} FPS
      </span>
      <button onClick={onPalette} style={styles.statusBtn} title="Palette (Cmd+Shift+P)">
        <span style={{
          ...styles.swatch,
          backgroundColor: `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`,
          border: "1px solid rgba(128,128,128,0.3)",
          verticalAlign: "middle",
        }} />
      </button>
      <button onClick={onFont} style={styles.statusBtn} title="Font (Cmd+Shift+F)">
        Aa
      </button>
      <span
        role="status"
        aria-label={sessions.status}
        style={{
          ...styles.statusDot,
          backgroundColor: sessions.status === "connected" ? "#4a4" : "#a44",
        }}
      />
    </>
  );
}
