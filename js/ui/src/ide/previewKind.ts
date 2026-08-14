/**
 * What a file can be previewed *as*, decided from its name.
 *
 * Extension-based rather than sniffed: the switcher has to decide whether
 * to offer a Preview tab before any bytes have been fetched, and a name is
 * what it has. A file whose extension lies gets a preview that fails
 * visibly, which is better than no tab at all.
 */

export type PreviewKind = "image" | "svg" | "markdown" | "html";

/** Raster formats a browser can decode from a blob URL. */
const IMAGE_MIME: Record<string, string> = {
  png: "image/png",
  jpg: "image/jpeg",
  jpeg: "image/jpeg",
  gif: "image/gif",
  webp: "image/webp",
  avif: "image/avif",
  bmp: "image/bmp",
  ico: "image/x-icon",
};

const MARKDOWN = new Set(["md", "markdown", "mdown", "mkd"]);
const HTML = new Set(["html", "htm", "xhtml"]);

function extensionOf(path: string): string {
  const base = path.slice(path.lastIndexOf("/") + 1);
  const dot = base.lastIndexOf(".");
  return dot <= 0 ? "" : base.slice(dot + 1).toLowerCase();
}

/** The preview a path supports, or null when it has none. */
export function previewKindFor(path: string): PreviewKind | null {
  const ext = extensionOf(path);
  if (ext === "svg") return "svg";
  if (ext in IMAGE_MIME) return "image";
  if (MARKDOWN.has(ext)) return "markdown";
  if (HTML.has(ext)) return "html";
  return null;
}

/** MIME type for a previewable path, for the blob the browser decodes. */
export function previewMime(path: string): string {
  const ext = extensionOf(path);
  if (ext === "svg") return "image/svg+xml";
  return IMAGE_MIME[ext] ?? "application/octet-stream";
}

/** Resolve an href found inside `fromPath` against it, POSIX-style.
 *  Returns null for anything that is not a local relative reference —
 *  absolute URLs, protocol-relative, fragments, and data URLs are for the
 *  caller to leave alone. */
export function resolveRelative(fromPath: string, href: string): string | null {
  if (!href || /^[a-z][a-z0-9+.-]*:/i.test(href)) return null;
  if (href.startsWith("//") || href.startsWith("#")) return null;
  const slash = fromPath.lastIndexOf("/");
  const base =
    href.startsWith("/") || slash === -1 ? "" : fromPath.slice(0, slash);
  // Drop any query/fragment before treating it as a path.
  const encoded = href.replace(/[?#].*$/, "");
  let clean = encoded;
  try {
    clean = decodeURIComponent(encoded);
  } catch {
    // A literal or malformed `%` is still a valid filesystem character.
  }
  const parts = `${base}/${clean}`.split("/");
  const out: string[] = [];
  for (const part of parts) {
    if (!part || part === ".") continue;
    if (part === "..") {
      out.pop();
      continue;
    }
    out.push(part);
  }
  return `/${out.join("/")}`;
}

export type WorkspaceLinkTarget = {
  path: string;
  fragment: string | null;
  view: "preview" | "editor";
};

/** Resolve any non-network markdown link to an in-app workspace destination.
 * Previewable files stay rendered; every other file opens in the editor.
 * A fragment-only link addresses the current document. */
export function workspaceLinkTarget(
  fromPath: string,
  href: string,
): WorkspaceLinkTarget | null {
  if (!href || /^[a-z][a-z0-9+.-]*:/i.test(href)) return null;
  if (href.startsWith("//")) return null;

  const hash = href.indexOf("#");
  let fragment: string | null = hash === -1 ? null : href.slice(hash + 1);
  if (fragment !== null) {
    try {
      fragment = decodeURIComponent(fragment);
    } catch {
      // Keep a malformed fragment usable as a literal heading id.
    }
  }

  const pathHref = href.slice(0, hash === -1 ? href.length : hash);
  const pathPart = pathHref.replace(/\?.*$/, "");
  const path = pathPart ? resolveRelative(fromPath, pathPart) : fromPath;
  if (!path) return null;
  return {
    path,
    fragment,
    view: previewKindFor(path) ? "preview" : "editor",
  };
}

/** GitHub-style heading id used by markdown fragment links. */
export function markdownHeadingSlug(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, "")
    .replace(/\s/g, "-");
}
