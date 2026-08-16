/**
 * Refcounted leases for the IDE session's lazy resources.
 *
 * The dock unmounts a collapsed section, so a panel is the natural owner of
 * "something is looking at this". A panel takes a lease on mount and lets go
 * on unmount; the resource behind it — a per-directory watch, a commit-log
 * walk, a language server and its pushed diagnostics — lives exactly as long
 * as some panel holds one. Its own module so it can be tested without pulling
 * in the session's component dependencies.
 */

import { createSignal, type Accessor } from "solid-js";

/**
 * A refcounted want: the accessor is true while at least one consumer holds a
 * lease, and `acquire` returns that consumer's release.
 *
 * Releases are idempotent, so a double release cannot underflow the count and
 * strand a live resource that nothing will ever close again.
 */
export function createLease(): {
  wanted: Accessor<boolean>;
  acquire: () => () => void;
} {
  const [wanted, setWanted] = createSignal(false);
  let leases = 0;
  const acquire = () => {
    leases++;
    setWanted(true);
    let released = false;
    return () => {
      if (released) return;
      released = true;
      leases--;
      if (leases === 0) setWanted(false);
    };
  };
  return { wanted, acquire };
}
