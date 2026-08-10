/** Compact relative age used by commit rows in the sidebar. */
export function relativeTime(seconds: bigint, nowSec: number): string {
  let d = nowSec - Number(seconds);
  if (d < 0) d = 0;
  if (d < 60) return `${d}s`;
  if (d < 3600) return `${Math.floor(d / 60)}m`;
  if (d < 86400) return `${Math.floor(d / 3600)}h`;
  if (d < 86400 * 30) return `${Math.floor(d / 86400)}d`;
  return `${Math.floor(d / (86400 * 30))}mo`;
}

/** Delay to the next Unix-second boundary, without accumulating drift. */
export function msUntilNextSecond(nowMs: number): number {
  const remainder = ((nowMs % 1000) + 1000) % 1000;
  return remainder === 0 ? 1000 : 1000 - remainder;
}
