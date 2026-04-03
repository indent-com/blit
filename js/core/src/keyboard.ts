export const encoder = new TextEncoder();

/**
 * Convert a single character to its Ctrl+char byte representation.
 * For a-z returns 0x01–0x1a, for special chars returns the standard mapping.
 * Returns null if the character has no Ctrl equivalent.
 */
export function ctrlCharToByte(char: string): Uint8Array | null {
  if (char.length !== 1) return null;
  const code = char.toLowerCase().charCodeAt(0);
  if (code >= 97 && code <= 122) return new Uint8Array([code - 96]); // a-z → 0x01-0x1a
  if (char === "[") return new Uint8Array([0x1b]); // Ctrl+[ = Escape
  if (char === "\\") return new Uint8Array([0x1c]);
  if (char === "]") return new Uint8Array([0x1d]);
  if (char === " " || char === "@") return new Uint8Array([0x00]); // Ctrl+Space / Ctrl+@
  return null;
}

/**
 * Encode a keyboard event into the byte sequence expected by the terminal.
 * Returns null if the event should not be forwarded.
 */
export function keyToBytes(
  e: KeyboardEvent,
  appCursor: boolean,
): Uint8Array | null {
  if (e.ctrlKey && !e.altKey && !e.metaKey) {
    const kc = e.key.charCodeAt(0);
    if (e.key.length === 1 && kc >= 1 && kc <= 26) return new Uint8Array([kc]);
    if (e.key.length === 1) {
      const code = e.key.toLowerCase().charCodeAt(0);
      if (code >= 97 && code <= 122) return new Uint8Array([code - 96]);
      if (e.key === "[") return new Uint8Array([0x1b]);
      if (e.key === "\\") return new Uint8Array([0x1c]);
      if (e.key === "]") return new Uint8Array([0x1d]);
    }
    // Fallback: use e.code when e.key is unhelpful (e.g. macOS Ctrl+letter)
    if (e.code && e.code.startsWith("Key")) {
      const cc = e.code.charCodeAt(3);
      if (cc >= 65 && cc <= 90) return new Uint8Array([cc - 64]);
    }
    if (e.code === "BracketLeft") return new Uint8Array([0x1b]);
    if (e.code === "Backslash") return new Uint8Array([0x1c]);
    if (e.code === "BracketRight") return new Uint8Array([0x1d]);
  }

  if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey) {
    if (e.key === "?") return new Uint8Array([0x7f]);
    if (e.key === " " || e.key === "@") return new Uint8Array([0x00]);
  }

  const arrows: Record<string, string> = {
    ArrowUp: "A",
    ArrowDown: "B",
    ArrowRight: "C",
    ArrowLeft: "D",
  };
  if (arrows[e.key]) {
    const mod =
      (e.shiftKey ? 1 : 0) +
      (e.altKey ? 2 : 0) +
      (e.ctrlKey ? 4 : 0) +
      (e.metaKey ? 8 : 0);
    if (mod) return encoder.encode(`\x1b[1;${mod + 1}${arrows[e.key]}`);
    const prefix = appCursor ? "\x1bO" : "\x1b[";
    return encoder.encode(prefix + arrows[e.key]);
  }

  const mod =
    (e.shiftKey ? 1 : 0) +
    (e.altKey ? 2 : 0) +
    (e.ctrlKey ? 4 : 0) +
    (e.metaKey ? 8 : 0);

  const tilde: Record<string, string> = {
    PageUp: "5",
    PageDown: "6",
    Delete: "3",
    Insert: "2",
  };
  if (tilde[e.key]) {
    if (mod) return encoder.encode(`\x1b[${tilde[e.key]};${mod + 1}~`);
    return encoder.encode(`\x1b[${tilde[e.key]}~`);
  }

  const he: Record<string, string> = { Home: "H", End: "F" };
  if (he[e.key]) {
    if (mod) return encoder.encode(`\x1b[1;${mod + 1}${he[e.key]}`);
    return encoder.encode(`\x1b[${he[e.key]}`);
  }

  const f14: Record<string, string> = { F1: "P", F2: "Q", F3: "R", F4: "S" };
  if (f14[e.key]) {
    if (mod) return encoder.encode(`\x1b[1;${mod + 1}${f14[e.key]}`);
    return encoder.encode(`\x1bO${f14[e.key]}`);
  }

  const fkeys: Record<string, string> = {
    F5: "15",
    F6: "17",
    F7: "18",
    F8: "19",
    F9: "20",
    F10: "21",
    F11: "23",
    F12: "24",
  };
  if (fkeys[e.key]) {
    if (mod) return encoder.encode(`\x1b[${fkeys[e.key]};${mod + 1}~`);
    return encoder.encode(`\x1b[${fkeys[e.key]}~`);
  }

  const simple: Record<string, string> = {
    Enter: "\r",
    Backspace: "\x7f",
    Tab: "\t",
    Escape: "\x1b",
  };
  if (simple[e.key]) return encoder.encode(simple[e.key]);

  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.length === 1) {
    const code = e.key.charCodeAt(0);
    if (code >= 0x20 && code <= 0x7e) return encoder.encode("\x1b" + e.key);
    return encoder.encode(e.key);
  }

  if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
    return encoder.encode(e.key);
  }

  return null;
}
