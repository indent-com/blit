import {
  parseTileAssignment,
  parseDiffArg,
  parseWebAssignment,
} from "../bsp/layout";
import { webLocationLabel } from "../preview";

function baseName(p: string): string {
  const s = p.replace(/\/+$/, "");
  const i = s.lastIndexOf("/");
  return i === -1 ? s : s.slice(i + 1);
}

/** Human-readable title/subtitle/kind for an IDE tile assignment. Shared by the
 *  Cmd+K/expose switcher and the right-dock background-editor cards. */
export function tileDisplay(assignment: string): {
  kind: "editor" | "diff" | "commit" | "web" | "manage";
  title: string;
  subtitle: string;
} {
  // A web pane parks in the same dock, so it needs a card too: host for the
  // title, full URL for the subtitle.
  const web = parseWebAssignment(assignment);
  if (web) {
    // The plain-iframe marker stays out of both lines; it changes how the
    // pane loads, not what it is showing.
    const url = webLocationLabel(web.url);
    let host = url;
    try {
      host = new URL(url).host || url;
    } catch {
      // Not parseable; the raw value is still the best label available.
    }
    return { kind: "web", title: host, subtitle: url };
  }
  const parsed = parseTileAssignment(assignment);
  if (!parsed) return { kind: "editor", title: assignment, subtitle: "" };
  if (parsed.kind === "diff") {
    const { path, staged } = parseDiffArg(parsed.arg);
    return {
      kind: "diff",
      title: baseName(path),
      subtitle: staged ? "Diff · staged" : "Diff",
    };
  }
  // A manage tile's whole address is its connection, and the panels inside it
  // are named by their own tabs — so the card says which server, not which tab.
  if (parsed.kind === "manage") {
    return {
      kind: "manage",
      title: parsed.connectionId,
      subtitle: "Manage",
    };
  }
  if (parsed.kind === "commit") {
    const colon = parsed.arg.indexOf(":");
    const oid = colon > 0 ? parsed.arg.slice(0, colon) : parsed.arg;
    const repoPath = colon > 0 ? parsed.arg.slice(colon + 1) : "";
    return {
      kind: "commit",
      title: oid.slice(0, 8),
      subtitle: repoPath ? `Commit · ${baseName(repoPath)}` : "Commit",
    };
  }
  return { kind: "editor", title: baseName(parsed.arg), subtitle: parsed.arg };
}
