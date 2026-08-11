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

/** The server's diagnostic when a PTY-relative open loses its anchor between
 *  the UI choosing it and the request being handled. This is an expected
 *  terminal-lifecycle race, not text that belongs in the UI. */
const SOURCE_TERMINAL_UNAVAILABLE = "source terminal has no working directory";

/** True for the fs/git/lsp open failure produced by that lifecycle race. */
export function isSourceTerminalUnavailableError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return message.toLowerCase().includes(SOURCE_TERMINAL_UNAVAILABLE);
}

/** The current incarnation of a PTY, or null once its newest one exited. */
export function currentSourceSessionForPty(
  sessions: readonly BlitSession[],
  connectionId: ConnectionId,
  ptyId: number,
): BlitSession | null {
  let newest: BlitSession | null = null;
  for (const session of sessions) {
    if (session.connectionId !== connectionId || session.ptyId !== ptyId)
      continue;
    // Later entries are newer (the connection appends); a live incarnation
    // wins over the closed generation retained during reconnect.
    if (!newest || newest.state === "closed" || session.state !== "closed")
      newest = session;
  }
  // An exited current generation also supersedes an older closed generation;
  // returning the stale one would resurrect a PTY that is known to be gone.
  return newest?.state === "exited" ? null : newest;
}

/** Whether that client-side incarnation can still resolve on the server.
 *  A closed session is provisional during reconnect, but stale once the
 *  replacement terminal list is complete (`connectionReady`). */
export function sourceSessionCanResolveCwd(
  session: BlitSession | null,
  connectionReady: boolean,
): boolean {
  return !!session && (session.state !== "closed" || !connectionReady);
}

/**
 * The SessionId to open against *now*: the newest session on `ptyId`, a live
 * one winning over a closed one, falling back to `fallback` when the pty is
 * unknown. Callers still send the fallback so the server remains the authority
 * on a racing open; the UI recognizes and absorbs that lifecycle refusal.
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
  return (
    currentSourceSessionForPty(sessions, connectionId, ptyId)?.id ?? fallback
  );
}
