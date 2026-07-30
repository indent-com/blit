import { describe, expect, it } from "vitest";
import type { BlitSession } from "@blit-sh/core";
import { currentSessionForPty } from "../ide/followTerminal";

/**
 * A dock session anchored on a terminal opens fs/git/lsp FROM_PTY: the server
 * resolves the root from that pty's live cwd. It names the pty by SessionId,
 * and SessionIds live for exactly one connection generation — every
 * re-establish marks the current sessions closed, mints new ids for the same
 * ptys, and prunes the superseded ones. The dock session is keyed by pty and
 * stays warm across all of that, so it has to re-resolve.
 */
const session = (
  id: string,
  ptyId: number,
  state: BlitSession["state"] = "active",
  connectionId = "local",
): BlitSession => ({
  id,
  connectionId,
  ptyId,
  tag: "",
  title: null,
  usedRows: 0,
  command: null,
  state,
  exitStatus: null,
});

describe("follow-terminal source resolution", () => {
  it("ignores same-pty sessions on other connections", () => {
    // pty ids are per-connection; following one across connections would open
    // an unrelated terminal's cwd.
    const other = [session("remote:1", 7, "active", "remote")];
    expect(currentSessionForPty(other, "local", 7, "local:1")).toBe("local:1");
  });

  it("follows the pty to the session id minted by a reconnect", () => {
    // Mid-reconnect: HELLO closed generation 1, LIST added generation 2, and
    // the prune has not run yet.
    const midReconnect = [
      session("local:1", 7, "closed"),
      session("local:2", 7),
    ];
    expect(currentSessionForPty(midReconnect, "local", 7, "local:1")).toBe(
      "local:2",
    );
    // After the prune only the live one is left.
    expect(
      currentSessionForPty([session("local:2", 7)], "local", 7, "local:1"),
    ).toBe("local:2");
  });

  it("takes the newest known session while every one is closed", () => {
    // The window between HELLO (all closed) and LIST: the newest closed
    // session is still the one the connection can resolve to a pty, so an
    // open issued here must use it rather than a pruned older id.
    const closed = [
      session("local:1", 7, "closed"),
      session("local:2", 7, "closed"),
    ];
    expect(currentSessionForPty(closed, "local", 7, "local:1")).toBe("local:2");
  });

  it("falls back when the pty is gone", () => {
    // The terminal itself exited: nothing to follow, so the caller keeps its
    // own id and the open fails loudly instead of silently rebasing onto the
    // server's cwd.
    expect(currentSessionForPty([], "local", 7, "local:1")).toBe("local:1");
  });
});
