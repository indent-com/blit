/**
 * Turning tree paths into paths a shell would accept.
 *
 * Rows in the file tree carry a path relative to the synced root, always
 * `/`-separated — the wire's convention, whatever the host's own is.
 */

/**
 * A row's path as it would be typed into a shell.
 *
 * The absolute form is what is useful to paste, so the root is prefixed when
 * it is known; it arrives with the FS_SYNCED echo, so before the first sync
 * there is only the relative path to give, which still pastes usefully next
 * to a terminal sitting in that root.
 *
 * The separator follows the root rather than the platform: a Windows root is
 * the one case where joining with `/` yields a path its own shell mishandles,
 * and a backslash is a legal character in a POSIX filename, so the test has
 * to be "does this root look like a Windows one", not "is there a backslash".
 */
export function absolutePath(root: string | null, rel: string): string {
  if (!root) return rel;
  const windows = !root.startsWith("/") && root.includes("\\");
  const sep = windows ? "\\" : "/";
  const base = root.length > 1 ? root.replace(/[/\\]+$/, "") : root;
  if (!rel) return base;
  const native = windows ? rel.replace(/\//g, sep) : rel;
  return base === "/" ? `/${native}` : `${base}${sep}${native}`;
}
