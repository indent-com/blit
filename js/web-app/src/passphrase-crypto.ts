import nacl from "tweetnacl";

const STORAGE_KEY = "blit-share-key";
const ENCRYPTED_PREFIX = "e.";

function base64urlEncode(bytes: Uint8Array): string {
  const binary = String.fromCharCode(...bytes);
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");
}

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

function getOrCreateKey(): Uint8Array {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored) {
    return hexDecode(stored);
  }
  const key = nacl.randomBytes(32);
  localStorage.setItem(STORAGE_KEY, hexEncode(key));
  return key;
}

export function encryptPassphrase(passphrase: string): string {
  const key = getOrCreateKey();
  const message = new TextEncoder().encode(passphrase);
  const nonce = nacl.randomBytes(24);
  const box = nacl.secretbox(message, nonce, key);
  const combined = new Uint8Array(nonce.length + box.length);
  combined.set(nonce);
  combined.set(box, nonce.length);
  return ENCRYPTED_PREFIX + base64urlEncode(combined);
}

export function isEncrypted(hash: string): boolean {
  return hash.startsWith(ENCRYPTED_PREFIX);
}

export function decryptPassphrase(ciphertext: string): string | null {
  try {
    const key = getOrCreateKey();
    const combined = base64urlDecode(ciphertext.slice(ENCRYPTED_PREFIX.length));
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
