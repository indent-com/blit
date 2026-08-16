import { describe, expect, it, vi } from "vitest";
import {
  anchorNeeded,
  createRemoteCommandAnchor,
  silentWav,
} from "../mediaSessionAnchor";

const ascii = (bytes: Uint8Array, offset: number, length: number) =>
  String.fromCharCode(...bytes.subarray(offset, offset + length));

describe("media session anchor", () => {
  it("builds a WAV a browser will actually decode", () => {
    const wav = silentWav();
    const view = new DataView(wav.buffer, wav.byteOffset, wav.byteLength);

    expect(ascii(wav, 0, 4)).toBe("RIFF");
    expect(ascii(wav, 8, 4)).toBe("WAVE");
    expect(ascii(wav, 12, 4)).toBe("fmt ");
    expect(ascii(wav, 36, 4)).toBe("data");
    // Declared lengths must match the buffer, or the decode is rejected.
    expect(view.getUint32(4, true)).toBe(wav.byteLength - 8);
    expect(view.getUint32(40, true)).toBe(wav.byteLength - 44);
    expect(view.getUint16(22, true)).toBe(1);
    expect(view.getUint16(34, true)).toBe(8);
  });

  it("is silent, which in unsigned 8-bit PCM is mid-scale and not zero", () => {
    const wav = silentWav();
    const samples = wav.subarray(44);

    expect(samples.length).toBeGreaterThan(0);
    expect(samples.every((sample) => sample === 128)).toBe(true);
  });

  it("anchors only the engine that withholds the commands", () => {
    // WebKit: publishes Now Playing but routes commands to a media element.
    expect(anchorNeeded({ mediaSession: {}, audioSession: {} } as never)).toBe(
      true,
    );
    // Everyone else already delivers them; claiming an audio session would be
    // a regression on a platform that works.
    expect(anchorNeeded({ mediaSession: {} } as never)).toBe(false);
    expect(anchorNeeded({} as never)).toBe(false);
    expect(anchorNeeded(undefined)).toBe(false);
  });

  it("retries a gesture-blocked start once the viewer interacts", async () => {
    const listeners = new Map<string, () => void>();
    const element = {
      loop: false,
      preload: "",
      volume: 0,
      src: "",
      play: vi.fn(() => Promise.reject(new Error("gesture required"))),
      pause: vi.fn(),
      removeAttribute: vi.fn(),
    };
    const documentLike = {
      createElement: () => element,
      addEventListener: (name: string, fn: () => void) =>
        listeners.set(name, fn),
      removeEventListener: (name: string) => listeners.delete(name),
    } as unknown as Document;
    vi.stubGlobal("navigator", { mediaSession: {}, audioSession: {} });
    vi.stubGlobal("URL", {
      createObjectURL: () => "blob:anchor",
      revokeObjectURL: () => undefined,
    });
    vi.stubGlobal("Blob", class {});

    const anchor = createRemoteCommandAnchor(documentLike);
    expect(anchor).not.toBeNull();
    anchor?.engage();
    await Promise.resolve();
    await Promise.resolve();

    expect(element.play).toHaveBeenCalledTimes(1);
    // A rejected autoplay must leave a way back, not give up silently.
    expect(listeners.has("pointerdown")).toBe(true);
    element.play.mockResolvedValue(undefined as never);
    listeners.get("pointerdown")?.();
    expect(element.play).toHaveBeenCalledTimes(2);

    // Releasing drops the pending retry so a later gesture cannot resurrect a
    // session no player is asking for.
    anchor?.release();
    expect(element.pause).toHaveBeenCalled();
    vi.unstubAllGlobals();
  });
});
