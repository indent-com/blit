/** Preview targets: the `/x/…` bootstrap prefix and the clean-path binding it establishes (docs/design/net.md § Client: service worker). */

/** Reserved path prefix. */
export const PREVIEW_PREFIX = "/x/";

/**
 * A host that can be written into a request line without changing its shape.
 *
 * Every parser here runs its input through `decodeURIComponent`, so `%0d%0a`
 * arrives as a real CRLF — and the host goes straight into the upstream
 * `Host:` and `Origin:` headers. It is not an escalation (a client authorized
 * to relay can already write whatever bytes it likes to a socket), but a
 * parser that accepts a newline inside a hostname is one that lies about what
 * it returns. Space and the header separators go too: none of them belong in
 * a hostname under any encoding.
 */
function hostIsSane(host: string): boolean {
  // eslint-disable-next-line no-control-regex
  return host.length > 0 && !/[\u0000-\u0020\u007f:/?#@\\[\]]/.test(host);
}

export interface PreviewTarget {
  /** Gateway destination name — the routing key for multi-server gateways. */
  dest: string;
  scheme: "http" | "https";
  host: string;
  port: number;
}

/** `https://localhost:3000` → a target on `dest`. */
export function parsePreviewLocation(
  location: string,
  dest: string,
): PreviewTarget {
  const trimmed = location.trim();
  const withScheme = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(trimmed)
    ? trimmed
    : `http://${trimmed}`;
  let url: URL;
  try {
    url = new URL(withScheme);
  } catch {
    throw new Error(`not a URL: ${location}`);
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") {
    throw new Error(`not an http(s) URL: ${location}`);
  }
  const scheme = url.protocol === "https:" ? "https" : "http";
  const port = url.port ? Number(url.port) : scheme === "https" ? 443 : 80;
  if (!Number.isInteger(port) || port <= 0 || port > 65535) {
    throw new Error(`bad port: ${location}`);
  }
  // `URL.hostname` brackets IPv6; the wire wants the bare address.
  const host = url.hostname.replace(/^\[|\]$/g, "");
  if (!host) throw new Error(`no host: ${location}`);
  // An IPv6 literal is the one legitimate host containing colons.
  if (!hostIsSane(host.replace(/:/g, ""))) {
    throw new Error(`bad host: ${location}`);
  }
  return { dest, scheme, host, port };
}

/** The human form of a target, as typed and as remembered. */
export function formatPreviewLocation(target: PreviewTarget): string {
  const host = target.host.includes(":") ? `[${target.host}]` : target.host;
  const implicit =
    (target.scheme === "http" && target.port === 80) ||
    (target.scheme === "https" && target.port === 443);
  return implicit
    ? `${target.scheme}://${host}`
    : `${target.scheme}://${host}:${target.port}`;
}

/** Bootstrap URL for an iframe: carries the target explicitly, so the binding needs no side channel and survives a worker restart via `client.url`. */
export function previewBootstrapUrl(target: PreviewTarget, path = "/"): string {
  const host = target.host.includes(":") ? `[${target.host}]` : target.host;
  const rest = path.startsWith("/") ? path.slice(1) : path;
  return `${PREVIEW_PREFIX}${encodeURIComponent(target.dest)}/${target.scheme}/${host}:${target.port}/${rest}`;
}

/** Query parameter that names a frame's target. */
export const PREVIEW_QUERY = "blit-preview";
/** Query parameter carrying the initial path on that target. */
export const PREVIEW_PATH_QUERY = "blit-path";

/**
 * The URL an iframe is pointed at: the origin root, with the target in the
 * query.
 *
 * `pathname` is `/`, which is what a client-side router reads, so a previewed
 * SPA routes on its own paths rather than on a proxy prefix. Two constraints
 * force the query rather than something tidier. A navigation's
 * `Window.location` is the *request* URL, kept even across redirects, so no
 * response can rewrite a prefixed path after the fact; and a frame whose URL
 * equals an ancestor's is refused as recursive nesting, so a bare `/` cannot
 * load inside an app that is itself served at `/`.
 */
export function previewFrameUrl(target: PreviewTarget, path = "/"): string {
  const spec = [
    target.dest,
    target.scheme,
    target.host,
    String(target.port),
  ].map(encodeURIComponent);
  const params = new URLSearchParams();
  params.set(PREVIEW_QUERY, spec.join("|"));
  const wanted = path.startsWith("/") ? path : `/${path}`;
  if (wanted !== "/") params.set(PREVIEW_PATH_QUERY, wanted);
  return `/?${params.toString()}`;
}

/** Parse what previewFrameUrl encoded, or null. */
export function parsePreviewFrameUrl(
  pathname: string,
  search: string,
): ParsedBootstrap | null {
  if (pathname !== "/") return null;
  const params = new URLSearchParams(search);
  const spec = params.get(PREVIEW_QUERY);
  if (!spec) return null;
  const parts = spec.split("|").map(decodeURIComponent);
  if (parts.length < 4) return null;
  const [dest, scheme, host, portText] = parts;
  const port = Number(portText);
  if (
    !dest ||
    (scheme !== "http" && scheme !== "https") ||
    !hostIsSane(host.replace(/:/g, "")) ||
    !Number.isInteger(port) ||
    port <= 0 ||
    port > 65535
  ) {
    return null;
  }
  return {
    target: { dest, scheme, host, port },
    path: params.get(PREVIEW_PATH_QUERY) || "/",
  };
}

export interface ParsedBootstrap {
  target: PreviewTarget;
  /** Path on the target, always starting with "/". */
  path: string;
}

/** Parse a bootstrap pathname (plus query) back into a target and path. */
export function parseBootstrapUrl(
  pathname: string,
  search = "",
): ParsedBootstrap | null {
  if (!pathname.startsWith(PREVIEW_PREFIX)) return null;
  const rest = pathname.slice(PREVIEW_PREFIX.length);
  const parts = rest.split("/");
  if (parts.length < 3) return null;
  const dest = decodeURIComponent(parts[0]);
  const scheme = parts[1];
  if (!dest || (scheme !== "http" && scheme !== "https")) return null;
  const authority = parts[2];
  const target = parseAuthority(authority, scheme, dest);
  if (!target) return null;
  const path = "/" + parts.slice(3).join("/");
  return { target, path: path + search };
}

function parseAuthority(
  authority: string,
  scheme: "http" | "https",
  dest: string,
): PreviewTarget | null {
  if (!authority) return null;
  let host: string;
  let portText: string;
  if (authority.startsWith("[")) {
    const close = authority.indexOf("]");
    if (close < 0) return null;
    host = authority.slice(1, close);
    portText = authority.slice(close + 1).replace(/^:/, "");
  } else {
    const colon = authority.lastIndexOf(":");
    if (colon < 0) return null;
    host = authority.slice(0, colon);
    portText = authority.slice(colon + 1);
  }
  const port = Number(portText);
  if (
    !hostIsSane(host.replace(/:/g, "")) ||
    !Number.isInteger(port) ||
    port <= 0 ||
    port > 65535
  ) {
    return null;
  }
  return { dest, scheme, host, port };
}

/**
 * Whether a `[remote>][command]` entry is really a location.
 *
 * An explicit scheme, or a `host:port` — a port is what makes it unambiguous.
 * A bare word stays a command: `localhost` is a plausible program name, and
 * guessing wrong would swallow a terminal the user asked for.
 */
export function looksLikeWebLocation(entry: string): boolean {
  const value = entry.trim();
  if (!value || /\s/.test(value)) return false;
  if (/^https?:\/\//i.test(value)) return true;
  return /^\[?[a-z0-9._:-]+\]?:\d{1,5}(\/.*)?$/i.test(value);
}

/** Two targets are the same relayed origin — the pooling and cookie-jar key. */
export function previewKey(target: PreviewTarget): string {
  return `${target.dest}|${target.scheme}|${target.host}|${target.port}`;
}
