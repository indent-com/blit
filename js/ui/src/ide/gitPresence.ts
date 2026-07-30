/**
 * When a root has no repository — the condition the git-backed dock sections
 * fold on.
 *
 * Its own module because the rule needs testing on its own (session.ts reaches
 * the JSX component chain, which the unit tests cannot load) and because the
 * distinction it draws is easy to lose: `gitHandle === null` plus an error
 * describes both "this directory is not a repository" and "the repository we
 * had just died", and only the first of those may fold.
 */

/**
 * "No repository here, and none coming."
 *
 * `hadRepo` is what keeps a *failure* from reading as an absence. A watch the
 * server closes for a resource limit, or a repo that goes away under an open
 * handle, leaves no handle and an error — but that section is showing commits
 * the user was reading, and has a reason worth stating. Folding it takes the
 * commits off the screen and, when there were none to begin with, hides the
 * reason too: a folded header looks the same whatever caused it. So only a root
 * that never produced a repo folds — an open that settled as a failure, or a
 * remote whose server has no git at all (there the open never even runs, so
 * there is no error to read either).
 *
 * That asymmetry is the point of the sibling `noLsp`, which reads the negotiated
 * features and never a failed attach; this is the same rule for a capability
 * that can also die mid-session.
 *
 * A reconnect cannot flip it in either direction: `hadRepo` outlives the handle
 * and the capability branch needs `fsReady`, so the answer holds at whatever the
 * last settled open gave. A blip over a working repo folds nothing, and a blip
 * over a directory that is not a repository does not briefly unfold it.
 */
export function settledWithoutRepo(v: {
  /** A repo is open right now. */
  hasHandle: boolean;
  /** One has opened for this root since the last open settled as a failure. */
  hadRepo: boolean;
  /** Why there is no repo, from a failed open or a server-side watch close. */
  gitError: string | null;
  /** The connection is up (`FEATURE_FS` negotiated). */
  fsReady: boolean;
  /** The server offers git (`FEATURE_GIT`). */
  gitReady: boolean;
}): boolean {
  if (v.hasHandle || v.hadRepo) return false;
  return v.gitError !== null || (v.fsReady && !v.gitReady);
}
