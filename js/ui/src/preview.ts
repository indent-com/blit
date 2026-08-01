/** Web panes: service worker registration, and the locations each server remembers (docs/design/net.md § Client: service worker). */

import {
  formatPreviewLocation,
  looksLikeWebLocation,
  parsePreviewLocation,
  previewFrameUrl,
  type PreviewTarget,
} from "@blit-sh/core";
import { shellCapabilities } from "./shellCapabilities";

export { looksLikeWebLocation };

/** Just the two KV calls these helpers need. */
export interface KvStore {
  kvFetch(key: string): Promise<{ value: Uint8Array } | null>;
  kvPut(key: string, value: Uint8Array): Promise<unknown>;
}
import { readStoredPassphrase } from "./passphrase-storage";

/** Where a server's remembered locations live. */
export const WEB_LOCATIONS_KEY = "web/locations";

/** Worker script path. In production the gateway serves the bundle; in dev the
 *  vite server transforms the TS entry, which must be registered as a module
 *  (its imports are real module specifiers) — see vite.config.ts. */
const SW_URL = import.meta.env?.DEV ? "/src/sw/index.ts" : "/sw.js";
const SW_TYPE: WorkerType | undefined = import.meta.env?.DEV
  ? "module"
  : undefined;

let registration: Promise<ServiceWorkerRegistration | null> | null = null;

/** True when previews can work at all here. */
export function previewSupported(): boolean {
  return (
    shellCapabilities().previews &&
    typeof navigator !== "undefined" &&
    "serviceWorker" in navigator &&
    self.isSecureContext
  );
}

/** Register the worker and give it the passphrase. */
export async function ensurePreviewWorker(): Promise<string | null> {
  if (!previewSupported()) {
    return self.isSecureContext
      ? "this browser has no service worker support"
      : "previews need a secure context (https, or http on localhost)";
  }
  registration ??= navigator.serviceWorker
    .register(SW_URL, { scope: "/", type: SW_TYPE })
    .catch((err: unknown) => {
      // Reset so a later attempt can retry rather than caching the failure for the life of the tab.
      registration = null;
      throw err;
    });
  try {
    const reg = await registration;
    if (!reg) return "could not register the preview worker";
    latestRegistration = reg;
    await navigator.serviceWorker.ready;
    sendPassphrase();
    return null;
  } catch (err) {
    return err instanceof Error ? err.message : String(err);
  }
}

/** Post the passphrase to the active worker. */
export function sendPassphrase(): void {
  const passphrase = readStoredPassphrase();
  if (!passphrase) return;
  // Prefer the controller, but a worker that is active-but-not-yet-controlling
  // still needs the credential to answer the pane that woke it.
  const worker = navigator.serviceWorker?.controller ?? registrationTarget();
  worker?.postMessage({ type: "blit-passphrase", passphrase });
}

function registrationTarget(): ServiceWorker | null {
  const reg = latestRegistration;
  return reg?.active ?? reg?.waiting ?? null;
}

let latestRegistration: ServiceWorkerRegistration | null = null;

/** Keep re-arming the credential: on worker replacement, and on demand — a
 *  worker that was restarted asks rather than answering 503 forever. */
export function watchPreviewWorker(): void {
  if (!previewSupported()) return;
  navigator.serviceWorker.addEventListener("controllerchange", sendPassphrase);
  navigator.serviceWorker.addEventListener("message", (event) => {
    const data = (event as MessageEvent).data as { type?: string } | null;
    if (data?.type === "blit-need-passphrase") sendPassphrase();
  });
}

/** The URL an iframe should load for a target. */
export function previewIframeUrl(
  dest: string,
  location: string,
  path = "/",
): string {
  return previewFrameUrl(parsePreviewLocation(location, dest), path);
}

// --------------------------------------------------------------------------- Plain iframes ---------------------------------------------------------------------------

/**
 * Marker on a location opened as a plain iframe: the URL loads directly,
 * no relay, no shims. That trades both ways — it works without the preview
 * worker and for public sites the server cannot reach, but the page must
 * allow being framed, and its cross-origin document is unreadable (title
 * and path stay at their defaults).
 *
 * It rides inside the web assignment's URL slot so every pipe a web pane
 * flows through — the dock, persistence, pane moves — carries it untouched.
 * "plain" is not an http(s) scheme, so `normalizeLocation` leaves the
 * string as-is rather than reading it as a relayed location.
 */
const PLAIN_PREFIX = "plain:";

/** Mark `url` as a plain-iframe location. A bare host gets https, not the
 *  relayed flow's http leniency: a plain iframe is for the public web, and
 *  an http embed inside an https workspace is blocked as mixed content. */
export function plainLocation(url: string): string {
  const withScheme = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(url)
    ? url
    : `https://${url}`;
  return PLAIN_PREFIX + withScheme;
}

/** The URL a plain location loads, or null when it is a relayed one. */
export function parsePlainLocation(location: string): string | null {
  return location.startsWith(PLAIN_PREFIX)
    ? location.slice(PLAIN_PREFIX.length)
    : null;
}

/** A location as shown to people: the plain marker off, the URL kept. */
export function webLocationLabel(location: string): string {
  return parsePlainLocation(location) ?? location;
}

// --------------------------------------------------------------------------- Remembered locations ---------------------------------------------------------------------------

/** One remembered location. */
export interface WebLocation {
  url: string;
  title?: string;
  /** Epoch millis of the last open, for most-recent-first ordering. */
  lastUsed?: number;
}

/** Parse the stored blob, tolerating anything that is not what we wrote — a hand-edited value must not break the picker. */
export function parseLocations(text: string | null): WebLocation[] {
  if (!text) return [];
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const out: WebLocation[] = [];
  for (const item of parsed) {
    if (typeof item === "string") {
      out.push({ url: item });
      continue;
    }
    if (
      item &&
      typeof item === "object" &&
      typeof (item as WebLocation).url === "string"
    ) {
      const entry = item as WebLocation;
      out.push({
        url: entry.url,
        title: typeof entry.title === "string" ? entry.title : undefined,
        lastUsed:
          typeof entry.lastUsed === "number" && Number.isFinite(entry.lastUsed)
            ? entry.lastUsed
            : undefined,
      });
    }
  }
  return dedupe(out);
}

export function serializeLocations(locations: readonly WebLocation[]): string {
  return JSON.stringify(dedupe([...locations]));
}

/** Most recently used first, then alphabetical — a stable order for a picker that should put what you just used at the top. */
export function sortLocations(
  locations: readonly WebLocation[],
): WebLocation[] {
  return [...locations].sort((a, b) => {
    const at = a.lastUsed ?? 0;
    const bt = b.lastUsed ?? 0;
    if (at !== bt) return bt - at;
    return a.url.localeCompare(b.url);
  });
}

/** Add or refresh one location, normalizing it so `localhost:3000` and `http://localhost:3000` are one entry rather than two. */
export function withLocation(
  locations: readonly WebLocation[],
  rawUrl: string,
  now: number,
  title?: string,
): WebLocation[] {
  const url = normalizeLocation(rawUrl);
  const rest = locations.filter((l) => normalizeLocation(l.url) !== url);
  const existing = locations.find((l) => normalizeLocation(l.url) === url);
  return [{ url, title: title ?? existing?.title, lastUsed: now }, ...rest];
}

export function withoutLocation(
  locations: readonly WebLocation[],
  rawUrl: string,
): WebLocation[] {
  const url = normalizeLocation(rawUrl);
  return locations.filter((l) => normalizeLocation(l.url) !== url);
}

/** Canonical display form, or the input trimmed when it is not a URL at all (so a bad entry round-trips instead of vanishing). */
/**
 * Split a typed location into an origin and the path within it.
 *
 * A bare path (`/x`) keeps the current origin; anything else is read as a
 * location, with a missing scheme filled in as `http` — the same leniency
 * the web overlay accepts, since `localhost:3000` is what people type.
 */
export function splitLocation(
  text: string,
  currentOrigin: string,
): { origin: string; path: string } | null {
  const trimmed = text.trim();
  if (!trimmed) return null;
  if (trimmed.startsWith("/")) return { origin: currentOrigin, path: trimmed };
  const withScheme = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\//.test(trimmed)
    ? trimmed
    : `http://${trimmed}`;
  let url: URL;
  try {
    url = new URL(withScheme);
  } catch {
    return null;
  }
  if (url.protocol !== "http:" && url.protocol !== "https:") return null;
  // `URL.origin` drops an implicit port, which is the same normalization
  // `normalizeLocation` applies to what the pane remembers — so the two are
  // comparable.
  return {
    origin: normalizeLocation(url.origin),
    path: (url.pathname || "/") + url.search + url.hash,
  };
}

export function normalizeLocation(raw: string): string {
  try {
    return formatPreviewLocation(parsePreviewLocation(raw, ""));
  } catch {
    return raw.trim();
  }
}

function dedupe(locations: WebLocation[]): WebLocation[] {
  const seen = new Map<string, WebLocation>();
  for (const location of locations) {
    const key = normalizeLocation(location.url);
    const prev = seen.get(key);
    // Keep the most recent sighting of a duplicate, and its title.
    if (!prev || (location.lastUsed ?? 0) >= (prev.lastUsed ?? 0)) {
      seen.set(key, {
        ...location,
        url: key,
        title: location.title ?? prev?.title,
      });
    }
  }
  return [...seen.values()];
}

/** Read a server's remembered locations. */
export async function loadLocations(
  connection: KvStore,
): Promise<WebLocation[]> {
  const entry = await connection.kvFetch(WEB_LOCATIONS_KEY);
  if (!entry) return [];
  return parseLocations(new TextDecoder().decode(entry.value));
}

/** Persist a server's remembered locations. */
export async function saveLocations(
  connection: KvStore,
  locations: readonly WebLocation[],
): Promise<void> {
  // Neither `ifHash` nor `create` means unconditional, which is what a convenience list wants: a CAS conflict on "I opened a URL" is not worth surfacing to someone who just wanted a preview.
  await connection.kvPut(
    WEB_LOCATIONS_KEY,
    new TextEncoder().encode(serializeLocations(locations)),
  );
}

/** The target a pane's URL names, for display. */
export function previewTarget(dest: string, url: string): PreviewTarget | null {
  try {
    return parsePreviewLocation(url, dest);
  } catch {
    return null;
  }
}
