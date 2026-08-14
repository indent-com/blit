import {
  MPRIS_CAN_RAISE,
  TRAY_HAS_MENU,
  TRAY_ITEM_IS_MENU,
  type DesktopNotification,
  type PortalRequest,
} from "@blit-sh/core";

export type DesktopDelivery = "toast" | "native" | "retain";

/** Presentation policy for a live (never replayed) notification upsert. */
export function desktopDelivery(
  visibility: DocumentVisibilityState,
  permission: NotificationPermission,
): DesktopDelivery {
  if (visibility !== "hidden") return "toast";
  return permission === "granted" ? "native" : "retain";
}

export function desktopNativeTag(
  connectionId: string,
  bootGeneration: bigint | null,
  notificationId: number,
): string | null {
  return bootGeneration == null
    ? null
    : `blit:${connectionId}:${bootGeneration}:${notificationId}`;
}

export function matchesDesktopNotification(
  item: DesktopNotification,
  identity: { notificationId?: number; revision?: number },
): boolean {
  return (
    item.notificationId === identity.notificationId &&
    item.revision === identity.revision
  );
}

/** Whether the sender supplied anything below the summary and provenance
 *  lines. The "default" action does not count: it is activated by clicking the
 *  notification body, so it never renders a button of its own. */
export function desktopNotificationHasDetail(
  item: DesktopNotification,
): boolean {
  return (
    item.body.length > 0 ||
    item.image.png.length > 0 ||
    item.actions.some((action) => action.key !== "default")
  );
}

/** Mouse activation follows StatusNotifierItem semantics. A touch tap opens an
 *  advertised menu because touch has no reliable secondary-click gesture. */
export function trayPrimaryOpensMenu(flags: number, touch = false): boolean {
  return (
    (flags & TRAY_ITEM_IS_MENU) !== 0 ||
    (touch && (flags & TRAY_HAS_MENU) !== 0)
  );
}

export interface MprisSubscriptionTarget {
  subscribe(enabled: boolean): void;
}

/**
 * Reconcile document chrome's protocol subscriptions without toggling stores
 * whose connection snapshots merely changed revision.
 */
export function reconcileMprisSubscriptions(
  active: Set<MprisSubscriptionTarget>,
  desired: Iterable<MprisSubscriptionTarget>,
): void {
  const next = new Set(desired);
  for (const store of active) {
    if (next.has(store)) continue;
    store.subscribe(false);
    active.delete(store);
  }
  for (const store of next) {
    if (active.has(store)) continue;
    store.subscribe(true);
    active.add(store);
  }
}

export interface PortalPresentationEntry {
  connectionId: string;
  connectionLabel: string;
  readOnly: boolean;
  request: PortalRequest;
}

/** Keep a live modal mounted while unrelated workspace snapshots arrive. */
export function samePortalPresentationEntry(
  previous: PortalPresentationEntry | undefined,
  next: PortalPresentationEntry | undefined,
): boolean {
  return (
    previous === next ||
    (previous !== undefined &&
      next !== undefined &&
      previous.connectionId === next.connectionId &&
      previous.connectionLabel === next.connectionLabel &&
      previous.readOnly === next.readOnly &&
      previous.request === next.request)
  );
}

/** Return the focus target needed to keep Tab navigation inside a portal. */
export function portalDialogFocusTarget(
  dialog: HTMLElement,
  active: Element | null,
  backwards: boolean,
): HTMLElement | undefined {
  const focusable = [
    ...dialog.querySelectorAll<HTMLElement>(
      'button:not([disabled]), input:not([disabled]), select:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])',
    ),
  ].filter((element) => element.getAttribute("aria-hidden") !== "true");
  if (focusable.length === 0) return dialog;
  const first = focusable[0]!;
  const last = focusable[focusable.length - 1]!;
  if (active === dialog) return backwards ? last : first;
  if (backwards && active === first) return last;
  if (!backwards && active === last) return first;
  return undefined;
}

export interface MprisMediaSessionEntry {
  connectionId: string;
  readOnly: boolean;
  player: {
    playerId: number;
    active: boolean;
    playbackStatus: "playing" | "paused" | "stopped";
  };
}

export function mprisMediaSessionKey(entry: MprisMediaSessionEntry): string {
  return `${entry.connectionId}:${entry.player.playerId}`;
}

/**
 * Pick the one document-wide Media Session owner. Observation-only players
 * are excluded; focus wins while playing, then cross-connection playing
 * recency, then the focused connection's paused/stopped active player.
 */
export function selectMediaSessionEntry<T extends MprisMediaSessionEntry>(
  entries: readonly T[],
  focusedConnectionId?: string,
  playingOrder: ReadonlyMap<string, number> = new Map(),
  manuallySelectedKey?: string,
): T | undefined {
  const writable = entries.filter((entry) => !entry.readOnly);
  const manual = manuallySelectedKey
    ? writable.find(
        (entry) =>
          entry.player.active &&
          mprisMediaSessionKey(entry) === manuallySelectedKey,
      )
    : undefined;
  if (manual) return manual;
  const focused = writable.filter(
    (entry) => entry.connectionId === focusedConnectionId,
  );
  const focusedPlaying = focused.find(
    (entry) => entry.player.active && entry.player.playbackStatus === "playing",
  );
  if (focusedPlaying) return focusedPlaying;
  const playing = writable.filter(
    (entry) => entry.player.active && entry.player.playbackStatus === "playing",
  );
  if (playing.length > 0) {
    return playing.reduce((latest, entry) =>
      (playingOrder.get(mprisMediaSessionKey(entry)) ?? 0) >
      (playingOrder.get(mprisMediaSessionKey(latest)) ?? 0)
        ? entry
        : latest,
    );
  }
  return (
    focused.find((entry) => entry.player.active) ??
    writable.find((entry) => entry.player.active) ??
    focused[0] ??
    writable[0]
  );
}

/** CanRaise is a base-interface capability and does not depend on CanControl. */
export function canRaiseMpris(
  readOnly: boolean,
  capabilityFlags: number,
): boolean {
  return !readOnly && Boolean(capabilityFlags & MPRIS_CAN_RAISE);
}
