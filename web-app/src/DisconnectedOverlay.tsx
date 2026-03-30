import { useState, useEffect, useRef } from "react";
import type { ConnectionStatus, TerminalPalette } from "blit-react";
import { disconnectedStyles, themeFor, uiScale, z } from "./theme";
import { OverlayBackdrop, OverlayPanel } from "./Overlay";
import { t, tp } from "./i18n";

interface LogEntry {
  time: string;
  status: ConnectionStatus;
  message: string;
}

function formatTimestamp(): string {
  return new Date().toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

function formatRetryLabel(retryCount: number): string {
  return retryCount === 1 ? t("disconnected.retryOne") : tp("disconnected.retryMany", { count: retryCount });
}

function describeStatusTransition(
  status: ConnectionStatus,
  retryCount: number,
  previousStatus: ConnectionStatus | null,
): string {
  switch (status) {
    case "connecting":
      return retryCount > 0
        ? tp("disconnected.retryStarted", { count: retryCount })
        : t("disconnected.connectingToServer");
    case "authenticating":
      return retryCount > 0
        ? tp("disconnected.retryAuth", { count: retryCount })
        : t("disconnected.authenticatingWithServer");
    case "connected":
      return retryCount > 0
        ? tp("disconnected.connectionRestoredAfter", { retries: formatRetryLabel(retryCount) })
        : t("disconnected.connectionRestored");
    case "error":
      if (previousStatus === "authenticating") {
        return retryCount > 0
          ? tp("disconnected.retryAuthFailed", { count: retryCount })
          : t("disconnected.authFailed");
      }
      return retryCount > 0
        ? tp("disconnected.retryFailed", { count: retryCount })
        : t("disconnected.connectionFailed");
    case "disconnected":
      return previousStatus === "connected"
        ? t("disconnected.connectionLost")
        : retryCount > 0
          ? tp("disconnected.retryDisconnected", { count: retryCount })
          : t("disconnected.disconnectedFromServer");
  }
}

export function DisconnectedOverlay({
  palette,
  fontSize,
  status,
  retryCount,
  error,
  onReconnect,
}: {
  palette: TerminalPalette;
  fontSize: number;
  status: ConnectionStatus;
  retryCount: number;
  error: string | null;
  onReconnect: () => void;
}) {
  const dark = palette.dark;
  const theme = themeFor(palette);
  const scale = uiScale(fontSize);
  const styles = disconnectedStyles(theme, dark, scale);
  const [log, setLog] = useState<LogEntry[]>([]);
  const logRef = useRef<HTMLDivElement>(null);
  const previousStatusRef = useRef<ConnectionStatus | null>(null);

  useEffect(() => {
    const previousStatus = previousStatusRef.current;
    if (previousStatus === status) return;

    previousStatusRef.current = status;
    setLog((prev) => [
      ...prev.slice(-23),
      {
        time: formatTimestamp(),
        status,
        message: describeStatusTransition(status, retryCount, previousStatus),
      },
    ]);
  }, [status, retryCount]);

  useEffect(() => {
    logRef.current?.scrollTo(0, logRef.current.scrollHeight);
  }, [log]);

  const statusText =
    status === "connecting"
      ? t("status.connecting")
      : status === "authenticating"
        ? t("status.authenticating")
        : status === "error"
          ? (error ?? t("status.connectionFailed"))
          : t("status.disconnected");

  const handleReconnect = () => {
    setLog((prev) => [
      ...prev.slice(-23),
      {
        time: formatTimestamp(),
        status,
        message: t("disconnected.manualReconnect"),
      },
    ]);
    onReconnect();
  };

  return (
    <OverlayBackdrop
      palette={palette}
      label={t("disconnected.label")}
      dismissOnBackdrop={false}
      style={{ zIndex: z.disconnected }}
    >
      <OverlayPanel palette={palette} fontSize={fontSize} style={styles.card}>
        <div style={styles.content}>
          <h2 style={styles.title}>{statusText}</h2>
          <div
            style={{
              width: "100%",
              display: "grid",
              gridTemplateColumns: "repeat(2, minmax(0, 1fr))",
              gap: 8,
            }}
          >
            <div
              style={{
                border: `1px solid ${theme.subtleBorder}`,
                padding: "0.7em 0.8em",
                background: theme.inputBg,
              }}
            >
              <div style={{ fontSize: "0.72em", opacity: 0.68, marginBottom: 4 }}>
                {t("disconnected.statusLabel")}
              </div>
              <div style={{ fontSize: "0.95em", fontWeight: 600 }}>{statusText}</div>
            </div>
            <div
              style={{
                border: `1px solid ${theme.subtleBorder}`,
                padding: "0.7em 0.8em",
                background: theme.inputBg,
              }}
            >
              <div style={{ fontSize: "0.72em", opacity: 0.68, marginBottom: 4 }}>
                {t("disconnected.retriesLabel")}
              </div>
              <div style={{ fontSize: "0.95em", fontWeight: 600 }}>{retryCount}</div>
            </div>
          </div>
          {log.length > 0 ? (
            <div
              style={{
                width: "100%",
                display: "grid",
                gap: 6,
              }}
            >
              <div style={{ fontSize: "0.72em", opacity: 0.68 }}>{t("disconnected.activityLabel")}</div>
              <div
                ref={logRef}
                style={{
                  width: "100%",
                  maxHeight: "9.5em",
                  overflow: "auto",
                  fontSize: "0.75em",
                  fontFamily: "ui-monospace, monospace",
                  lineHeight: 1.6,
                  border: `1px solid ${theme.subtleBorder}`,
                  background: theme.inputBg,
                  padding: "0.65em 0.8em",
                }}
              >
                {log.map((entry, index) => (
                  <div
                    key={`${entry.time}-${index}`}
                    style={{ display: "grid", gridTemplateColumns: "auto 1fr", gap: "0.8em" }}
                  >
                    <span style={{ opacity: 0.56 }}>{entry.time}</span>
                    <span>{entry.message}</span>
                  </div>
                ))}
              </div>
            </div>
          ) : null}
          <div style={{ display: "flex", gap: 8 }}>
            <button
              type="button"
              onClick={handleReconnect}
              style={styles.reloadButton}
              disabled={status === "connecting" || status === "authenticating"}
            >
              {t("disconnected.reconnectNow")}
            </button>
            <button
              type="button"
              onClick={() => window.location.reload()}
              style={{ ...styles.reloadButton, opacity: 0.5 }}
            >
              {t("disconnected.reloadPage")}
            </button>
          </div>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
