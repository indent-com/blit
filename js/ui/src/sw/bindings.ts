/** Persisted clientId → target bindings, so a restarted worker still knows what an open frame is. */

import type { PreviewTarget } from "@blit-sh/core";

const DB = "blit-preview";
const STORE = "bindings";

let cached: Promise<IDBDatabase | null> | null = null;

/** One connection, reused: a fresh `open` per call can block behind the
 *  others and a blocked open never settles. */
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
    // Storage can be unavailable (private mode, evicted); previews still work
    // for as long as the worker lives, so this is a degradation, not an error.
    request.onerror = () => resolve(null);
  });
}

function tx(db: IDBDatabase, mode: IDBTransactionMode): IDBObjectStore {
  return db.transaction(STORE, mode).objectStore(STORE);
}

export async function loadBindings(): Promise<Map<string, PreviewTarget>> {
  const db = await open();
  if (!db) return new Map();
  return new Promise((resolve) => {
    const out = new Map<string, PreviewTarget>();
    const store = tx(db, "readonly");
    const cursor = store.openCursor();
    cursor.onsuccess = () => {
      const c = cursor.result;
      if (!c) {
        resolve(out);
        return;
      }
      out.set(String(c.key), c.value as PreviewTarget);
      c.continue();
    };
    cursor.onerror = () => resolve(out);
  });
}

export async function rememberBinding(
  clientId: string,
  target: PreviewTarget,
): Promise<void> {
  const db = await open();
  if (!db) return;
  try {
    tx(db, "readwrite").put(target, clientId);
  } catch {
    // As above: losing persistence costs restart recovery, nothing more.
  }
}

export async function forgetBinding(clientId: string): Promise<void> {
  const db = await open();
  if (!db) return;
  try {
    tx(db, "readwrite").delete(clientId);
  } catch {
    // Ditto.
  }
}
