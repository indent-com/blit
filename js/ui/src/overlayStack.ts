/**
 * Who owns Escape when overlays are stacked.
 *
 * The workspace's shortcut handler is a window listener in the capture phase,
 * registered at mount — so it sees Escape before anything an overlay adds
 * later, whatever phase that uses, and closes the *bottom* layer. A nested
 * overlay therefore cannot claim the key by listening harder; it has to be
 * asked. This is the asking.
 *
 * Deliberately not a signal: nothing renders from it, and a keydown handler
 * needs the current answer rather than a reactive one.
 */

type Claim = { dismiss: () => void };

const claims: Claim[] = [];

/**
 * Claim Escape for as long as the caller is mounted. The returned function
 * releases the claim and must be called on cleanup, or a dismissed overlay
 * keeps swallowing the key.
 */
export function claimEscape(dismiss: () => void): () => void {
  const claim: Claim = { dismiss };
  claims.push(claim);
  return () => {
    const at = claims.lastIndexOf(claim);
    if (at >= 0) claims.splice(at, 1);
  };
}

/**
 * Dismiss the innermost claimant, if there is one. True when it handled the
 * key, so the caller leaves the layers below alone.
 */
export function dismissTopClaim(): boolean {
  const claim = claims[claims.length - 1];
  if (!claim) return false;
  claim.dismiss();
  return true;
}
