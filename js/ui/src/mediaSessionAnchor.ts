/**
 * Gives WebKit a media element to route remote commands through.
 *
 * blit plays audio entirely through an `AudioContext`, with no `<audio>` or
 * `<video>` anywhere. That is enough for WebKit to publish Now Playing metadata
 * — the iPadOS panel shows the track and artist — but not enough for it to
 * deliver the panel's transport commands: WebKit routes those to a playing
 * `HTMLMediaElement`, and with none present, play/pause fall back to its own
 * handling of the audio context while next/previous have nothing to reach. The
 * page's `setActionHandler` callbacks are registered and simply never fire, so
 * the buttons appear live and do nothing.
 *
 * A silent element playing on loop supplies the missing routing target. It is
 * inaudible and costs one decode of a one-second buffer, but it does claim an
 * audio session, so it runs only while a controllable player is on screen and
 * stops as soon as one is not.
 */

/** Sample rate and depth are as low as a WAV can legally go: nothing decodes
 *  this buffer for its content, only for its existence. */
const ANCHOR_RATE = 8_000;

/**
 * A one-second silent 8-bit mono WAV.
 *
 * Built rather than embedded as base64 so the header stays readable. Note that
 * silence in unsigned 8-bit PCM is mid-scale (128), not zero — a zero-filled
 * buffer is full-negative DC, which is not silent.
 */
export function silentWav(): Uint8Array {
  const samples = ANCHOR_RATE;
  const bytes = new Uint8Array(44 + samples);
  const view = new DataView(bytes.buffer);
  const ascii = (offset: number, text: string) => {
    for (let index = 0; index < text.length; index++) {
      view.setUint8(offset + index, text.charCodeAt(index));
    }
  };
  ascii(0, "RIFF");
  view.setUint32(4, 36 + samples, true);
  ascii(8, "WAVE");
  ascii(12, "fmt ");
  view.setUint32(16, 16, true); // PCM header length
  view.setUint16(20, 1, true); // PCM, uncompressed
  view.setUint16(22, 1, true); // mono
  view.setUint32(24, ANCHOR_RATE, true);
  view.setUint32(28, ANCHOR_RATE, true); // byte rate: rate x channels x depth
  view.setUint16(32, 1, true); // block align
  view.setUint16(34, 8, true); // bits per sample
  ascii(36, "data");
  view.setUint32(40, samples, true);
  bytes.fill(128, 44);
  return bytes;
}

/**
 * Whether this browser needs the anchor.
 *
 * Only WebKit withholds the commands, and an always-on anchor would claim an
 * audio session on engines that already work — so this is deliberately narrow.
 * `navigator.audioSession` is the same WebKit-only marker `audioSession.ts`
 * relies on; the cost is that a WebKit old enough to lack it goes unanchored,
 * which leaves those viewers exactly where they are today rather than worse.
 */
export function anchorNeeded(
  navigatorLike: Navigator | undefined = globalThis.navigator,
): boolean {
  if (!navigatorLike) return false;
  return "mediaSession" in navigatorLike && "audioSession" in navigatorLike;
}

export interface RemoteCommandAnchor {
  /** Hold the audio session so remote commands are delivered. */
  engage(): void;
  /** Release it: nothing on screen can be controlled. */
  release(): void;
  dispose(): void;
}

/**
 * `play()` needs a user gesture, and the first controllable player usually
 * appears without one, so a rejected start is retried once on the next
 * interaction rather than abandoned.
 */
export function createRemoteCommandAnchor(
  documentLike: Document = globalThis.document,
): RemoteCommandAnchor | null {
  if (!anchorNeeded()) return null;
  const element = documentLike.createElement("audio");
  element.loop = true;
  element.preload = "auto";
  // Not `muted`: WebKit does not treat a muted element as audio worth routing
  // commands to. The buffer itself is silent instead.
  element.volume = 1;
  const url = URL.createObjectURL(
    new Blob([silentWav() as BlobPart], { type: "audio/wav" }),
  );
  element.src = url;
  let wanted = false;
  let pendingGesture: (() => void) | null = null;

  const dropGesture = () => {
    if (!pendingGesture) return;
    documentLike.removeEventListener("pointerdown", pendingGesture);
    documentLike.removeEventListener("keydown", pendingGesture);
    pendingGesture = null;
  };
  const start = () => {
    void element.play().catch(() => {
      if (!wanted || pendingGesture) return;
      pendingGesture = () => {
        dropGesture();
        if (wanted) start();
      };
      documentLike.addEventListener("pointerdown", pendingGesture, {
        once: true,
      });
      documentLike.addEventListener("keydown", pendingGesture, { once: true });
    });
  };

  return {
    engage() {
      if (wanted) return;
      wanted = true;
      start();
    },
    release() {
      if (!wanted) return;
      wanted = false;
      dropGesture();
      element.pause();
    },
    dispose() {
      wanted = false;
      dropGesture();
      element.pause();
      element.removeAttribute("src");
      URL.revokeObjectURL(url);
    },
  };
}
