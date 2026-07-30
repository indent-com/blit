/**
 * Follow-terminal roots: resolving the pty an IDE session's fs/git/lsp opens
 * hang off (FROM_PTY, docs/ide.md Decision 3 — the server joins the requested
 * path onto that pty's live cwd).
 *
 * The core opens take a `SessionId`, but a SessionId is only valid for one
 * connection generation: every re-establish marks the current sessions closed,
 * mints fresh ids for the same ptys, and prunes the superseded ones
 * (BlitConnection.pruneSupersededSessions). An IdeSession is keyed by *pty* and
 * kept warm across reconnects, so the id its descriptor was built with dies
 * under it — and an open that cannot resolve its source is refused, because
 * dropping FROM_PTY would rebase a pty-relative path (the dock's
 * follow-terminal root is `""`) onto the server's own cwd. So the descriptor
 * carries the pty, which is stable, and this resolves the live id from it at
 * every open.
 */

import type { BlitSession, ConnectionId, SessionId } from "@blit-sh/core";

/**
 * The SessionId to open against *now*: the newest session on `ptyId`, a live
 * one winning over a closed one, falling back to `fallback` when the pty is
 * unknown (the terminal exited — then the open fails loudly, as it should).
 *
 * The closed-session case is load-bearing: between S2C_HELLO and S2C_LIST
 * every session is closed, and the newest of those is still the one the
 * connection can resolve to a pty.
 */
export function currentSessionForPty(
  sessions: readonly BlitSession[],
  connectionId: ConnectionId,
  ptyId: number,
  fallback: SessionId,
): SessionId {
  let newest: BlitSession | null = null;
  for (const s of sessions) {
    if (s.connectionId !== connectionId || s.ptyId !== ptyId) continue;
    // Later entries are newer (the connection appends).
    if (!newest || newest.state === "closed" || s.state !== "closed")
      newest = s;
  }
  return newest ? newest.id : fallback;
}
