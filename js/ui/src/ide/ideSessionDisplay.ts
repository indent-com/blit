import type { IdeSession } from "./session";

/**
 * A replacement is visually complete once both persistent dock views have
 * settled: the Explorer has a live tree (or an error), and the repository
 * open has resolved — a handle, or a settled failure.
 *
 * The git half deliberately asks whether the *repo* settled, not whether a
 * log page arrived. A page only ever arrives while some panel holds the log
 * lease, and only the session already on screen is handed to the panels — so
 * gating on it made the condition unsatisfiable for the incoming session and
 * a root switch never completed: picking a different root moved the picker
 * and left the dock showing the old root's files, log and branches forever.
 * (Collapsing the Log section would have wedged it for the same reason,
 * which is the opposite of a folded section costing nothing.)
 *
 * The cost is that the Log panel may show its "Loading…" state for a beat
 * after a switch, which is what the log-page gate was trying to avoid. That
 * is the right trade: a brief empty log is a switch in progress, while the
 * old behaviour was a switch that never happened.
 *
 * Problems attaches lazily from its panel and is therefore not a gate.
 */
export function ideSessionReadyForDisplay(session: IdeSession): boolean {
  const fsSettled =
    session.treePhase() === "live" || session.fsError() !== null;
  return fsSettled && session.gitSettled();
}

/** Keep rendered state across same-server root changes until the replacement
 *  is complete. A different server switches immediately: showing another
 *  host's files under the new host label would be misleading. */
export function selectIdeSessionForDisplay(
  previous: IdeSession | null,
  next: IdeSession | null,
): IdeSession | null {
  if (!next || !previous || next === previous) return next;
  if (next.connectionId !== previous.connectionId) return next;
  return ideSessionReadyForDisplay(next) ? next : previous;
}
