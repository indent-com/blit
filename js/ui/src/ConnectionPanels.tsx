/**
 * ConnectionPanels — everything there is to say about ONE remote, as tabs.
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
import { themeFor, ui, uiScale } from "./theme";

type Tab = "clients" | "extensions" | "session" | "systemd";

const LABELS: Record<Tab, string> = {
  clients: "Clients",
  extensions: "Extensions",
  session: "Session",
  systemd: "systemd",
};

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
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [served, setServed] = createSignal<ReadonlySet<string>>(
    new Set<string>(),
  );
  // What the viewer picked, which is not the same as what is shown: an answer
  // can land after the click and a tab can vanish under it, so the selection is
  // resolved against what exists rather than corrected by an effect that would
  // fight the viewer for it.
  const [chosen, setChosen] = createSignal<Tab | null>(null);

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
    const pick = chosen();
    if (pick && available.includes(pick)) return pick;
    return available[0] ?? null;
  };

  return (
    <Show when={tabs().length > 0}>
      <div
        style={{
          display: "flex",
          "flex-direction": "column",
          "background-color": theme().panelBg,
          "min-width": "0",
        }}
      >
        <div
          role="tablist"
          style={{
            display: "flex",
            gap: `${scale().tightGap}px`,
            padding: `${scale().tightGap}px ${scale().controlX}px`,
            "border-bottom": `1px solid ${theme().subtleBorder}`,
          }}
        >
          <For each={tabs()}>
            {(name) => (
              <button
                type="button"
                role="tab"
                data-connection-tab={name}
                aria-selected={tab() === name}
                onClick={() => setChosen(name)}
                style={{
                  ...ui.btn,
                  "border-radius": "0",
                  border: "none",
                  "border-bottom": `2px solid ${
                    tab() === name ? theme().accent : "transparent"
                  }`,
                  "background-color": "transparent",
                  color: "inherit",
                  "font-size": `${scale().sm}px`,
                  padding: `${scale().controlY}px ${scale().controlX}px`,
                  cursor: "pointer",
                  opacity: tab() === name ? 1 : 0.6,
                }}
              >
                {LABELS[name]}
              </button>
            )}
          </For>
        </div>

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
          <div style={{ padding: `${scale().controlX}px`, "min-width": "0" }}>
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
          <div style={{ padding: `${scale().controlX}px`, "min-width": "0" }}>
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
  );
}
