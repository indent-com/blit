export const SHARE_SECRET_KEY = "blit-share-secret";

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
