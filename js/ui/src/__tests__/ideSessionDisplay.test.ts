import { describe, expect, it } from "vitest";
import type { IdeSession } from "../ide/session";
import {
  ideSessionReadyForDisplay,
  selectIdeSessionForDisplay,
} from "../ide/ideSessionDisplay";

function session(
  name: string,
  over: {
    connectionId?: string;
    treePhase?: "opening" | "loading" | "live";
    fsError?: string | null;
    hasRepo?: boolean;
    noRepo?: boolean;
    gitError?: string | null;
    /** The repo open resolved — a handle, or a settled failure. */
    gitSettled?: boolean;
    logLoaded?: boolean;
  } = {},
): IdeSession {
  const {
    connectionId = "local",
    treePhase = "opening",
    fsError = null,
    hasRepo = false,
    noRepo = false,
    gitError = null,
    gitSettled = false,
    logLoaded = false,
  } = over;
  return {
    key: name,
    connectionId,
    treePhase: () => treePhase,
    fsError: () => fsError,
    gitHandle: () => (hasRepo ? ({} as never) : null),
    noRepo: () => noRepo,
    gitError: () => gitError,
    gitSettled: () => gitSettled,
    logLoaded: () => logLoaded,
  } as unknown as IdeSession;
}

describe("IDE session display handoff", () => {
  it("keeps the rendered dock while a same-server terminal root opens", () => {
    const current = session("pty1", {
      treePhase: "live",
      hasRepo: true,
      gitSettled: true,
    });
    const opening = session("pty2");

    expect(selectIdeSessionForDisplay(current, opening)).toBe(current);
  });

  it("switches once both the tree and repository state have settled", () => {
    const current = session("pty1");
    const readyRepo = session("pty2", {
      treePhase: "live",
      hasRepo: true,
      gitSettled: true,
    });
    const readyPlainDir = session("pty3", {
      treePhase: "live",
      noRepo: true,
      gitSettled: true,
    });

    expect(ideSessionReadyForDisplay(readyRepo)).toBe(true);
    expect(ideSessionReadyForDisplay(readyPlainDir)).toBe(true);
    expect(selectIdeSessionForDisplay(current, readyRepo)).toBe(readyRepo);
  });

  it("waits for the tree, but not for a commit page", () => {
    // The gate used to also require `logLoaded()`. A log page only arrives
    // while a panel holds the log lease, and panels are only ever handed the
    // session already on screen — so the incoming one could never satisfy it
    // and the switch never happened: the picker moved and the dock kept
    // showing the old root. Waiting on the tree is still right (it is not
    // lease-gated); waiting on a page is what deadlocked.
    const current = session("pty1");
    const treeStillOpening = session("pty2", {
      hasRepo: true,
      gitSettled: true,
    });
    expect(ideSessionReadyForDisplay(treeStillOpening)).toBe(false);
    expect(selectIdeSessionForDisplay(current, treeStillOpening)).toBe(current);

    const repoOpenNoPageYet = session("pty3", {
      treePhase: "live",
      hasRepo: true,
      gitSettled: true,
      logLoaded: false,
    });
    expect(ideSessionReadyForDisplay(repoOpenNoPageYet)).toBe(true);
    expect(selectIdeSessionForDisplay(current, repoOpenNoPageYet)).toBe(
      repoOpenNoPageYet,
    );
  });

  it("holds the dock while the repository is still opening", () => {
    // The other half of the gate: a repo mid-open would render as "no
    // repository" — branches and log empty — so it waits.
    const current = session("pty1");
    const opening = session("pty2", { treePhase: "live", gitSettled: false });
    expect(ideSessionReadyForDisplay(opening)).toBe(false);
    expect(selectIdeSessionForDisplay(current, opening)).toBe(current);
  });

  it("shows settled errors and switches servers immediately", () => {
    const current = session("local", { connectionId: "local" });
    const failed = session("failed", {
      fsError: "not found",
      gitError: "not a repository",
      gitSettled: true,
    });
    const remoteOpening = session("remote", { connectionId: "remote" });

    expect(ideSessionReadyForDisplay(failed)).toBe(true);
    expect(selectIdeSessionForDisplay(current, failed)).toBe(failed);
    expect(selectIdeSessionForDisplay(current, remoteOpening)).toBe(
      remoteOpening,
    );
  });

  it("clears promptly when there is no selected root", () => {
    expect(selectIdeSessionForDisplay(session("current"), null)).toBeNull();
  });
});
