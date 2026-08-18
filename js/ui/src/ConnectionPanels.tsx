/**
 * ConnectionPanels — everything there is to say about ONE remote, as tabs.
 *
 * An expanded remote row used to stack its sections; it now switches between
 * them, because the set stopped being two short lists. Applications and clients
 * are still short, but a unit table is a thousand rows and a journal page is a
 * scroller of its own — stacked, either one buries whatever is under it.
 *
 * Which tabs exist is a property of the server, discovered rather than assumed:
 * systemd and applications are extensions, so their tabs appear only when the
 * channel they publish answers. That probe is one connect-and-close per
 * expansion, which is cheaper than the channel a tab would hold open.
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
import { sessionSupervisorPresent } from "./session";
import { systemdWatcherPresent } from "./systemd";
import { themeFor, ui, uiScale } from "./theme";

type Tab = "apps" | "clients" | "systemd" | "extensions";

const LABELS: Record<Tab, string> = {
  apps: "Applications",
  clients: "Clients",
  systemd: "systemd",
  extensions: "Extensions",
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

  const [hasSession, setHasSession] = createSignal(false);
  const [hasSystemd, setHasSystemd] = createSignal(false);
  // What the viewer picked, which is not the same as what is shown: a probe
  // can land after the click and a tab can vanish on reconnect, so the
  // selection is resolved against what exists rather than corrected by an
  // effect that would fight the viewer for it.
  const [chosen, setChosen] = createSignal<Tab | null>(null);

  // One probe per connection. A rejected connect means nobody serves that
  // channel here, which is an answer rather than a failure.
  createEffect(() => {
    const connection = props.workspace.getConnection(props.connectionId);
    setHasSession(false);
    setHasSystemd(false);
    if (!connection) return;
    let live = true;
    void sessionSupervisorPresent(connection).then((present) => {
      if (live) setHasSession(present);
    });
    void systemdWatcherPresent(connection).then((present) => {
      if (live) setHasSystemd(present);
    });
    onCleanup(() => {
      live = false;
    });
  });

  const tabs = (): Tab[] => {
    const available: Tab[] = [];
    if (hasSession()) available.push("apps");
    if (props.canListClients) available.push("clients");
    if (hasSystemd()) available.push("systemd");
    if (props.canManageExtensions) available.push("extensions");
    return available;
  };

  /** The tab actually shown: the pick if it still exists, else the first.
   *  Applications leads the order, so it opens where it exists — "what does
   *  this machine run" outlives "who is watching it". */
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

        <Show when={tab() === "apps"}>
          <ConnectionSession
            workspace={props.workspace}
            connectionId={props.connectionId}
            palette={props.palette}
            fontSize={props.fontSize}
          />
        </Show>
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
        {/* The two extension panels were built as their own overlays, so they
            carry their own padding; the wrapper only bounds them. */}
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
      </div>
    </Show>
  );
}
