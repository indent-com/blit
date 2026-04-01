import { PALETTES, DEFAULT_FONT } from "blit-react";
import type { TerminalPalette } from "blit-react";

export const PASS_KEY = "blit.passphrase";
export const HOST_KEY = "blit.host";
export const PALETTE_KEY = "blit.palette";
export const FONT_KEY = "blit.fontFamily";
export const FONT_SIZE_KEY = "blit.fontSize";
export const FONT_SMOOTHING_KEY = "blit.fontSmoothing";

/** Remote hostname: injected by CLI, or falls back to location.hostname for gateway. */
export function blitHost(): string {
  return readStorage(HOST_KEY) || location.hostname;
}

export function readStorage(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}
export function writeStorage(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {}
}

/** Gateway host — in dev mode, Vite injects VITE_BLIT_GATEWAY to point to the gateway port. */
const gatewayHost = import.meta.env.VITE_BLIT_GATEWAY ?? location.host;

/** Base path for API requests. In dev mode, points to the gateway; in production, relative to the page. */
export const basePath =
  gatewayHost !== location.host
    ? `//${gatewayHost}/`
    : location.pathname.endsWith("/")
      ? location.pathname
      : location.pathname + "/";

export function wsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return proto + "//" + gatewayHost + location.pathname;
}

export function wtUrl(): string {
  return "https://" + gatewayHost + location.pathname;
}

/** SHA-256 cert hash injected by the gateway for self-signed certs. */
export function wtCertHash(): string | undefined {
  return (window as unknown as { __blitCertHash?: string }).__blitCertHash;
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
