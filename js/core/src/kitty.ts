import { encoder, keyToBytes } from "./keyboard";

/**
 * Kitty keyboard protocol (CSI u) encoding.
 *
 * All byte-level rules for the modern keyboard protocol live here; the legacy
 * xterm encoder in `keyboard.ts` delegates to `encodeKittyKey` whenever the
 * terminal has negotiated a non-zero flag set.  The encoding mirrors
 * alacritty's `SequenceBuilder` (alacritty/src/input/keyboard.rs in the fork),
 * scoped to the three flags blit currently supports.
 *
 * Flag bits (see the kitty spec):
 *   1 = disambiguate escape codes
 *   2 = report event types (press / repeat / release)
 *   4 = report alternate keys (shifted / base-layout codepoints)
 * Bits 8 (report-associated-text) and 16 (report-all-as-escape-codes) are
 * intentionally masked off by the caller — the encoder API still takes the full
 * integer so those can be wired up later without changing the signature.
 */
export const KITTY_DISAMBIGUATE = 1;
export const KITTY_EVENT_TYPES = 2;
export const KITTY_ALTERNATE = 4;
export const KITTY_SUPPORTED_MASK = 0b111;

export type KittyEventType = "press" | "repeat" | "release";

/** Codepoints for the three "editing" keys that stay legacy when unmodified. */
const EDIT_KEYS: Record<string, number> = {
  Enter: 13,
  Tab: 9,
  Backspace: 127,
};

/** Legacy byte for an unmodified editing key press/repeat. */
const EDIT_LEGACY: Record<string, string> = {
  Enter: "\r",
  Tab: "\t",
  Backspace: "\x7f",
};

/** Functional keys that use the `CSI 1 ; mods LETTER` form. */
const LETTER_KEYS: Record<string, string> = {
  ArrowUp: "A",
  ArrowDown: "B",
  ArrowRight: "C",
  ArrowLeft: "D",
  Home: "H",
  End: "F",
  F1: "P",
  F2: "Q",
  F3: "R",
  F4: "S",
};

/** Functional keys that use the `CSI number ; mods ~` form. */
const TILDE_KEYS: Record<string, string> = {
  Insert: "2",
  Delete: "3",
  PageUp: "5",
  PageDown: "6",
  F5: "15",
  F6: "17",
  F7: "18",
  F8: "19",
  F9: "20",
  F10: "21",
  F11: "23",
  F12: "24",
};

function modifierBitmask(e: KeyboardEvent): number {
  return (
    (e.shiftKey ? 1 : 0) +
    (e.altKey ? 2 : 0) +
    (e.ctrlKey ? 4 : 0) +
    (e.metaKey ? 8 : 0)
  );
}

/**
 * The `;<mods+1>[:<event>]` suffix shared by every CSI-u / functional form.
 * Emitted whenever there are modifiers or an event-type subfield to report;
 * the empty string otherwise (e.g. a bare `CSI 27 u` for unmodified Escape).
 */
function modSuffix(
  bitmask: number,
  eventTypeActive: boolean,
  eventType: KittyEventType,
): string {
  if (bitmask === 0 && !eventTypeActive) return "";
  let out = `;${bitmask + 1}`;
  if (eventTypeActive) out += eventType === "repeat" ? ":2" : ":3";
  return out;
}

/** Best-effort base-layout codepoint from `e.code` for the alternate-key field. */
function baseLayoutCodepoint(code: string): number | null {
  if (code.length === 4 && code.startsWith("Key")) {
    return code.charCodeAt(3) + 32; // "KeyA" → 'a'
  }
  if (code.length === 6 && code.startsWith("Digit")) {
    return code.charCodeAt(5); // "Digit1" → '1'
  }
  return null;
}

/**
 * Encode a keyboard event as a kitty CSI-u sequence.  Returns null when the
 * event must not be forwarded (modifier-only keys; any release without event
 * reporting; text-key and editing-key releases).
 *
 * `flags` is the full negotiated integer; only the supported bits are honoured.
 * `appCursor` only affects the legacy fallback for unmodified functional keys.
 */
export function encodeKittyKey(
  e: KeyboardEvent,
  flags: number,
  eventType: KittyEventType,
  appCursor: boolean,
): Uint8Array | null {
  const key = e.key;

  // Modifier-only keys are never forwarded on their own.
  if (key === "Shift" || key === "Control" || key === "Alt" || key === "Meta") {
    return null;
  }

  const hasEventTypes = (flags & KITTY_EVENT_TYPES) !== 0;
  const hasAlternate = (flags & KITTY_ALTERNATE) !== 0;

  // Releases can only be represented when event reporting is on.
  if (eventType === "release" && !hasEventTypes) return null;

  const bitmask = modifierBitmask(e);
  // Event types are only *reported* on the modifier param for repeat/release;
  // a press carries no subfield.
  const eventTypeActive = hasEventTypes && eventType !== "press";
  const suffix = () => modSuffix(bitmask, eventTypeActive, eventType);

  // --- Editing keys: Enter / Tab / Backspace ----------------------------
  if (key in EDIT_KEYS) {
    if (eventType === "release") return null; // never report their release
    if (bitmask === 0) return encoder.encode(EDIT_LEGACY[key]); // legacy CR/HT/DEL
    return encoder.encode(`\x1b[${EDIT_KEYS[key]}${suffix()}u`);
  }

  // --- Escape: always CSI-u --------------------------------------------
  if (key === "Escape") {
    return encoder.encode(`\x1b[27${suffix()}u`);
  }

  // --- Text keys (single character) ------------------------------------
  if (key.length === 1) {
    const withMod = e.ctrlKey || e.altKey || e.metaKey;
    if (!withMod) {
      // Plain text (shift only, or nothing) is delivered as-is; no releases.
      if (eventType === "release") return null;
      return encoder.encode(key);
    }
    // ctrl / alt / super + char → CSI u on the unshifted codepoint.
    if (eventType === "release") return null; // text keys carry no release
    const unshifted = key.toLowerCase().codePointAt(0)!;
    let field = `${unshifted}`;
    if (hasAlternate) {
      const shifted = key.codePointAt(0)!;
      const base = baseLayoutCodepoint(e.code);
      const includeShifted = shifted !== unshifted;
      const includeBase = base !== null && base !== unshifted;
      if (includeShifted || includeBase) {
        field = includeShifted ? `${unshifted}:${shifted}` : `${unshifted}:`;
        if (includeBase) field += `:${base}`;
      }
    }
    return encoder.encode(`\x1b[${field}${suffix()}u`);
  }

  // --- Functional keys: arrows / nav / F-keys --------------------------
  const needsKitty = bitmask !== 0 || eventTypeActive;
  if (key in LETTER_KEYS) {
    if (!needsKitty) return keyToBytes(e, appCursor); // legacy incl. appCursor SS3
    return encoder.encode(`\x1b[1${suffix()}${LETTER_KEYS[key]}`);
  }
  if (key in TILDE_KEYS) {
    if (!needsKitty) return keyToBytes(e, appCursor);
    return encoder.encode(`\x1b[${TILDE_KEYS[key]}${suffix()}~`);
  }

  // Anything else (unknown/dead keys, etc.) falls back to the legacy encoder,
  // which returns null when it too has nothing to send.
  return keyToBytes(e, appCursor);
}
