import { createSignal, onCleanup } from "solid-js";
import { PALETTES, DEFAULT_FONT, DEFAULT_TEXT_GAMMA } from "@blit-sh/core";
import type { TerminalPalette } from "@blit-sh/core";
import {
  readStoredPassphrase,
  clearStoredPassphrase,
} from "./passphrase-storage";

// ---------------------------------------------------------------------------
// Remotes — live list of named remote connections from the config WebSocket
// ---------------------------------------------------------------------------

export interface Remote {
  name: string;
  uri: string;
  /** True for `# name = uri` lines: kept on disk but excluded from resolution. */
  disabled: boolean;
}

/** Parse a raw blit.remotes text into an ordered array.
 *  `name = uri` is enabled, `# name = uri` is disabled. Pure comments (no
 *  `name = uri` body) and blank lines are ignored. */
export function parseRemotesText(text: string): Remote[] {
  const result: Remote[] = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let body = trimmed;
    let disabled = false;
    if (body.startsWith("#")) {
      body = body.slice(1).trimStart();
      disabled = true;
    }
    const eq = body.indexOf("=");
    if (eq <= 0) continue;
    const name = body.slice(0, eq).trim();
    const uri = body.slice(eq + 1).trim();
    if (name && uri) result.push({ name, uri, disabled });
  }
  return result;
}

const [remotes, setRemotesSignal] = createSignal<Remote[]>([]);

/** Reactive accessor — returns the current list of configured remotes. */
export function useRemotes(): () => Remote[] {
  return remotes;
}

// ---------------------------------------------------------------------------
// WebTransport cert hash — pushed by the gateway via the config WS as
// `wt=<sha256hex>` when QUIC is enabled.
// ---------------------------------------------------------------------------

const [wtCertHash, setWtCertHash] = createSignal<string | undefined>(undefined);

/** Reactive accessor — returns the WebTransport cert hash (hex), or
 *  undefined when the gateway does not offer WebTransport. */
export function useWtCertHash(): () => string | undefined {
  return wtCertHash;
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

/** Toggle a remote's enabled/disabled state. Disabled remotes are kept
 *  in blit.remotes (commented out) so they can be re-enabled later. */
export function toggleRemote(name: string): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`remotes-toggle ${name}`);
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

// ---------------------------------------------------------------------------
// Roots — live list of named IDE workspace roots from the config WebSocket.
// A root's on-disk value is an opaque `remote:path` spec; we parse it into a
// (remote, path) pair for the UI. An empty `remote` means the default target.
// ---------------------------------------------------------------------------

export interface Root {
  name: string;
  /** Declared remote name, or "" for the default target. */
  remote: string;
  /** Absolute path on that remote. */
  path: string;
  /** True for `# name = value` lines: kept on disk but hidden from the picker. */
  disabled: boolean;
}

/** Split a `remote:path` value. The remote is the segment before the first
 *  `:` when it contains no `/` (so absolute paths like `/a:b` stay whole). */
function splitRootValue(value: string): { remote: string; path: string } {
  const c = value.indexOf(":");
  // A remote prefix is the segment before the first ':' only when it has no
  // '/' AND the remainder is an absolute path — so "remote:/abs" splits but
  // "/local:x" and "a:b:c" (default target) stay whole and round-trip.
  if (
    c > 0 &&
    !value.slice(0, c).includes("/") &&
    value.slice(c + 1).startsWith("/")
  ) {
    return { remote: value.slice(0, c), path: value.slice(c + 1) };
  }
  return { remote: "", path: value };
}

function joinRootValue(remote: string, path: string): string {
  return remote ? `${remote}:${path}` : path;
}

/** Parse a raw blit.roots text (same `name = value` format as remotes). */
export function parseRootsText(text: string): Root[] {
  const result: Root[] = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let body = trimmed;
    let disabled = false;
    if (body.startsWith("#")) {
      body = body.slice(1).trimStart();
      disabled = true;
    }
    const eq = body.indexOf("=");
    if (eq <= 0) continue;
    const name = body.slice(0, eq).trim();
    const value = body.slice(eq + 1).trim();
    if (!name || !value) continue;
    const { remote, path } = splitRootValue(value);
    result.push({ name, remote, path, disabled });
  }
  return result;
}

const [roots, setRootsSignal] = createSignal<Root[]>([]);

/** Reactive accessor — returns the current list of declared workspace roots. */
export function useRoots(): () => Root[] {
  return roots;
}

/** Add or retarget a root. `remote` may be "" for the default target. */
export function addRoot(name: string, remote: string, path: string): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`roots-add ${name} ${joinRootValue(remote, path)}`);
}

/** Remove a root by name. */
export function removeRoot(name: string): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`roots-remove ${name}`);
}

/** Toggle a root's enabled/disabled state. */
export function toggleRoot(name: string): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`roots-toggle ${name}`);
}

/** Reorder roots to match the supplied name sequence. */
export function reorderRoots(names: string[]): void {
  if (!configWs || configWs.readyState !== WebSocket.OPEN) return;
  configWs.send(`roots-reorder ${names.join(" ")}`);
}

export const HOST_KEY = "blit.host";
export const PALETTE_KEY = "blit.palette";
export const FONT_KEY = "blit.fontFamily";
export const FONT_SIZE_KEY = "blit.fontSize";
/** Glyph antialiasing coverage gamma — see DEFAULT_TEXT_GAMMA. */
export const TEXT_GAMMA_KEY = "blit.textGamma";
export const TARGET_KEY = "blit.target";
export const AUDIO_BITRATE_KEY = "blit.audioBitrate";
export const AUDIO_MUTED_KEY = "blit.audioMuted";
export const VIDEO_QUALITY_KEY = "blit.videoQuality";
export const SURFACE_STREAMING_KEY = "blit.surfaceStreaming";
// Panel widths are UI-local (not synced to blit.conf), so they stay out of
// PERSISTED_KEYS below and round-trip through localStorage only.
export const LEFT_DOCK_WIDTH_KEY = "blit.leftDockWidth";
export const PREVIEW_PANEL_WIDTH_KEY = "blit.previewPanelWidth";
/** Whether the IDE dock is open ("1"/"0"). */
export const LEFT_DOCK_OPEN_KEY = "blit.leftDockOpen";
/** Comma-separated list of collapsed dock sections. */
export const LEFT_COLLAPSED_KEY = "blit.leftCollapsed";
/** Editor soft-wrap ("1"/"0"). Persisted like the font settings — it is a
 *  reading preference, not per-machine chrome geometry. */
export const EDITOR_WRAP_KEY = "blit.editorWrap";

const PERSISTED_KEYS = new Set([
  PALETTE_KEY,
  FONT_KEY,
  FONT_SIZE_KEY,
  TEXT_GAMMA_KEY,
  EDITOR_WRAP_KEY,
  "blit.layouts",
  TARGET_KEY,
  AUDIO_BITRATE_KEY,
  AUDIO_MUTED_KEY,
  VIDEO_QUALITY_KEY,
  SURFACE_STREAMING_KEY,
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

// Reconnect backoff. A fixed short interval is actively harmful when the
// server is throttling: every client retrying in lockstep keeps the gateway's
// global unauthenticated-handshake slots occupied, which makes the throttle
// refuse still more attempts. Back off, and jitter so clients spread out.
const CONFIG_RECONNECT_MIN_MS = 2000;
const CONFIG_RECONNECT_MAX_MS = 30000;
let configReconnectDelay = CONFIG_RECONNECT_MIN_MS;

function scheduleConfigReconnect(): void {
  const jitter = 1 + Math.random() * 0.3;
  setTimeout(connectConfigWs, configReconnectDelay * jitter);
  configReconnectDelay = Math.min(
    configReconnectDelay * 2,
    CONFIG_RECONNECT_MAX_MS,
  );
}

export type ConfigWsStatus = "connecting" | "connected" | "unavailable";
const [configWsStatus, setConfigWsStatus] =
  createSignal<ConfigWsStatus>("connecting");
export { configWsStatus };

/** Close the config WebSocket and stop reconnection attempts. */
export function disconnectConfigWs(): void {
  if (configWs) {
    const ws = configWs;
    configWs = null;
    configReady = false;
    ws.onclose = null;
    ws.close();
  }
}

export function connectConfigWs(): void {
  if (configWs || configUnavailable) return;
  const pass = readStoredPassphrase();
  if (!pass) return;

  const ws = new WebSocket(configWsUrl());
  configWs = ws;

  ws.onopen = () => ws.send(pass);
  setConfigWsStatus("connecting");

  const serverValues = new Map<string, string>();

  ws.onmessage = (ev) => {
    const msg = String(ev.data);
    if (msg === "auth") {
      // Auth rejected — stop reconnecting and navigate back to login.
      configWs = null;
      configReady = false;
      ws.onclose = null;
      ws.close();
      clearStoredPassphrase();
      // App listens to hashchange to re-evaluate passphrase state.
      window.dispatchEvent(new Event("hashchange"));
      return;
    }
    if (msg === "busy") {
      // The gateway's auth throttle refused this handshake without checking
      // the passphrase — a peer lockout or the concurrent-handshake cap. The
      // stored credential is still good, so keep it and retry; clearing it
      // here would drop the user at the login screen for a transient server
      // condition, and the login attempt would fail for the same reason.
      configWs = null;
      configReady = false;
      ws.onclose = null;
      ws.close();
      setConfigWsStatus("connecting");
      scheduleConfigReconnect();
      return;
    }
    if (msg === "ok") {
      configEverAuthed = true;
      configReconnectDelay = CONFIG_RECONNECT_MIN_MS;
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
    if (msg.startsWith("roots:")) {
      setRootsSignal(parseRootsText(msg.slice("roots:".length)));
      return;
    }
    if (msg.startsWith("wt=")) {
      setWtCertHash(msg.slice(3));
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
    scheduleConfigReconnect();
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

/** Preferred glyph coverage gamma. See DEFAULT_TEXT_GAMMA. */
export function preferredTextGamma(): number {
  const q = new URLSearchParams(location.search).get("textGamma");
  const raw = q ?? readStorage(TEXT_GAMMA_KEY);
  if (raw) {
    const n = Number(raw);
    // Past ~2.5 the thinning eats stems outright, so refuse to render
    // unreadably; below 1 it fattens, which is a legitimate light-theme want.
    if (Number.isFinite(n) && n >= 0.5 && n <= 2.5) return n;
  }
  return DEFAULT_TEXT_GAMMA;
}

/**
 * The stack to use when the visitor has expressed no font preference.
 *
 * `DEFAULT_FONT` is deliberately `ui-monospace, monospace`: the app is
 * served by a blit server that ships no webfont, so the right answer there
 * is whatever the platform calls its terminal face. A host that *does* ship
 * one — blit.sh self-hosts JetBrains Mono for the whole site — wants the
 * embedded workspace on the same face as the page around it, and saying so
 * from the host beats hardcoding a webfont into a client that usually has
 * no way to fetch it.
 *
 * Page-level like the shell capabilities, and for the same reason: it
 * describes the document, not a component instance, and is set once before
 * mount. A stored or `?font=` choice still wins over it — this replaces the
 * fallback, not the preference.
 */
let pageDefaultFont = DEFAULT_FONT;

export function setDefaultFont(family: string): void {
  pageDefaultFont = family.trim() || DEFAULT_FONT;
}

export function defaultFont(): string {
  return pageDefaultFont;
}

export function preferredFont(): string {
  const q = new URLSearchParams(location.search).get("font");
  if (q?.trim()) return q.trim();
  const s = readStorage(FONT_KEY);
  if (s?.trim()) return s.trim();
  return pageDefaultFont;
}

/** Preferred audio muted state. Defaults to true (browser autoplay policy). */
export function preferredAudioMuted(): boolean {
  const s = readStorage(AUDIO_MUTED_KEY);
  if (s === "0") return false;
  // Default to muted — browsers require a user gesture before audio can play.
  return true;
}

/** Preferred audio bitrate in kbps. 0 = server default. */
export function preferredAudioBitrate(): number {
  const s = readStorage(AUDIO_BITRATE_KEY);
  if (s) {
    const n = parseInt(s, 10);
    if (n >= 0) return n;
  }
  return 0;
}

/** Preferred video quality.  0 = server default, 1–4 = presets,
 *  10–255 = custom AV1 quantizer. */
export function preferredVideoQuality(): number {
  const s = readStorage(VIDEO_QUALITY_KEY);
  if (s) {
    const n = parseInt(s, 10);
    if (n >= 0 && n <= 255) return n;
  }
  return 0;
}

/** Preferred surface streaming state.  Defaults to enabled. */
export function preferredSurfaceStreaming(): boolean {
  const s = readStorage(SURFACE_STREAMING_KEY);
  if (s === "0") return false;
  return true;
}

function preferredWidth(key: string, fallback: number): number {
  const n = parseInt(readStorage(key) ?? "", 10);
  return Number.isFinite(n) && n >= 160 ? n : fallback;
}

export function preferredLeftDockWidth(): number {
  return preferredWidth(LEFT_DOCK_WIDTH_KEY, 260);
}

export function preferredPreviewPanelWidth(): number {
  return preferredWidth(PREVIEW_PANEL_WIDTH_KEY, 160);
}

/** Whether the IDE dock is open. A stored choice wins either way; first
 *  run opens it wherever the viewport can afford the width — the dock is
 *  the workspace's front door, and arriving at a bare terminal hid the
 *  files/log/problems surface behind a shortcut nobody has learned yet.
 *  On a phone it would bury the terminal instead, so it starts closed
 *  there. */
export function preferredLeftDockOpen(): boolean {
  const raw = readStorage(LEFT_DOCK_OPEN_KEY);
  if (raw != null) return raw === "1";
  return typeof window !== "undefined" && window.innerWidth >= 768;
}

type LeftSection = "explorer" | "log" | "problems";

/**
 * The set of collapsed dock sections, persisted as a comma list.
 *
 * Absent (first run) collapses Problems, so Files — with its folded-in
 * changes — shows on its own. Commit Log is deliberately *not* in that list
 * even though it starts folded in practice: it folds because the root is not
 * a repository (`noRepo`, see dockSections), and that is a different
 * statement. A user collapse is a preference and outranks the auto-unfold, so
 * seeding one here left the log folded on entering a repo — permanently, for
 * anyone who never thought to click a header they had never seen open.
 */
export function preferredCollapsedSections(): LeftSection[] {
  const raw = readStorage(LEFT_COLLAPSED_KEY);
  if (raw == null) return ["problems"];
  // An id missing here is silently dropped on every reload, so the
  // section would come back expanded forever.
  const valid = new Set(["explorer", "log", "problems"]);
  return raw
    .split(",")
    .map((s) => s.trim())
    .filter((p): p is LeftSection => valid.has(p));
}
