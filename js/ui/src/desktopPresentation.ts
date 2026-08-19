import {
  MPRIS_CAN_CONTROL,
  MPRIS_CAN_RAISE,
  MPRIS_CAN_SEEK,
  TRAY_HAS_MENU,
  TRAY_ITEM_IS_MENU,
  type DesktopNotification,
  type PortalRequest,
} from "@blit-sh/core";

export type DesktopDelivery = "toast" | "native" | "retain";

/**
 * How far to slide a chrome popup horizontally to keep it on screen.
 *
 * The popups hang off the right edge of the chrome, which is not the right
 * edge of the window — the status bar's own tools sit to their right. That is
 * invisible on a desktop, where a popup is far narrower than the screen, and
 * unmissable on a phone, where one sized `calc(100vw - 2em)` starts further
 * left than the window does and loses its first centimetre.
 *
 * Returns a translation in pixels: positive to push right off the left edge,
 * negative to pull left off the right one. Never trades one overflow for the
 * other — a popup wider than the viewport is pinned to the left margin, since
 * that is where its content starts.
 */
export function popupViewportShift(
  rect: { left: number; right: number },
  viewportWidth: number,
  margin = 8,
): number {
  if (rect.left >= margin && rect.right <= viewportWidth - margin) return 0;
  if (rect.left < margin) return margin - rect.left;
  return Math.max(margin - rect.left, viewportWidth - margin - rect.right);
}

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

export type TrayPrimaryGesture = "ignore" | "activate" | "menu";

/**
 * What a tray icon's primary click should do.
 *
 * A long press on a touch screen fires `contextmenu` — which has already opened
 * the menu — and then a trailing `click` on the same press. Acting on that click
 * activated the item as well, so the app's window came up behind the menu the
 * user was reading, and the tray menu the app repainted in response could be
 * voided under them.
 */
export function trayPrimaryGesture(
  flags: number,
  pointerType: string | null,
  openedFromLongPress: boolean,
): TrayPrimaryGesture {
  if (openedFromLongPress) return "ignore";
  return trayPrimaryOpensMenu(flags, pointerType === "touch")
    ? "menu"
    : "activate";
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

/**
 * Whether a player's progress can be shown at all.
 *
 * A track of unknown length has no proportion to draw, and a bar that fills
 * the whole width for a live stream would be a lie rather than a readout.
 */
export function mprisHasProgress(lengthUs: number): boolean {
  return Number.isFinite(lengthUs) && lengthUs > 0;
}

/** Whether the progress bar accepts a drag rather than just reporting one. */
export function canSeekMpris(
  readOnly: boolean,
  capabilityFlags: number,
  lengthUs: number,
): boolean {
  return (
    !readOnly &&
    mprisHasProgress(lengthUs) &&
    (capabilityFlags & (MPRIS_CAN_CONTROL | MPRIS_CAN_SEEK)) ===
      (MPRIS_CAN_CONTROL | MPRIS_CAN_SEEK)
  );
}

/**
 * Clamp a scrubbed position into the range `SetPosition` will accept.
 *
 * A track's final microsecond is not a seekable position: the bridge rejects
 * any target at or past `mpris:length`, so a handle dragged to the far right
 * would fail outright rather than skip to the end. Stopping a millisecond
 * short is inaudible, and wide enough to survive a rounded slider step — on a
 * track shorter than that headroom, only its very start is reachable.
 */
export function mprisSeekTargetUs(
  positionUs: number,
  lengthUs: number,
): number {
  if (!mprisHasProgress(lengthUs) || !Number.isFinite(positionUs)) return 0;
  const last = Math.max(0, lengthUs - 1_000);
  return Math.min(Math.max(0, Math.round(positionUs)), last);
}
