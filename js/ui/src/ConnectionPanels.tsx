/**
 * ConnectionPanels — everything there is to say about ONE remote, as tabs.
 *
 * Hosted by {@link ./ManageTile.tsx}, which is a BSP tile: these are pane
 * content, not a dialog. They were a dialog, and the thing that finally settled
 * it was Enable in the Session tab — the application it started raised itself,
 * an activation closes whatever overlay is up, and the panel dismissed itself
 * one second after being used.
 *
 * An expanded remote row used to stack its sections; it now switches between
 * them, because the set stopped being two short lists. Session and clients are
 * still short, but a unit table is a thousand rows and a journal page is a
 * scroller of its own — stacked, either one buries whatever is under it.
 *
 * Which tabs exist is a property of the server, discovered rather than assumed:
 * Session and systemd are extensions, so their tabs exist only while the
 * channel each publishes has a listener. That is followed rather than sampled
 * (`channelPresence.ts`), so installing an extension adds its tab and removing
 * one takes it away while the row stays open — the panel that installs them is
 * one tab over, which is exactly where a stale answer would be noticed.
 *
 * Hence the order: the two tabs every server has come first, and the two an
 * extension provides follow, so the set grows and shrinks at the end of the
 * row instead of shuffling what the viewer was aiming at.
 */

import { createEffect, createSignal, For, onCleanup, Show } from "solid-js";
import type {
  BlitSession,
  BlitSurface,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import { ConnectionClients } from "./ConnectionClients";
import { ConnectionSession } from "./ConnectionSession";
import { ExtensionsPanel } from "./ExtensionsPanel";
import { SystemdPanel } from "./SystemdPanel";
import { followChannelNames } from "./channelPresence";
import { SESSION_CHANNEL } from "./session";
import { SYSTEMD_CHANNEL } from "./systemd";
import { scrollbarStyle, themeFor, ui, uiScale } from "./theme";

type Tab = "clients" | "extensions" | "session" | "systemd";

const LABELS: Record<Tab, string> = {
  clients: "Clients",
  extensions: "Extensions",
  session: "Session",
  systemd: "systemd",
};

// What the viewer picked, per connection, outside any component — the panels are
// pane content and a pane can be parked, which unmounts them. A pick that lived
// in the component would be lost there, and the two places it is needed are
// exactly across that unmount: the thumbnail names the tab the tile is on, and
// the tile comes back on the tab it left.
//
// Not the same as what is shown: an answer can land after the click and a tab
// can vanish under it, so the pick is resolved against what exists rather than
// corrected by an effect that would fight the viewer for it.
const picks = new Map<
  ConnectionId,
  ReturnType<typeof createSignal<Tab | null>>
>([]);

function pick(id: ConnectionId) {
  let signal = picks.get(id);
  if (!signal) {
    signal = createSignal<Tab | null>(null);
    picks.set(id, signal);
  }
  return signal;
}

export function ConnectionPanels(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  palette: TerminalPalette;
  fontSize: number;
  sessions?: readonly BlitSession[];
  surfaces?: readonly BlitSurface[];
  /** The server advertises the client-control family, and we may use it. */
  canListClients: boolean;
  /** The server advertises the extension family. */
  canManageExtensions: boolean;
  /** Draw the tab the tile is on and nothing else: a dock thumbnail is a
   *  picture of where the viewer left this pane, and the panel under the tab is
   *  the expensive half (a client catalog every second, a unit table). */
  preview?: boolean;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [served, setServed] = createSignal<ReadonlySet<string>>(
    new Set<string>(),
  );
  const chosen = () => pick(props.connectionId)[0]();
  const setChosen = (name: Tab) => pick(props.connectionId)[1](name);

  // One watch per connection, for both extension channels at once — the answer
  // is a property of the server's registry, not of either panel.
  createEffect(() => {
    const connection = props.workspace.getConnection(props.connectionId);
    setServed(new Set<string>());
    if (!connection) return;
    let live = true;
    let stop: (() => void) | null = null;
    void followChannelNames(
      connection,
      [SESSION_CHANNEL, SYSTEMD_CHANNEL],
      (present) => {
        // Copied, because the watch keeps one set and mutates it in place: a
        // signal handed the same object twice never sees a change.
        if (live) setServed(new Set(present));
      },
    ).then((release) => {
      // A watch that arrives after this effect was torn down is released at
      // once; it holds a channel ID on the server until it is.
      if (live) stop = release;
      else release();
    });
    onCleanup(() => {
      live = false;
      stop?.();
    });
  });

  const tabs = (): Tab[] => {
    const available: Tab[] = [];
    if (props.canListClients) available.push("clients");
    if (props.canManageExtensions) available.push("extensions");
    if (served().has(SESSION_CHANNEL)) available.push("session");
    if (served().has(SYSTEMD_CHANNEL)) available.push("systemd");
    return available;
  };

  /** The tab actually shown: the pick if it still exists, else the first. */
  const tab = (): Tab | null => {
    const available = tabs();
    const picked = chosen();
    if (picked && available.includes(picked)) return picked;
    return available[0] ?? null;
  };

  const activeLabel = (): string => {
    const name = tab();
    return name ? LABELS[name] : "";
  };

  /** One tab, selected or not. Shared so the thumbnail's single label is the
   *  same shape as the strip item it stands for. */
  const tabStyle = (selected: boolean) => ({
    ...ui.btn,
    "border-radius": "0",
    border: "none",
    "border-bottom": `2px solid ${selected ? theme().accent : "transparent"}`,
    "background-color": "transparent",
    color: "inherit",
    "font-size": `${scale().sm}px`,
    padding: `${scale().controlY}px ${scale().controlX}px`,
    opacity: selected ? 1 : 0.6,
  });

  return (
    <Show
      when={tabs().length > 0}
      fallback={
        // A pane cannot render nothing the way an overlay section could: the
        // viewer asked for this server's panels and is owed the answer that it
        // has none. A thumbnail asked for nothing, so it says nothing.
        <Show when={!props.preview}>
          <p
            style={{
              margin: "0",
              padding: `${scale().controlX}px`,
              color: theme().dimFg,
              "font-size": `${scale().sm}px`,
            }}
          >
            This server exposes no panels.
          </p>
        </Show>
      }
    >
      <div
        style={{
          display: "flex",
          "flex-direction": "column",
          "background-color": theme().panelBg,
          "min-width": "0",
          // Fills the tile and passes the height on. `min-height: 0` is what
          // lets it: a flex item defaults to its content's height as a floor,
          // so without it a thousand-row unit table makes this taller than the
          // pane and every bound below is measured against the wrong box.
          flex: "1 1 auto",
          "min-height": "0",
        }}
      >
        <div
          role={props.preview ? undefined : "tablist"}
          style={{
            display: "flex",
            gap: `${scale().tightGap}px`,
            padding: `${scale().tightGap}px ${scale().controlX}px`,
            "border-bottom": `1px solid ${theme().subtleBorder}`,
            // The strip is how a viewer leaves a long list; it does not scroll
            // away with it.
            flex: "0 0 auto",
            "min-width": "0",
          }}
        >
          <Show
            when={!props.preview}
            fallback={
              // The whole strip would not fit a dock card, and clipping it
              // would cut off the one item the card exists to show — a pick of
              // `systemd` is the last of four. So the thumbnail draws that item
              // alone: no button, nothing to aim at, because a click anywhere
              // on the card restores the pane.
              <span
                style={{
                  ...tabStyle(true),
                  overflow: "hidden",
                  "text-overflow": "ellipsis",
                  "white-space": "nowrap",
                }}
              >
                {activeLabel()}
              </span>
            }
          >
            <For each={tabs()}>
              {(name) => (
                <button
                  type="button"
                  role="tab"
                  data-connection-tab={name}
                  aria-selected={tab() === name}
                  onClick={() => setChosen(name)}
                  style={{ ...tabStyle(tab() === name), cursor: "pointer" }}
                >
                  {LABELS[name]}
                </button>
              )}
            </For>
          </Show>
        </div>

        {/* One bounded region for whichever panel is up, and the only scroller
            the chrome owns. A panel with a long list of its own bounds that
            list to this box instead (`flex: 1; min-height: 0`), so it scrolls
            there and this never has to — which is what keeps one list to one
            scrollbar. The clients panel has no list of its own and scrolls
            here.

            A thumbnail stops above this: mounting it would run the panel's
            subscriptions — a client catalog every second, a unit table — for a
            picture too small to read either. */}
        <Show when={!props.preview}>
          <div
            style={{
              display: "flex",
              "flex-direction": "column",
              flex: "1 1 auto",
              "min-height": "0",
              "min-width": "0",
              "overflow-y": "auto",
              ...scrollbarStyle(theme()),
            }}
          >
            <Show when={tab() === "clients"}>
              <ConnectionClients
                workspace={props.workspace}
                connectionId={props.connectionId}
                sessions={props.sessions ?? []}
                surfaces={props.surfaces ?? []}
                palette={props.palette}
                fontSize={props.fontSize}
              />
            </Show>
            {/* The extensions panel was built as its own overlay, so it carries
              its own padding; the wrapper only bounds it. Same for systemd. */}
            <Show when={tab() === "extensions"}>
              <div
                style={{
                  padding: `${scale().controlX}px`,
                  "min-width": "0",
                  display: "flex",
                  "flex-direction": "column",
                  flex: "1 1 auto",
                  "min-height": "0",
                }}
              >
                <ExtensionsPanel
                  workspace={props.workspace}
                  connectionId={props.connectionId}
                  palette={props.palette}
                  fontSize={props.fontSize}
                />
              </div>
            </Show>
            <Show when={tab() === "session"}>
              <ConnectionSession
                workspace={props.workspace}
                connectionId={props.connectionId}
                palette={props.palette}
                fontSize={props.fontSize}
              />
            </Show>
            <Show when={tab() === "systemd"}>
              <div
                style={{
                  padding: `${scale().controlX}px`,
                  "min-width": "0",
                  display: "flex",
                  "flex-direction": "column",
                  flex: "1 1 auto",
                  "min-height": "0",
                }}
              >
                <SystemdPanel
                  workspace={props.workspace}
                  connectionId={props.connectionId}
                  palette={props.palette}
                  fontSize={props.fontSize}
                />
              </div>
            </Show>
          </div>
        </Show>
      </div>
    </Show>
  );
}
