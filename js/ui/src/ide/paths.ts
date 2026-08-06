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

/**
 * The path an LSP message names a file by, mirroring the server's own
 * `wire_path` (`crates/lsp/src/text.rs`): workspace-root-relative under the
 * root, absolute otherwise. Absolute is a real case, not a fallback — a
 * definition lands in the stdlib or a registry checkout — and the server's
 * `resolve_wire` takes an absolute path as-is, so the two ends agree.
 *
 * The separator boundary is load-bearing. `/a/bc` is not a child of `/a/b`,
 * and slicing by the root's length alone would turn it into `c` and address a
 * different file. Returning the bare filename when the prefix does not match
 * (the shape this replaced) was worse still: for any path not spelled as a
 * literal prefix of the canonical root — a symlinked checkout, a `..`, a
 * trailing slash — it silently pointed diagnostics *and* the buffer overlay
 * at whatever happened to sit at the workspace root.
 */
export function lspWirePath(root: string | null, path: string): string {
  if (!root) return path;
  const base = root.length > 1 ? root.replace(/[/\\]+$/, "") : root;
  if (path === base) return "";
  for (const sep of ["/", "\\"]) {
    const prefix = /[/\\]$/.test(base) ? base : `${base}${sep}`;
    if (path.startsWith(prefix)) return path.slice(prefix.length);
  }
  return path;
}
