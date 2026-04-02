import { createSignal, onCleanup } from "solid-js";
import type {
  BlitWorkspace,
  BlitWorkspaceSnapshot,
} from "@blit-sh/core";
import { useRequiredBlitWorkspace } from "../BlitContext";

export function createBlitWorkspace(): BlitWorkspace {
  return useRequiredBlitWorkspace();
}

export function createBlitWorkspaceState(
  workspace?: BlitWorkspace,
): () => BlitWorkspaceSnapshot {
  const ws = workspace ?? useRequiredBlitWorkspace();
  const [snapshot, setSnapshot] = createSignal(ws.getSnapshot());
  const unsub = ws.subscribe(() => setSnapshot(ws.getSnapshot()));
  onCleanup(unsub);
  return snapshot;
}
