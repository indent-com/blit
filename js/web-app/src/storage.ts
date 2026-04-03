import { useSyncExternalStore } from "react";
import { PALETTES, DEFAULT_FONT } from "@blit-sh/core";
import type { TerminalPalette } from "@blit-sh/core";
import { isEncrypted, decryptPassphrase } from "./passphrase-crypto";

export const HOST_KEY = "blit.host";
export const PALETTE_KEY = "blit.palette";
export const FONT_KEY = "blit.fontFamily";
export const FONT_SIZE_KEY = "blit.fontSize";
export const FONT_SMOOTHING_KEY = "blit.fontSmoothing";

const PERSISTED_KEYS = new Set([
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  FONT_SMOOTHING_KEY,
  "blit.layouts",
]);

// ---------------------------------------------------------------------------
// Config WS — syncs persisted keys to/from ~/.config/blit/blit.conf
// ---------------------------------------------------------------------------

const cache = new Map<string, string>();
let configWs: WebSocket | null = null;
let configReady = false;
type ConfigListener = (key: string, value: string) => void;
const listeners = new Set<ConfigListener>();

export function onConfigChange(fn: ConfigListener): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function notifyListeners(key: string, value: string) {
  for (const fn of listeners) fn(key, value);
}

function configWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const base = location.pathname.endsWith("/")
    ? location.pathname
    : location.pathname + "/";
  return proto + "//" + location.host + base + "config";
}

let configUnavailable = false;
const pendingWrites = new Map<string, string>();

function getPassphraseFromHash(): string | null {
  const raw = location.hash.slice(1);
  if (!raw) return null;
  const first = raw.split("&")[0];
  if (/^[lpa]=/.test(first)) return null;
  const decoded = decodeURIComponent(first);
  if (isEncrypted(decoded)) return decryptPassphrase(decoded);
  return decoded;
}

export function connectConfigWs(): void {
  if (configWs || configUnavailable) return;
  const pass = getPassphraseFromHash();
  if (!pass) return;

  const ws = new WebSocket(configWsUrl());
  configWs = ws;

  ws.onopen = () => ws.send(pass);

  const serverValues = new Map<string, string>();

  ws.onmessage = (ev) => {
    const msg = String(ev.data);
    if (msg === "ok") return;
    if (msg === "ready") {
      configReady = true;
      for (const [key, value] of pendingWrites) {
        if (serverValues.get(key) !== value) {
          ws.send(`set ${key} ${value}`);
        }
      }
      pendingWrites.clear();
      return;
    }
    const eq = msg.indexOf("=");
    if (eq > 0) {
      const key = msg.slice(0, eq);
      const value = msg.slice(eq + 1);
      if (!configReady) serverValues.set(key, value);
      cache.set(key, value);
      notifyListeners(key, value);
    }
  };

  ws.onerror = () => {};

  ws.onclose = (ev) => {
    configWs = null;
    configReady = false;
    if (ev.code === 1006 && !ev.wasClean) {
      configUnavailable = true;
      return;
    }
    setTimeout(connectConfigWs, 2000);
  };
}

// ---------------------------------------------------------------------------
// Storage read/write — persisted keys go through the config WS + cache,
// everything else falls through to localStorage.
// ---------------------------------------------------------------------------

function readLocal(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function readStorage(key: string): string | null {
  if (PERSISTED_KEYS.has(key)) {
    const cached = cache.get(key);
    if (cached !== undefined) return cached;
  }
  return readLocal(key);
}

export function writeStorage(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {}
  if (PERSISTED_KEYS.has(key)) {
    cache.set(key, value);
    if (configWs && configWs.readyState === WebSocket.OPEN && configReady) {
      configWs.send(`set ${key} ${value}`);
    } else if (configWs && !configReady) {
      pendingWrites.set(key, value);
    }
  }
}

// ---------------------------------------------------------------------------
// React hook — subscribe to a single config key reactively.
// ---------------------------------------------------------------------------

export function useConfigValue(key: string): string | null {
  return useSyncExternalStore(
    (cb) =>
      onConfigChange((k) => {
        if (k === key) cb();
      }),
    () => readStorage(key),
  );
}

// ---------------------------------------------------------------------------
// Derived helpers
// ---------------------------------------------------------------------------

export function blitHost(): string {
  return readStorage(HOST_KEY) || location.hostname;
}

const gatewayHost =
  (import.meta.env.VITE_BLIT_GATEWAY as string | undefined) ?? location.host;

export const basePath = location.pathname.endsWith("/")
  ? location.pathname
  : location.pathname + "/";

export function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return proto + "//" + gatewayHost + location.pathname;
}

export function wtUrl(): string {
  return "https://" + gatewayHost + location.pathname;
}

export function wtCertHash(): string | undefined {
  return undefined;
}

export function preferredPalette(): TerminalPalette {
  const q = new URLSearchParams(location.search).get("palette");
  if (q) {
    const p = PALETTES.find((x) => x.id === q);
    if (p) return p;
  }
  const s = readStorage(PALETTE_KEY);
  if (s) {
    const p = PALETTES.find((x) => x.id === s);
    if (p) return p;
  }
  return PALETTES[0];
}

export function preferredFontSize(): number {
  const q = new URLSearchParams(location.search).get("fontSize");
  if (q) {
    const n = parseInt(q, 10);
    if (n > 0) return n;
  }
  const s = readStorage(FONT_SIZE_KEY);
  if (s) {
    const n = parseInt(s, 10);
    if (n > 0) return n;
  }
  return 13;
}

export function preferredFont(): string {
  const q = new URLSearchParams(location.search).get("font");
  if (q?.trim()) return q.trim();
  const s = readStorage(FONT_KEY);
  if (s?.trim()) return s.trim();
  return DEFAULT_FONT;
}
