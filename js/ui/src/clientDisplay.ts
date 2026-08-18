import {
  CLIENT_SUBSCRIPTION_AUDIO,
  CLIENT_SUBSCRIPTION_FS,
  CLIENT_SUBSCRIPTION_GIT,
  CLIENT_SUBSCRIPTION_KV,
  CLIENT_SUBSCRIPTION_LSP,
  CLIENT_SUBSCRIPTION_NET,
  formatExtensionId,
  type BlitClientInfo,
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

/**
 * What to call a connection in the clients list.
 *
 * An extension is named by its definition, because "Client 7" tells a reader
 * nothing about the one row in the pane they did not open themselves. An
 * unnamed transient `ext run` falls back to `id:…`, the same handle the
 * extensions panel shows and the same one `blit ext status` accepts.
 */
export function formatClientLabel(client: BlitClientInfo): string {
  const origin = client.origin;
  if (origin?.kind !== "extension") return `Client ${client.id}`;
  return origin.name || `id:${formatExtensionId(origin.extensionId)}`;
}

/** The short tag beside the label, or null for a connection that is only ever
 *  an ordinary client — most rows, which should stay quiet. */
export function formatClientOriginTag(client: BlitClientInfo): string | null {
  switch (client.origin?.kind) {
    case "extension":
      return "extension";
    // A kind this build has no name for. Saying so beats calling it a browser,
    // and beats saying nothing where the row carries a Kick button.
    case "unknown":
      return "unrecognized";
    default:
      return null;
  }
}

/**
 * Which run of the extension this connection belongs to.
 *
 * Worth showing beside the age: a definition that keeps restarting is a
 * climbing attempt number on a row whose age keeps resetting — the two
 * together say "crash loop" where either alone says "new connection".
 *
 * The task id is deliberately not here. It is a random 32-bit handle, not an
 * ordinal, so `task 4035822760` would cost more attention than it repays; it
 * belongs in {@link formatExtensionTitle}, where someone correlating this row
 * with `blit ext status` can still find it.
 */
export function formatExtensionAttempt(client: BlitClientInfo): string | null {
  const origin = client.origin;
  if (origin?.kind !== "extension") return null;
  return `attempt ${origin.attempt}`;
}

/** The coordinates that address this attempt elsewhere — `ext status`, the
 *  event stream — for the row's tooltip. */
export function formatExtensionTitle(client: BlitClientInfo): string | null {
  const origin = client.origin;
  if (origin?.kind !== "extension") return null;
  return `Extension id:${formatExtensionId(origin.extensionId)} · revision ${
    origin.definitionRevision
  } · task ${origin.taskId}`;
}

/**
 * The three states of this row's destructive button.
 *
 * Kicking an extension's connection ends the running attempt — a definition
 * with a restart policy will start another — so the button says what the click
 * does rather than leaving "Kick" to imply a peer being disconnected.
 */
export function formatKickAction(client: BlitClientInfo): {
  idle: string;
  confirm: string;
  busy: string;
} {
  if (client.origin?.kind === "extension") {
    return {
      idle: "Stop attempt",
      confirm: "Confirm stop",
      busy: "Stopping…",
    };
  }
  return { idle: "Kick", confirm: "Confirm kick", busy: "Kicking…" };
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
