/**
 * ManageTile — one server's panels as pane content, not as a dialog.
 *
 * The panels used to be a modal stack: the remotes overlay, and on top of it an
 * overlay per remote. That made them the least durable thing on the screen.
 * Anything that closed an overlay closed these too — and one of the things that
 * closes an overlay is a window asking to be raised, which is exactly what
 * happens a second after Enable starts an application. So the panel that
 * launched the app dismissed itself, and the viewer's next click had to walk
 * back in through two dialogs.
 *
 * A pane has none of that: it is a tile like an editor or a terminal, it can be
 * split next to the thing it manages, it survives focus going elsewhere, and it
 * is restored by the same hash + tab registry as every other tile.
 *
 * One tile per connection, from {@link manageAssignment} — the panels hold live
 * subscriptions (a client watch pushing a catalog every second, a unit table),
 * and two tiles onto one server would run two of each.
 */

import { createEffect, createSignal, onCleanup, Show } from "solid-js";
import { createBlitWorkspaceState } from "@blit-sh/solid";
import type {
  BlitSurface,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import { ConnectionPanels } from "./ConnectionPanels";
import { connectionHasClientList } from "./ConnectionClients";
import {
  PanelEmpty,
  SectionHeading,
  StatusPill,
  type PanelTone,
} from "./panelKit";
import { scrollbarStyle, type Theme, type UIScale } from "./theme";

/** Connection status → the pill's tone and word. */
function statusTone(status: string | null): { tone: PanelTone; label: string } {
  switch (status) {
    case "connected":
      return { tone: "ok", label: "connected" };
    case "connecting":
    case "authenticating":
      return { tone: "warn", label: status };
    case "error":
      return { tone: "bad", label: "error" };
    default:
      return { tone: "idle", label: status ?? "disconnected" };
  }
}

export function ManageTile(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  theme: Theme;
  palette: TerminalPalette;
  scale: UIScale;
  fontSize: number;
  /** The connection is an `.ro` share: the client-control family never
   *  answers through the forwarder, so the clients tab must not be offered. */
  readOnly?: boolean;
  /** Read-only thumbnail (the background dock). It draws a card, never the
   *  panels — a parked tile that kept its client watch and unit table alive
   *  would cost a per-second catalog for a picture nobody is reading. */
  preview?: boolean;
}) {
  const snapshot = createBlitWorkspaceState(props.workspace);
  const connection = () =>
    snapshot().connections.find((c) => c.id === props.connectionId) ?? null;
  const sessions = () => snapshot().sessions;

  // Surfaces, for the client rows' "watching …" labels. Only this connection's:
  // the panels filter by connection anyway, so aggregating every server's would
  // be work thrown away.
  const [surfaces, setSurfaces] = createSignal<readonly BlitSurface[]>([]);
  createEffect(() => {
    // Re-run on reconnect: the store is per BlitConnection, and a connection
    // that was absent when this first ran has one now.
    void snapshot().connections.length;
    const conn = props.workspace.getConnection(props.connectionId);
    if (!conn) {
      setSurfaces([]);
      return;
    }
    const sync = () =>
      setSurfaces([...conn.surfaceStore.getSurfaces().values()]);
    sync();
    onCleanup(conn.surfaceStore.onChange(sync));
  });

  const canListClients = () => {
    const conn = connection();
    return (
      !!conn &&
      connectionHasClientList(
        conn,
        props.readOnly ? new Set([props.connectionId]) : new Set(),
      )
    );
  };

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        display: "flex",
        "flex-direction": "column",
        "min-width": "0",
        overflow: props.preview ? "hidden" : "auto",
        background: props.theme.bg,
        color: props.theme.fg,
        "font-size": `${props.scale.md}px`,
        ...scrollbarStyle(props.theme),
      }}
    >
      <SectionHeading
        theme={props.theme}
        scale={props.scale}
        label={props.connectionId}
      >
        <StatusPill
          theme={props.theme}
          scale={props.scale}
          {...statusTone(connection()?.status ?? null)}
        />
      </SectionHeading>

      <Show
        when={!props.preview}
        fallback={
          <PanelEmpty theme={props.theme} scale={props.scale}>
            Server panels — applications, clients, units, extensions.
          </PanelEmpty>
        }
      >
        <Show
          when={connection()?.status === "connected"}
          fallback={
            <PanelEmpty theme={props.theme} scale={props.scale}>
              Connect to this remote to manage it.
            </PanelEmpty>
          }
        >
          <ConnectionPanels
            workspace={props.workspace}
            connectionId={props.connectionId}
            palette={props.palette}
            fontSize={props.fontSize}
            sessions={sessions()}
            surfaces={surfaces()}
            canListClients={canListClients()}
            canManageExtensions={connection()?.supportsExtensions === true}
          />
        </Show>
      </Show>
    </div>
  );
}
