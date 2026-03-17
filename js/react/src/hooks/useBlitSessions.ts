import { useSyncExternalStore } from "react";
import type { BlitSession } from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "../BlitContext";

export function useBlitSessions(): readonly BlitSession[] {
  const workspace = useRequiredBlitWorkspace();
  const snapshot = useSyncExternalStore(
    workspace.subscribe,
    workspace.getSnapshot,
    workspace.getSnapshot,
  );
  return snapshot.sessions;
}
