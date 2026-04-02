import type {
  BlitSession,
  BlitWorkspace,
  SessionId,
} from "@blit-sh/core";

/**
 * Look up a single session by ID from the workspace's current snapshot.
 * This is a plain function (not reactive) — call it inside a `createEffect`
 * or `createMemo` to make it reactive.
 */
export function useBlitSession(
  workspace: BlitWorkspace,
  sessionId: SessionId | null,
): BlitSession | null {
  if (!sessionId) return null;
  const snapshot = workspace.getSnapshot();
  return snapshot.sessions.find((s) => s.id === sessionId) ?? null;
}

export function useBlitFocusedSession(
  workspace: BlitWorkspace,
): BlitSession | null {
  const snapshot = workspace.getSnapshot();
  if (!snapshot.focusedSessionId) return null;
  return (
    snapshot.sessions.find((s) => s.id === snapshot.focusedSessionId) ?? null
  );
}
