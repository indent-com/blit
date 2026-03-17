import { useEffect } from "react";
import type {
  BlitTransport,
  ConnectionId,
  TransportConfig,
} from "@blit-sh/core";
import type { BlitWorkspace } from "@blit-sh/core";

export function useBlitWorkspaceConnection(
  workspace: BlitWorkspace,
  connectionId: ConnectionId,
  transport: BlitTransport | TransportConfig,
): void {
  useEffect(() => {
    workspace.addConnection({ id: connectionId, transport });
    return () => workspace.removeConnection(connectionId);
  }, [workspace, connectionId, transport]);
}
