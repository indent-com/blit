export const PASSPHRASE_KEY = "blit-passphrase";

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
