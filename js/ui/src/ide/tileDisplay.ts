import {
  parseTileAssignment,
  parseDiffArg,
  parseWebAssignment,
} from "../bsp/layout";
import { shownTab, TAB_LABELS } from "../connectionTab";
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
  // A manage tile's whole address is its connection, so the title carries the
  // whole card: "dev:manage > Session". The tab is the half a card is picked
  // by — this server's is on Session because an application is starting, that
  // one's is on systemd — and there is nowhere else to say it: the panels are
  // unmounted while the tile is parked, and drawing them for a thumbnail would
  // put a per-second client catalog behind a picture nobody can read. Absent
  // until they have resolved a tab, which only they can do, so a tile that has
  // never been opened says which server and stops.
  if (parsed.kind === "manage") {
    const tab = shownTab(parsed.connectionId);
    return {
      kind: "manage",
      title: `${parsed.connectionId}:manage${tab ? ` > ${TAB_LABELS[tab]}` : ""}`,
      subtitle: "",
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
