/**
 * The faces a server served us, kept across loads.
 *
 * `font/<family>` answers with the face inlined as base64 in a data URL, so a
 * single family is routinely tens of megabytes — 25 MB for PragmataPro Mono.
 * The response says `immutable`, but a resource that large is exactly what an
 * HTTP cache evicts first (browsers cap one entry at a fraction of the whole
 * cache), so the honest expectation is that every load pays for it again.
 * Storing it ourselves turns the terminal's font into a one-time download.
 *
 * IndexedDB rather than `localStorage` (megabytes, and a synchronous read of
 * them would block the first paint) and rather than the Cache API (which needs
 * a secure context, while a blit server is often reached over plain http on a
 * LAN address). Everything here degrades to `null`: private windows, evicted
 * storage and a quota that says no all mean "fetch it like we used to".
 */

const DB = "blit-fonts";
const STORE = "faces";

/** How long a stored face is used without asking the server again. Matches
 *  the `max-age` the font routes serve, so a font installed or upgraded on
 *  the server lands within a day rather than never. */
export const FONT_MAX_AGE_MS = 24 * 60 * 60 * 1000;

/** How much face CSS is worth keeping. One family can exceed this on its own
 *  — the most recently used is kept regardless, since evicting the font the
 *  terminal is drawing with right now is the one useless thing to do. */
export const FONT_STORE_BUDGET_BYTES = 64 * 1024 * 1024;

export interface StoredFont {
  /** The `@font-face` CSS, verbatim as the server sent it. */
  css: string;
  /** When it was fetched, for [`FONT_MAX_AGE_MS`]. */
  savedAt: number;
  /** When it was last handed to a document, for eviction order. */
  usedAt: number;
}

/** What a stored entry costs and when it was last wanted. */
export interface FontStoreEntry {
  key: string;
  bytes: number;
  usedAt: number;
}

let cached: Promise<IDBDatabase | null> | null = null;

/** One connection, reused: a fresh `open` per call can block behind the
 *  others, and a blocked open never settles. */
function open(): Promise<IDBDatabase | null> {
  cached ??= openOnce();
  return cached;
}

function openOnce(): Promise<IDBDatabase | null> {
  return new Promise((resolve) => {
    let request: IDBOpenDBRequest;
    try {
      request = indexedDB.open(DB, 1);
    } catch {
      resolve(null);
      return;
    }
    request.onupgradeneeded = () => {
      const db = request.result;
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => resolve(null);
    request.onblocked = () => resolve(null);
  });
}

function store(db: IDBDatabase, mode: IDBTransactionMode): IDBObjectStore {
  return db.transaction(STORE, mode).objectStore(STORE);
}

function request<T>(req: IDBRequest<T>): Promise<T | null> {
  return new Promise((resolve) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => resolve(null);
  });
}

/**
 * The stored CSS for a font URL, or null when we have never held it.
 *
 * The read bumps the entry's `usedAt` so eviction sees which families are
 * actually in use, not which were fetched most recently.
 */
export async function loadFontCss(key: string): Promise<StoredFont | null> {
  const db = await open();
  if (!db) return null;
  const entry = await request<StoredFont | undefined>(
    store(db, "readonly").get(key),
  );
  if (!entry || typeof entry.css !== "string" || !entry.css) return null;
  const now = Date.now();
  if (now - entry.usedAt > 60_000) {
    try {
      store(db, "readwrite").put({ ...entry, usedAt: now }, key);
    } catch {}
  }
  return entry;
}

/** Whether a stored entry is old enough to be worth revalidating. */
export function isStale(entry: StoredFont, now: number = Date.now()): boolean {
  return now - entry.savedAt >= FONT_MAX_AGE_MS;
}

/** Persist a font's CSS, evicting whatever no longer fits the budget. */
export async function saveFontCss(key: string, css: string): Promise<void> {
  const db = await open();
  if (!db) return;
  const now = Date.now();
  try {
    store(db, "readwrite").put({ css, savedAt: now, usedAt: now }, key);
  } catch {
    return;
  }
  await prune(db, key);
}

/** Drop a font we can no longer get — the server stopped serving the family,
 *  so the copy we hold is of a face nothing will ask about again. */
export async function forgetFontCss(key: string): Promise<void> {
  const db = await open();
  if (!db) return;
  try {
    store(db, "readwrite").delete(key);
  } catch {}
}

/**
 * Which entries to evict so the rest fits `budget`.
 *
 * Least-recently-used first, and `keep` survives whatever its size: the entry
 * just written is the font in use.
 */
export function selectEvictions(
  entries: readonly FontStoreEntry[],
  budget: number,
  keep?: string,
): string[] {
  const total = entries.reduce((sum, e) => sum + e.bytes, 0);
  if (total <= budget) return [];
  const evictable = entries
    .filter((e) => e.key !== keep)
    .sort((a, b) => a.usedAt - b.usedAt);
  const evicted: string[] = [];
  let held = total;
  for (const entry of evictable) {
    if (held <= budget) break;
    evicted.push(entry.key);
    held -= entry.bytes;
  }
  return evicted;
}

async function prune(db: IDBDatabase, keep: string): Promise<void> {
  const entries = await new Promise<FontStoreEntry[]>((resolve) => {
    const out: FontStoreEntry[] = [];
    let cursor: IDBRequest<IDBCursorWithValue | null>;
    try {
      cursor = store(db, "readonly").openCursor();
    } catch {
      resolve(out);
      return;
    }
    cursor.onsuccess = () => {
      const c = cursor.result;
      if (!c) {
        resolve(out);
        return;
      }
      const value = c.value as StoredFont;
      out.push({
        key: String(c.key),
        bytes: value.css?.length ?? 0,
        usedAt: value.usedAt ?? 0,
      });
      c.continue();
    };
    cursor.onerror = () => resolve(out);
  });
  const evicted = selectEvictions(entries, FONT_STORE_BUDGET_BYTES, keep);
  if (evicted.length === 0) return;
  try {
    const s = store(db, "readwrite");
    for (const key of evicted) s.delete(key);
  } catch {}
}

// ---------------------------------------------------------------------------
// Metrics and the family list — small enough for localStorage, and worth the
// synchronous read: the advance ratio decides the terminal's cell width, so
// having it before the first paint is the difference between sizing the grid
// once and reflowing it once the network answers.
// ---------------------------------------------------------------------------

const METRICS_PREFIX = "blit.font-metrics:";
const LIST_KEY = "blit.font-list:";

interface Timestamped<T> {
  value: T;
  savedAt: number;
}

function readJson<T>(key: string): Timestamped<T> | null {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return null;
    const parsed: unknown = JSON.parse(raw);
    if (
      typeof parsed !== "object" ||
      parsed === null ||
      !("value" in parsed) ||
      typeof (parsed as Timestamped<T>).savedAt !== "number"
    ) {
      return null;
    }
    return parsed as Timestamped<T>;
  } catch {
    return null;
  }
}

function writeJson<T>(key: string, value: T): void {
  try {
    localStorage.setItem(
      key,
      JSON.stringify({ value, savedAt: Date.now() } satisfies Timestamped<T>),
    );
  } catch {}
}

/** A family's remembered advance ratio, and whether to ask again. */
export function loadFontMetrics(
  key: string,
): { advanceRatio: number; stale: boolean } | null {
  const stored = readJson<number>(METRICS_PREFIX + key);
  if (!stored || typeof stored.value !== "number") return null;
  return {
    advanceRatio: stored.value,
    stale: Date.now() - stored.savedAt >= FONT_MAX_AGE_MS,
  };
}

export function saveFontMetrics(key: string, advanceRatio: number): void {
  writeJson(METRICS_PREFIX + key, advanceRatio);
}

export function forgetFontMetrics(key: string): void {
  try {
    localStorage.removeItem(METRICS_PREFIX + key);
  } catch {}
}

/** The remembered `fonts` listing for a base path, and whether it is old. */
export function loadFontList(
  base: string,
): { fonts: string[]; stale: boolean } | null {
  const stored = readJson<string[]>(LIST_KEY + base);
  if (!stored || !Array.isArray(stored.value)) return null;
  const fonts = stored.value.filter(
    (f): f is string => typeof f === "string" && f.trim().length > 0,
  );
  if (fonts.length === 0) return null;
  return { fonts, stale: Date.now() - stored.savedAt >= FONT_MAX_AGE_MS };
}

export function saveFontList(base: string, fonts: string[]): void {
  writeJson(LIST_KEY + base, fonts);
}
