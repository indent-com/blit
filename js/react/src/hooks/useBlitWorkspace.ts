import { useSyncExternalStore } from "react";
import type { BlitWorkspaceSnapshot } from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "../BlitContext";

export function useBlitWorkspace() {
  return useRequiredBlitWorkspace();
}

export function useBlitWorkspaceState(): BlitWorkspaceSnapshot {
  const workspace = useRequiredBlitWorkspace();
  return useSyncExternalStore(
    workspace.subscribe,
    workspace.getSnapshot,
    workspace.getSnapshot,
  );
}
