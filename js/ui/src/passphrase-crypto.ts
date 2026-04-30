import nacl from "tweetnacl";

// Legacy decrypt-only support — secrets are now stored in localStorage and
// no longer encrypted into URL fragments. These helpers exist so older
// share/auth URLs in browser history or bookmarks still work on first load.

const STORAGE_KEY = "blit-share-key";
const ENCRYPTED_PREFIX = "e=";
const LEGACY_PREFIX = "e.";

function base64urlDecode(str: string): Uint8Array {
  const padded = str.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

function hexEncode(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function hexDecode(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.slice(i, i + 2), 16);
  }
  return bytes;
}

function readKey(): Uint8Array | null {
  const stored = localStorage.getItem(STORAGE_KEY);
  return stored ? hexDecode(stored) : null;
}

export function isEncrypted(hash: string): boolean {
  return hash.startsWith(ENCRYPTED_PREFIX) || hash.startsWith(LEGACY_PREFIX);
}

export function decryptPassphrase(ciphertext: string): string | null {
  try {
    const key = readKey();
    if (!key) return null;
    const prefix = ciphertext.startsWith(ENCRYPTED_PREFIX)
      ? ENCRYPTED_PREFIX
      : LEGACY_PREFIX;
    const combined = base64urlDecode(ciphertext.slice(prefix.length));
    if (combined.length < 25) return null;
    const nonce = combined.slice(0, 24);
    const box = combined.slice(24);
    const message = nacl.secretbox.open(box, nonce, key);
    if (!message) return null;
    return new TextDecoder().decode(message);
  } catch {
    return null;
  }
}
