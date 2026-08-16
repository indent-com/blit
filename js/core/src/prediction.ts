/**
 * The capture textarea as a mirror of the line being edited.
 *
 * A host text predictor (macOS inline predictive text) completes the text it
 * can *see* in the focused field.  A terminal normally lets it see nothing:
 * every printable key is encoded and forwarded at `keydown`, the default is
 * prevented, and the capture textarea is emptied after every event — so the
 * field is permanently zero-length and there is no prefix to predict from.
 *
 * Prediction mode lets those keys land in the field instead and forwards the
 * *difference* between the field and what has already been sent (the
 * "mirror").  The field then carries the line being typed, the host predicts
 * against it, and the proposal comes back as a selected tail that the caller
 * draws next to the terminal cursor.
 *
 * Two invariants keep this safe to put in front of a shell:
 *
 * - **Only ever append or truncate.**  A change that rewrites text already
 *   forwarded (an autocorrect substitution) is refused, not applied: the
 *   caller puts the field back.  This is what `autocorrect="off"` used to buy,
 *   preserved now that the attribute has to be on.
 * - **Never forward the proposal.**  Everything from `selectionStart` on is
 *   the host's suggestion, not the user's text; it reaches the pty only once
 *   the user accepts it and it becomes part of the committed prefix.
 */

/** What the capture field looks like at the moment of an input event. */
export interface CaptureState {
  value: string;
  /** Caret, or the start of the proposed range when one is up. */
  selectionStart: number;
  selectionEnd: number;
  /** `isComposing`, or an equivalent composition-lifecycle flag. */
  composing: boolean;
  /** `InputEvent.inputType`, "" when the state change came from elsewhere
   *  (a `compositionend`, say). */
  inputType: string;
}

/** What the caller should do about it. */
export interface CaptureDelta {
  /** DELs (0x7f) to forward first — the user truncated the line. */
  deletes: number;
  /** Text to forward after them. */
  send: string;
  /** The mirror's new value, or `null` to leave both mirror and field as they
   *  are (the change is not ours to act on yet). */
  mirror: string | null;
  /** The tail the host is proposing: chip content, "" for no chip. */
  suggestion: string;
  /** The change rewrote already-forwarded text and was refused: the caller
   *  must write the mirror back into the field. */
  restore: boolean;
}

const HOLD: CaptureDelta = {
  deletes: 0,
  send: "",
  mirror: null,
  suggestion: "",
  restore: false,
};

/**
 * Reconcile the capture field against `mirror` — the text already forwarded.
 *
 * A real IME composition (romaji resolving to kana, a dead key waiting for
 * its base letter) is *held*: it has no proposed tail, its intermediate
 * states are not text the user typed, and forwarding them would put every
 * romaji on the shell's line only to delete it again.  A host proposal is
 * told apart from one by exactly that — it arrives as a selected range the
 * user did not type.
 */
export function captureDelta(
  mirror: string,
  state: CaptureState,
): CaptureDelta {
  const value = state.value;
  const selStart = Math.max(0, Math.min(state.selectionStart, value.length));
  const selEnd = Math.max(selStart, Math.min(state.selectionEnd, value.length));
  const suggestion = value.slice(selStart, selEnd);

  // A composition with nothing proposed is a composition, not a prediction.
  if (state.composing && !suggestion) return HOLD;

  const committed = value.slice(0, selStart);

  if (committed.startsWith(mirror)) {
    return {
      deletes: 0,
      send: committed.slice(mirror.length),
      mirror: committed,
      suggestion,
      restore: false,
    };
  }

  // Truncation — the user is deleting back through what they typed.  Only
  // honour it when the event says so: a shorter value from anything else is a
  // rewrite wearing a deletion's clothes.
  if (mirror.startsWith(committed) && state.inputType.startsWith("delete")) {
    return {
      deletes: mirror.length - committed.length,
      send: "",
      mirror: committed,
      suggestion,
      restore: false,
    };
  }

  // Anything else rewrites text the pty already has.  Refuse it.
  return { deletes: 0, send: "", mirror: null, suggestion: "", restore: true };
}
