import type { BlitSession, BlitWorkspace } from "@blit-sh/core";
import { createBlitWorkspaceState } from "./createBlitWorkspace";

export function createBlitSessions(
  workspace?: BlitWorkspace,
): () => readonly BlitSession[] {
  const snapshot = createBlitWorkspaceState(workspace);
  return () => snapshot().sessions;
}
