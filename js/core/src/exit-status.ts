/**
 * Exit-status decoding for `S2C_EXITED` frames.
 *
 * The server encodes a process exit status as a signed 32-bit integer in the
 * `S2C_EXITED` frame (`crates/remote/src/lib.rs`, wire layout
 * `[0x08][pty_id:2][exit_status:4]`, little-endian):
 *
 *  - `>= 0` — normal exit; the value is the `WEXITSTATUS` exit code.
 *  - `< 0`  — terminated by a signal; the value is the negated signal number.
 *  - {@link EXIT_STATUS_UNKNOWN} — the status has not been collected yet.
 *
 * These helpers mirror the canonical Rust implementation in
 * `crates/cli/src/agent.rs` (`exit_code_from_status` / `format_exit_status`)
 * so JS consumers don't have to re-derive the `128 + signal` convention.
 */

/** Sentinel exit status meaning "not yet collected" (`i32::MIN`). */
export const EXIT_STATUS_UNKNOWN = -2147483648;

/**
 * Convert a raw `exit_status` into a conventional shell exit code:
 * unknown → `1`, normal exit → the code itself, signalled → `128 + signal`.
 */
export function exitCodeFromStatus(status: number): number {
  if (status === EXIT_STATUS_UNKNOWN) return 1;
  if (status >= 0) return status;
  return 128 + -status;
}

/**
 * Human-readable rendering of an `exit_status`, matching `blit`'s CLI output:
 * `"exited"`, `"exited(<code>)"` or `"signal(<n>)"`.
 */
export function formatExitStatus(status: number): string {
  if (status === EXIT_STATUS_UNKNOWN) return "exited";
  if (status >= 0) return `exited(${status})`;
  return `signal(${-status})`;
}
