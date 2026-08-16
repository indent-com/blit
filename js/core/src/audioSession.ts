/**
 * iOS audio session category.
 *
 * On iOS the Bluetooth profile follows the page's audio session category, not
 * what the page is actually doing: `play-and-record` puts the headset on the
 * bidirectional HFP/SCO link — mono, narrowband, audibly worse — while
 * `playback` leaves it on A2DP. Safari's default `"auto"` infers the category
 * from capture, but does not reliably hand it back once the capture ends, so a
 * viewer who shared a microphone once keeps listening through the degraded
 * profile for the rest of the page's life. Declaring the category is the only
 * way back onto A2DP.
 *
 * Every other browser leaves `navigator.audioSession` undefined and pays a
 * property read.
 */

export type AudioSessionType =
  | "auto"
  | "playback"
  | "transient"
  | "transient-solo"
  | "ambient"
  | "play-and-record";

interface AudioSessionLike {
  type: AudioSessionType;
}

function audioSession(): AudioSessionLike | null {
  const target = globalThis.navigator as
    | (Navigator & { audioSession?: AudioSessionLike })
    | undefined;
  return target?.audioSession ?? null;
}

/**
 * Live captures are counted rather than flagged: two connections can hold a
 * microphone at once, and the first one to stop must not hand the session back
 * to playback while the other is still recording.
 */
let captures = 0;

/** Sticky: a document that has played audio once is still a playback document
 *  when the player is torn down and rebuilt, which happens on every device
 *  change. */
let playing = false;

function apply(): void {
  const session = audioSession();
  if (!session) return;
  const wanted: AudioSessionType =
    captures > 0 ? "play-and-record" : playing ? "playback" : "auto";
  try {
    if (session.type !== wanted) session.type = wanted;
  } catch {
    // Advisory. A browser that refuses the assignment still plays — it just
    // plays on whichever profile it picked for itself.
  }
}

/**
 * Declare that this document plays audio.
 *
 * Call before creating the `AudioContext`: iOS routes the output when the
 * context is created, so a category set afterwards only takes effect on the
 * next one.
 */
export function claimPlaybackAudioSession(): void {
  playing = true;
  apply();
}

/** Hold the session in a recording-capable category for one live capture. */
export function retainRecordingAudioSession(): void {
  captures += 1;
  apply();
}

/** Drop one capture's claim, returning to playback when the last one ends. */
export function releaseRecordingAudioSession(): void {
  if (captures === 0) return;
  captures -= 1;
  apply();
}

/** Test seam: forget both claims. */
export function resetAudioSessionForTests(): void {
  captures = 0;
  playing = false;
}
