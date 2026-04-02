import { onCleanup } from "solid-js";
import type {
  BlitTransport,
  BlitWorkspace,
  ConnectionId,
  TransportConfig,
} from "@blit-sh/core";

/**
 * Manage a connection's lifecycle within a Solid component.
 * Adds the connection on creation and removes it on cleanup.
 */
export function createBlitWorkspaceConnection(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  transport: BlitTransport | TransportConfig,
): void {
  workspace.addConnection({ id: connectionId, transport });
  onCleanup(() => workspace.removeConnection(connectionId));
}
