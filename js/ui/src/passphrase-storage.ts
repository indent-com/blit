export const PASSPHRASE_KEY = "blit-passphrase";

// Legacy per-browser encryption key from the old encrypted-URL scheme.
// Removed unconditionally on first load; the secret itself is now persisted
// directly under PASSPHRASE_KEY.
const LEGACY_ENCRYPTION_KEY = "blit-share-key";

try {
  localStorage.removeItem(LEGACY_ENCRYPTION_KEY);
} catch {}

export function readStoredPassphrase(): string | null {
  try {
    return localStorage.getItem(PASSPHRASE_KEY);
  } catch {
    return null;
  }
}

export function clearStoredPassphrase(): void {
  try {
    localStorage.removeItem(PASSPHRASE_KEY);
  } catch {}
}
