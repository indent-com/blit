import {
  TRAY_HAS_MENU,
  TRAY_ITEM_IS_MENU,
  type DesktopNotification,
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

/** Mouse activation follows StatusNotifierItem semantics. A touch tap opens an
 *  advertised menu because touch has no reliable secondary-click gesture. */
export function trayPrimaryOpensMenu(flags: number, touch = false): boolean {
  return (
    (flags & TRAY_ITEM_IS_MENU) !== 0 ||
    (touch && (flags & TRAY_HAS_MENU) !== 0)
  );
}
