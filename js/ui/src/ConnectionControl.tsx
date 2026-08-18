/**
 * ConnectionControl — one remote's panels, as an overlay of their own.
 *
 * They used to expand inside the remotes list. That put a unit table and a
 * journal inside a row of a list that is itself a dialog, so the thing being
 * read was always the narrowest column on the screen, and opening it pushed
 * every other remote out of view.
 *
 * It sits on top of the remotes overlay rather than replacing it: the list is
 * where the viewer came from and where closing this returns them, and neither
 * layer has to know how the other is dismissed.
 */

import { onCleanup, onMount } from "solid-js";
import type {
  BlitSession,
  BlitSurface,
  BlitWorkspace,
  ConnectionId,
  TerminalPalette,
} from "@blit-sh/core";
import { ConnectionPanels } from "./ConnectionPanels";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { claimEscape } from "./overlayStack";
import { tp } from "./i18n";

export function ConnectionControlOverlay(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  /** The remote's name, which is also its connection id. Shown in the title. */
  name: string;
  palette: TerminalPalette;
  fontSize: number;
  sessions?: readonly BlitSession[];
  surfaces?: readonly BlitSurface[];
  canListClients: boolean;
  canManageExtensions: boolean;
  onClose: () => void;
}) {
  // Escape closes this, and only this. A listener of our own would not do it:
  // the workspace's handler is a capture-phase window listener registered at
  // mount, so it sees the key first and closes the remotes overlay underneath —
  // one key, two layers dismissed.
  onMount(() => onCleanup(claimEscape(() => props.onClose())));

  return (
    <OverlayBackdrop
      palette={props.palette}
      label={tp("remotes.controlTitle", { name: props.name })}
      onClose={props.onClose}
    >
      <OverlayPanel
        palette={props.palette}
        fontSize={props.fontSize}
        style={{
          width: "min(1100px, 94vw)",
          display: "flex",
          "flex-direction": "column",
          "min-width": "0",
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={tp("remotes.controlTitle", { name: props.name })}
          onClose={props.onClose}
        />
        <ConnectionPanels
          workspace={props.workspace}
          connectionId={props.connectionId}
          palette={props.palette}
          fontSize={props.fontSize}
          sessions={props.sessions}
          surfaces={props.surfaces}
          canListClients={props.canListClients}
          canManageExtensions={props.canManageExtensions}
        />
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
