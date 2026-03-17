import { createSignal, onCleanup } from "solid-js";
import { PALETTES, DEFAULT_FONT } from "@blit-sh/core";
import type { TerminalPalette } from "@blit-sh/core";
import { isEncrypted, decryptPassphrase } from "./passphrase-crypto";

// ---------------------------------------------------------------------------
// Remotes — live list of named remote connections from the config WebSocket
// ---------------------------------------------------------------------------

export interface Remote {
  name: string;
  uri: string;
}

/** Parse a raw blit.remotes text (`name = uri` lines) into an ordered array. */
export function parseRemotesText(text: string): Remote[] {
  const result: Remote[] = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const eq = trimmed.indexOf("=");
    if (eq <= 0) continue;
    const name = trimmed.slice(0, eq).trim();
    const uri = trimmed.slice(eq + 1).trim();
    if (name && uri) result.push({ name, uri });
  }
  return result;
}

const [remotes, setRemotesSignal] = createSignal<Remote[]>([]);

/** Reactive accessor — returns the current list of configured remotes. */
export function useRemotes(): () => Remote[] {
  return remotes;
}

/** Send a remotes-add command over the config WebSocket. */
export function addRemote(name: string, uri: string): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`remotes-add ${name} ${uri}`);
}

/** Send a remotes-remove command over the config WebSocket. */
export function removeRemote(name: string): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`remotes-remove ${name}`);
}

/** Set the default remote by writing `target = <name>` to blit.conf. */
export function setDefaultRemote(name: string): void {
  writeStorage(TARGET_KEY, name === "local" ? "" : name);
}

/** Reactive accessor — returns the current default remote name (or null for local). */
export function useDefaultRemote(): () => string | null {
  return useConfigValue(TARGET_KEY);
}

/** Reorder remotes to match the supplied name sequence. */
export function reorderRemotes(names: string[]): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`remotes-reorder ${names.join(" ")}`);
}

/** Rename a remote (remove + add). */
export function renameRemote(oldName: string, newName: string): void {
  const r = remotes().find((r) => r.name === oldName);
  if (!r) return;
  removeRemote(oldName);
  addRemote(newName, r.uri);
}

/** Change a remote's target URI (remove + add). */
export function retargetRemote(name: string, newUri: string): void {
  removeRemote(name);
  addRemote(name, newUri);
}

export const HOST_KEY = "blit.host";
export const PALETTE_KEY = "blit.palette";
export const FONT_KEY = "blit.fontFamily";
export const FONT_SIZE_KEY = "blit.fontSize";
export const FONT_SMOOTHING_KEY = "blit.fontSmoothing";
export const TARGET_KEY = "blit.target";

const PERSISTED_KEYS = new Set([
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  FONT_SMOOTHING_KEY,
  "blit.layouts",
  TARGET_KEY,
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

export function configWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const base = location.pathname.endsWith("/")
    ? location.pathname
    : location.pathname + "/";
  return proto + "//" + location.host + base + "config";
}

let configUnavailable = false;
let configEverAuthed = false;
const pendingWrites = new Map<string, string>();

export type ConfigWsStatus = "connecting" | "connected" | "unavailable";
const [configWsStatus, setConfigWsStatus] =
  createSignal<ConfigWsStatus>("connecting");
export { configWsStatus };

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
  setConfigWsStatus("connecting");

  const serverValues = new Map<string, string>();

  ws.onmessage = (ev) => {
    const msg = String(ev.data);
    if (msg === "ok") {
      configEverAuthed = true;
      return;
    }
    if (msg === "ready") {
      configReady = true;
      setConfigWsStatus("connected");
      for (const [key, value] of pendingWrites) {
        if (serverValues.get(key) !== value) {
          ws.send(`set ${key} ${value}`);
        }
      }
      pendingWrites.clear();
      return;
    }
    if (msg.startsWith("remotes:")) {
      setRemotesSignal(parseRemotesText(msg.slice("remotes:".length)));
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
    if (ev.code === 1006 && !ev.wasClean && !configEverAuthed) {
      configUnavailable = true;
      setConfigWsStatus("unavailable");
      return;
    }
    setConfigWsStatus("connecting");
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
// Solid primitive — subscribe to a single config key reactively.
// Must be called within a reactive owner (component or createRoot).
// ---------------------------------------------------------------------------

export function useConfigValue(key: string): () => string | null {
  const [value, setValue] = createSignal(readStorage(key));
  const unsub = onConfigChange((k) => {
    if (k === key) setValue(readStorage(key));
  });
  onCleanup(unsub);
  return value;
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
