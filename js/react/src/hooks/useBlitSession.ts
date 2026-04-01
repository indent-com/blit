import { useSyncExternalStore } from "react";
import type { BlitSession, SessionId } from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "../BlitContext";

export function useBlitSession(
  sessionId: SessionId | null,
): BlitSession | null {
  const workspace = useRequiredBlitWorkspace();
  const snapshot = useSyncExternalStore(
    workspace.subscribe,
    workspace.getSnapshot,
    workspace.getSnapshot,
  );

  if (!sessionId) return null;
  return snapshot.sessions.find((session) => session.id === sessionId) ?? null;
}

export function useBlitFocusedSession(): BlitSession | null {
  const workspace = useRequiredBlitWorkspace();
  const snapshot = useSyncExternalStore(
    workspace.subscribe,
    workspace.getSnapshot,
    workspace.getSnapshot,
  );

  if (!snapshot.focusedSessionId) return null;
  return (
    snapshot.sessions.find(
      (session) => session.id === snapshot.focusedSessionId,
    ) ?? null
  );
}
