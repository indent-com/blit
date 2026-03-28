import { useSyncExternalStore } from "react";
import type { BlitConnectionSnapshot, ConnectionId } from "../types";
import { useRequiredBlitWorkspace } from "../BlitContext";

export function useBlitConnection(
  connectionId?: ConnectionId,
): BlitConnectionSnapshot | null {
  const workspace = useRequiredBlitWorkspace();
  const snapshot = useSyncExternalStore(
    workspace.subscribe,
    workspace.getSnapshot,
    workspace.getSnapshot,
  );

  if (connectionId) {
    return snapshot.connections.find((connection) => connection.id === connectionId) ?? null;
  }

  if (snapshot.connections.length === 1) {
    return snapshot.connections[0];
  }

  if (snapshot.focusedSessionId) {
    const focused = snapshot.sessions.find((session) => session.id === snapshot.focusedSessionId);
    if (focused) {
      return snapshot.connections.find((connection) => connection.id === focused.connectionId) ?? null;
    }
  }

  return snapshot.connections[0] ?? null;
}
