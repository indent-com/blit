import { createSignal, onCleanup, type Accessor } from "solid-js";
import type {
  BlitWorkspaceSnapshot,
  BlitSession,
  BlitConnectionSnapshot,
  SessionId,
  ConnectionId,
} from "@blit-sh/core";
import { useBlitContext } from "./BlitContext";

export function useBlitWorkspaceState(): Accessor<BlitWorkspaceSnapshot> {
  const ctx = useBlitContext();
  const [snap, setSnap] = createSignal(ctx.workspace.getSnapshot());
  const unsub = ctx.workspace.subscribe(() => setSnap(ctx.workspace.getSnapshot()));
  onCleanup(unsub);
  return snap;
}

export function useBlitSession(sessionId: Accessor<SessionId | null | undefined>): Accessor<BlitSession | undefined> {
  const state = useBlitWorkspaceState();
  return () => {
    const id = sessionId();
    if (id == null) return undefined;
    return state().sessions.find((s) => s.id === id);
  };
}

export function useBlitConnection(connectionId: Accessor<ConnectionId | null | undefined>): Accessor<BlitConnectionSnapshot | undefined> {
  const state = useBlitWorkspaceState();
  return () => {
    const id = connectionId();
    if (id == null) return undefined;
    return state().connections.find((c) => c.id === id);
  };
}
