export interface HmrLeaseState {
  hmrLeaseOwner?: object;
  hmrReleaseTimer?: ReturnType<typeof setTimeout> | null;
}

export interface CloseableTransport {
  close(): void;
}

/** Close the mux and every direct or multiplexed transport retained beside it. */
export function closeTransportBundle<
  K,
  V extends { transport: CloseableTransport },
>(mux: CloseableTransport, channelCache: Map<K, V>): void {
  mux.close();
  for (const entry of channelCache.values()) entry.transport.close();
  channelCache.clear();
}

/** Claim preserved state, cancelling a teardown queued by the previous mount. */
export function claimHmrLease<T extends HmrLeaseState>(
  state: T,
  owner: object,
): T {
  if (state.hmrReleaseTimer != null) {
    clearTimeout(state.hmrReleaseTimer);
    state.hmrReleaseTimer = null;
  }
  state.hmrLeaseOwner = owner;
  return state;
}

/** Cancel any pending release without transferring ownership. */
export function cancelHmrRelease(state: HmrLeaseState): void {
  if (state.hmrReleaseTimer != null) {
    clearTimeout(state.hmrReleaseTimer);
    state.hmrReleaseTimer = null;
  }
}

/**
 * Release preserved state unless another mount claims it first.
 *
 * HMR disposes the old Solid root before the replacement root can adopt its
 * state. Deferring one task makes that handoff explicit while still cleaning
 * up ordinary development-mode unmounts, where import.meta.hot also exists.
 */
export function deferHmrRelease<T extends HmrLeaseState>(
  state: T,
  owner: object,
  isCurrent: () => boolean,
  release: () => void,
  clearCurrent: () => void,
): void {
  if (state.hmrLeaseOwner !== owner) return;
  cancelHmrRelease(state);
  state.hmrReleaseTimer = setTimeout(() => {
    state.hmrReleaseTimer = null;
    if (state.hmrLeaseOwner !== owner || !isCurrent()) return;
    try {
      release();
    } finally {
      if (state.hmrLeaseOwner === owner && isCurrent()) clearCurrent();
    }
  }, 0);
}
