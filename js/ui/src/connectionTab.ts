/**
 * Which tab a server's panels are on — kept here, outside the panels.
 *
 * The panels are pane content ({@link ./ConnectionPanels.tsx}), and a pane can
 * be parked in the dock, which unmounts them. State held inside them is lost at
 * exactly the moment two other readers want it: the dock card, which names the
 * tab the tile is on because that is what a viewer picks the card by, and the
 * restored tile, which should come back on the tab it left.
 *
 * Two values, not one. The pick is what the viewer clicked and may name a tab
 * that has since gone (an extension can be removed while its panel is up); the
 * shown tab is what that resolves to against the set the server actually
 * serves, which only the mounted panels know. Collapsing them would let a
 * fallback overwrite the pick, so a tab that came back would not be returned to.
 */

import { createSignal, type Signal } from "solid-js";

export type ConnectionTab = "clients" | "extensions" | "session" | "systemd";

export const TAB_LABELS: Record<ConnectionTab, string> = {
  clients: "Clients",
  extensions: "Extensions",
  session: "Session",
  systemd: "systemd",
};

type Slot = Signal<ConnectionTab | null>;

const picked = new Map<string, Slot>();
const shown = new Map<string, Slot>();

/** Signals are made on demand and kept: one per connection the viewer has
 *  opened panels for, which is bounded by the remotes they have. */
function slot(map: Map<string, Slot>, connectionId: string): Slot {
  let signal = map.get(connectionId);
  if (!signal) {
    signal = createSignal<ConnectionTab | null>(null);
    map.set(connectionId, signal);
  }
  return signal;
}

/** The viewer's last click, which may name a tab that no longer exists. */
export function pickedTab(connectionId: string): ConnectionTab | null {
  return slot(picked, connectionId)[0]();
}

export function pickTab(connectionId: string, tab: ConnectionTab): void {
  slot(picked, connectionId)[1](tab);
}

/** The tab the panels last resolved to — what a card should name. Null until
 *  they have been opened once, and after the server stops serving any. */
export function shownTab(connectionId: string): ConnectionTab | null {
  return slot(shown, connectionId)[0]();
}

export function setShownTab(
  connectionId: string,
  tab: ConnectionTab | null,
): void {
  slot(shown, connectionId)[1](tab);
}
