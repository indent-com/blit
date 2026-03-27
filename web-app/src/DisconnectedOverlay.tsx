import { disconnectedStyles, themeFor } from "./theme";
import { OverlayBackdrop, OverlayPanel } from "./Overlay";

export function DisconnectedOverlay({ dark }: { dark: boolean }) {
  const theme = themeFor(dark);
  const styles = disconnectedStyles(theme, dark);

  return (
    <OverlayBackdrop
      dark={dark}
      label="Offline"
      dismissOnBackdrop={false}
      style={{ zIndex: 120 }}
    >
      <OverlayPanel dark={dark} style={styles.card}>
        <div style={styles.content}>
          <h2 style={styles.title}>Offline</h2>
          <button
            type="button"
            onClick={() => window.location.reload()}
            style={styles.reloadButton}
          >
            Reload page
          </button>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
