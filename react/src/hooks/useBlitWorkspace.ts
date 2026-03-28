import { useSyncExternalStore } from "react";
import type { BlitWorkspaceSnapshot } from "../types";
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
