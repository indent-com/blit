import type {
  BlitConnectionSnapshot,
  BlitWorkspace,
  SessionId,
} from "@blit-sh/core";

/**
 * Look up the connection snapshot for a given session.
 * Plain function — call inside `createEffect` / `createMemo` for reactivity.
 */
export function useBlitConnection(
  workspace: BlitWorkspace,
  sessionId: SessionId | null,
): BlitConnectionSnapshot | null {
  const snapshot = workspace.getSnapshot();
  if (!sessionId) {
    return snapshot.connections[0] ?? null;
  }
  const session = snapshot.sessions.find((s) => s.id === sessionId);
  if (!session) return null;
  return (
    snapshot.connections.find((c) => c.id === session.connectionId) ?? null
  );
}
