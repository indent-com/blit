import { describe, expect, it, vi } from "vitest";
import {
  claimHmrLease,
  closeTransportBundle,
  deferHmrRelease,
  type HmrLeaseState,
} from "../hmrLease";

describe("HMR state leases", () => {
  it("closes the mux and every retained upstream channel", () => {
    const mux = { close: vi.fn() };
    const channels = [
      { close: vi.fn() },
      { close: vi.fn() },
      { close: vi.fn() },
    ];
    const cache = new Map(
      channels.map((transport, index) => [`remote-${index}`, { transport }]),
    );

    closeTransportBundle(mux, cache);

    expect(mux.close).toHaveBeenCalledOnce();
    for (const channel of channels) {
      expect(channel.close).toHaveBeenCalledOnce();
    }
    expect(cache.size).toBe(0);
  });

  it("releases state after a real development-mode unmount", () => {
    vi.useFakeTimers();
    try {
      const state: HmrLeaseState = {};
      const owner = {};
      let current: HmrLeaseState | null = state;
      const release = vi.fn();
      claimHmrLease(state, owner);

      deferHmrRelease(
        state,
        owner,
        () => current === state,
        release,
        () => {
          current = null;
        },
      );
      vi.runAllTimers();

      expect(release).toHaveBeenCalledOnce();
      expect(current).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps state when the replacement hot mount claims it", () => {
    vi.useFakeTimers();
    try {
      const state: HmrLeaseState = {};
      const oldOwner = {};
      const newOwner = {};
      const release = vi.fn();
      claimHmrLease(state, oldOwner);

      deferHmrRelease(
        state,
        oldOwner,
        () => true,
        release,
        () => {},
      );
      claimHmrLease(state, newOwner);
      vi.runAllTimers();

      expect(release).not.toHaveBeenCalled();
      expect(state.hmrLeaseOwner).toBe(newOwner);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not let a stale mount release replacement state", () => {
    vi.useFakeTimers();
    try {
      const state: HmrLeaseState = {};
      const oldOwner = {};
      const newOwner = {};
      const release = vi.fn();
      claimHmrLease(state, newOwner);

      deferHmrRelease(
        state,
        oldOwner,
        () => true,
        release,
        () => {},
      );
      vi.runAllTimers();

      expect(release).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });
});
