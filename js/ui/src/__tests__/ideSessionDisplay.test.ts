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
    logLoaded: () => logLoaded,
  } as unknown as IdeSession;
}

describe("IDE session display handoff", () => {
  it("keeps the rendered dock while a same-server terminal root opens", () => {
    const current = session("pty1", {
      treePhase: "live",
      hasRepo: true,
      logLoaded: true,
    });
    const opening = session("pty2");

    expect(selectIdeSessionForDisplay(current, opening)).toBe(current);
  });

  it("switches once both the tree and repository state have settled", () => {
    const current = session("pty1");
    const readyRepo = session("pty2", {
      treePhase: "live",
      hasRepo: true,
      logLoaded: true,
    });
    const readyPlainDir = session("pty3", {
      treePhase: "live",
      noRepo: true,
    });

    expect(ideSessionReadyForDisplay(readyRepo)).toBe(true);
    expect(ideSessionReadyForDisplay(readyPlainDir)).toBe(true);
    expect(selectIdeSessionForDisplay(current, readyRepo)).toBe(readyRepo);
  });

  it("does not reveal a half-loaded commit log", () => {
    const current = session("pty1");
    const logLoading = session("pty2", {
      treePhase: "live",
      hasRepo: true,
    });

    expect(ideSessionReadyForDisplay(logLoading)).toBe(false);
    expect(selectIdeSessionForDisplay(current, logLoading)).toBe(current);
  });

  it("shows settled errors and switches servers immediately", () => {
    const current = session("local", { connectionId: "local" });
    const failed = session("failed", {
      fsError: "not found",
      gitError: "not a repository",
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
