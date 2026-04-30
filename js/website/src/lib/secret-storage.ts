export const SHARE_SECRET_KEY = "blit-share-secret";

// Legacy per-browser encryption key from the old encrypted-URL scheme.
// Removed unconditionally on first load.
const LEGACY_ENCRYPTION_KEY = "blit-share-key";

try {
  localStorage.removeItem(LEGACY_ENCRYPTION_KEY);
} catch {}

export function readStoredSecret(): string | null {
  try {
    return localStorage.getItem(SHARE_SECRET_KEY);
  } catch {
    return null;
  }
}

export function writeStoredSecret(secret: string): void {
  try {
    localStorage.setItem(SHARE_SECRET_KEY, secret);
  } catch {}
}
