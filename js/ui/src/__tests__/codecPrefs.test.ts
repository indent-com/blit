import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// Codec negotiation is configurable in both directions: which codecs this
// device accepts for surface video, and which it uses to send camera and
// microphone. All four preferences are device-local, like the rest of the
// media settings — decoder and encoder support is a fact about this machine.

const LEGACY_OPUS_KEY = "blit.media.microphone.opus";
const MICROPHONE_CODEC_KEY = "blit.microphoneCodec";

/** storage.ts runs its migrations at module scope, so each case needs a
 *  freshly imported copy. */
async function freshStorage() {
  vi.resetModules();
  return await import("../storage");
}

/** The sandbox environment has no working `localStorage`, so provide one. */
function stubLocalStorage() {
  const map = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
    removeItem: (k: string) => void map.delete(k),
    clear: () => map.clear(),
  });
}

describe("codec preferences", () => {
  beforeEach(stubLocalStorage);
  afterEach(() => vi.unstubAllGlobals());

  it("defaults every axis to no opinion", async () => {
    const storage = await freshStorage();
    expect(storage.preferredSurfaceCodecs()).toBe(0);
    expect(storage.preferredCameraCodec()).toBe("auto");
    expect(storage.preferredCameraChroma()).toBe("auto");
    expect(storage.preferredMicrophoneCodec()).toBe("auto");
  });

  it("reads back a stored selection", async () => {
    localStorage.setItem("blit.surfaceCodecs", "10");
    localStorage.setItem("blit.cameraCodec", "av1");
    localStorage.setItem("blit.cameraChroma", "444");
    localStorage.setItem(MICROPHONE_CODEC_KEY, "opus");
    const storage = await freshStorage();
    expect(storage.preferredSurfaceCodecs()).toBe(10);
    expect(storage.preferredCameraCodec()).toBe("av1");
    expect(storage.preferredCameraChroma()).toBe("444");
    expect(storage.preferredMicrophoneCodec()).toBe("opus");
  });

  it("falls back to auto on a value it does not know", async () => {
    // A preference written by a newer build must not wedge an older one, and
    // an out-of-range mask must not reach the wire.
    localStorage.setItem("blit.cameraCodec", "vp9");
    localStorage.setItem("blit.cameraChroma", "422");
    localStorage.setItem(MICROPHONE_CODEC_KEY, "mp3");
    localStorage.setItem("blit.surfaceCodecs", "999");
    const storage = await freshStorage();
    expect(storage.preferredCameraCodec()).toBe("auto");
    expect(storage.preferredCameraChroma()).toBe("auto");
    expect(storage.preferredMicrophoneCodec()).toBe("auto");
    expect(storage.preferredSurfaceCodecs()).toBe(0);
  });
});

describe("blit.media.microphone.opus migration", () => {
  beforeEach(stubLocalStorage);
  afterEach(() => vi.unstubAllGlobals());

  it("turns an unchecked Opus box into an explicit PCM choice", async () => {
    localStorage.setItem(LEGACY_OPUS_KEY, "0");
    const storage = await freshStorage();

    expect(storage.preferredMicrophoneCodec()).toBe("pcm");
    expect(localStorage.getItem(MICROPHONE_CODEC_KEY)).toBe("pcm");
    // Read once, then gone — the legacy key is not a permanent read path.
    expect(localStorage.getItem(LEGACY_OPUS_KEY)).toBeNull();
  });

  it("turns a checked box into auto, not into an explicit Opus", async () => {
    // The checkbox meant "prefer Opus", and the store still falls back to PCM
    // when this browser cannot encode it. Writing "opus" would remove that
    // fallback and leave the viewer with no microphone at all.
    localStorage.setItem(LEGACY_OPUS_KEY, "1");
    const storage = await freshStorage();

    expect(storage.preferredMicrophoneCodec()).toBe("auto");
    expect(localStorage.getItem(LEGACY_OPUS_KEY)).toBeNull();
  });

  it("does not overwrite a codec chosen after the upgrade", async () => {
    localStorage.setItem(LEGACY_OPUS_KEY, "0");
    localStorage.setItem(MICROPHONE_CODEC_KEY, "opus");
    const storage = await freshStorage();

    expect(storage.preferredMicrophoneCodec()).toBe("opus");
    expect(localStorage.getItem(LEGACY_OPUS_KEY)).toBeNull();
  });
});
