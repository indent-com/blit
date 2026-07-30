import { describe, expect, it } from "vitest";
import { settledWithoutRepo } from "../ide/gitPresence";

/**
 * `noRepo` folds the git-backed dock sections away. A fold carries no reason,
 * so what may fold is exactly what was never a repository — never a repository
 * that failed.
 */
const state = (
  over: Partial<Parameters<typeof settledWithoutRepo>[0]> = {},
) => ({
  hasHandle: false,
  hadRepo: false,
  gitError: null,
  fsReady: true,
  gitReady: true,
  ...over,
});

describe("settled absence of a repository", () => {
  it("folds a root whose open failed for good", () => {
    // Not a repository, or a source terminal that is gone: there is nothing to
    // show and nothing coming.
    expect(settledWithoutRepo(state({ gitError: "invalid path" }))).toBe(true);
  });

  it("folds a remote with no git support", () => {
    // The open effect never runs, so there is no error to read — the absence of
    // the feature is the whole signal.
    expect(settledWithoutRepo(state({ gitReady: false }))).toBe(true);
  });

  it("does not fold a watch that died on an open repository", () => {
    // S2C_GIT_CLOSED for a resource limit or a vanished repo drops the handle
    // and sets the error, which is indistinguishable from a failed open unless
    // hadRepo says otherwise. Folding here would take the commits the user was
    // reading off the screen, and nothing on a folded header says why.
    expect(
      settledWithoutRepo(
        state({
          hadRepo: true,
          gitError: "Repository watch closed: resource limit",
        }),
      ),
    ).toBe(false);
    // Same close over a repo with no commits yet: the fold would hide the only
    // statement of the reason, which is the case a rows-based test would miss.
    expect(
      settledWithoutRepo(
        state({
          hadRepo: true,
          gitError: "Repository watch closed: repo gone",
        }),
      ),
    ).toBe(false);
  });

  it("folds again once an open settles as a failure on the new root", () => {
    // A follow-terminal dock re-resolves its root at every open, so a cwd that
    // has left the worktree must fold even though an earlier generation opened
    // a repo here. The failed open is what clears hadRepo.
    expect(
      settledWithoutRepo(
        state({ hadRepo: false, gitError: "not a repository" }),
      ),
    ).toBe(true);
  });

  it("holds the last settled answer across a reconnect", () => {
    // Transport down, nothing settled yet: not an answer, so no fold.
    expect(settledWithoutRepo(state({ fsReady: false, gitReady: false }))).toBe(
      false,
    );
    // Down over a repo that had opened: hadRepo outlives the handle, so the
    // blip cannot fold the section.
    expect(
      settledWithoutRepo(
        state({ hadRepo: true, fsReady: false, gitReady: false }),
      ),
    ).toBe(false);
    // Down over a root that had already settled as a non-repository: it stays
    // folded rather than unfolding for the length of the reconnect.
    expect(
      settledWithoutRepo(
        state({
          gitError: "not a repository",
          fsReady: false,
          gitReady: false,
        }),
      ),
    ).toBe(true);
    // An open handle is never a fold, whatever else is set.
    expect(settledWithoutRepo(state({ hasHandle: true }))).toBe(false);
  });
});
