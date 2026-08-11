import type { IdeSession } from "./session";

/** A replacement is visually complete once both persistent dock views have
 *  settled: the Explorer has a live tree (or an error), and Git has either
 *  produced its first log page or established that there is no repository.
 *  Problems attaches lazily from its panel and therefore is not a handoff
 *  gate. */
export function ideSessionReadyForDisplay(session: IdeSession): boolean {
  const fsSettled =
    session.treePhase() === "live" || session.fsError() !== null;
  const gitHandle = session.gitHandle();
  const gitSettled = gitHandle
    ? session.logLoaded()
    : session.noRepo() || session.gitError() !== null;
  return fsSettled && gitSettled;
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
