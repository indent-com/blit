import {
  CLIENT_SUBSCRIPTION_AUDIO,
  CLIENT_SUBSCRIPTION_FS,
  CLIENT_SUBSCRIPTION_GIT,
  CLIENT_SUBSCRIPTION_KV,
  CLIENT_SUBSCRIPTION_LSP,
  CLIENT_SUBSCRIPTION_NET,
} from "@blit-sh/core";

export function formatTerminalViewSize(
  cols: number | null,
  rows: number | null,
): string {
  return cols == null || rows == null ? "size not reported" : `${cols}×${rows}`;
}

export function formatSurfaceViewSize(
  width: number | null,
  height: number | null,
  scale120: number | null,
): string {
  if (width == null || height == null) return "size not reported";
  if (scale120 == null) return `${width}×${height}`;
  // Round to 2dp and drop trailing fraction zeros. Chaining .replace(/0$/)
  // after stripping ".00" would also eat a zero off the integer part, turning
  // a 10× scale into "1×".
  const scale = String(Number((scale120 / 120).toFixed(2)));
  return `${width}×${height} @ ${scale}×`;
}

export function formatClientAge(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ${seconds % 60}s`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `${days}d ${hours % 24}h`;
}

export function formatClientBandwidth(bytesPerSecond: number): string {
  const value = Math.max(0, bytesPerSecond);
  if (value < 1_000) return `${Math.round(value)} B/s`;
  if (value < 1_000_000) return `${formatRate(value / 1_000)} kB/s`;
  if (value < 1_000_000_000) return `${formatRate(value / 1_000_000)} MB/s`;
  return `${formatRate(value / 1_000_000_000)} GB/s`;
}

function formatRate(value: number): string {
  return value >= 100 ? value.toFixed(0) : value.toFixed(1).replace(/\.0$/, "");
}

export function formatClientSubscription(kind: number, id: number): string {
  switch (kind) {
    case CLIENT_SUBSCRIPTION_AUDIO:
      return "Audio";
    case CLIENT_SUBSCRIPTION_FS:
      return `Filesystem #${id}`;
    case CLIENT_SUBSCRIPTION_GIT:
      return `Git #${id}`;
    case CLIENT_SUBSCRIPTION_LSP:
      return `LSP #${id}`;
    case CLIENT_SUBSCRIPTION_KV:
      return `KV #${id}`;
    case CLIENT_SUBSCRIPTION_NET:
      return `Network #${id}`;
    default:
      return `Unknown ${kind} #${id}`;
  }
}
