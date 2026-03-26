import type { UseBlitSessionsReturn, TerminalPalette } from "blit-react";
import { formatBw } from "./useMetrics";
import type { Metrics } from "./useMetrics";
import { styles } from "./styles";

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
  const visible = sessions.sessions.filter((s) => s.state !== "closed");
  const exited = visible.filter((s) => s.state === "exited").length;
  const focused = sessions.sessions.find((s) => s.ptyId === sessions.focusedPtyId);
  return (
    <>
      <button onClick={onExpose} style={styles.statusBtn} title="Expose (Cmd+K)">
        {visible.length} PTY{visible.length !== 1 ? "s" : ""}
        {exited > 0 && <span style={{ opacity: 0.5 }}> ({exited} exited)</span>}
      </button>
      <span style={styles.statusTitle}>
        {focused && (
          <>
            {focused.title ?? `PTY ${focused.ptyId}`}
            {focused.state === "exited" && (
              <span style={{ color: "#a44", marginLeft: 6, fontSize: 11 }}>[exited]</span>
            )}
          </>
        )}
      </span>
      <span style={styles.statusMetrics}>
        {termSize && <>{termSize} &middot; </>}
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
