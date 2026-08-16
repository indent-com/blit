import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  type AudioSessionType,
  claimPlaybackAudioSession,
  releaseRecordingAudioSession,
  resetAudioSessionForTests,
  retainRecordingAudioSession,
} from "../audioSession.js";

/** Stands in for Safari's `navigator.audioSession`. */
function fakeNavigator(): { audioSession: { type: AudioSessionType } } {
  return { audioSession: { type: "auto" } };
}

let target: ReturnType<typeof fakeNavigator>;

beforeEach(() => {
  resetAudioSessionForTests();
  target = fakeNavigator();
  vi.stubGlobal("navigator", target);
});

afterEach(() => {
  vi.unstubAllGlobals();
  resetAudioSessionForTests();
});

describe("audio session category", () => {
  it("declares playback rather than leaving Safari on auto", () => {
    claimPlaybackAudioSession();
    expect(target.audioSession.type).toBe("playback");
  });

  it("returns to playback when a capture stops", () => {
    claimPlaybackAudioSession();
    retainRecordingAudioSession();
    expect(target.audioSession.type).toBe("play-and-record");

    // The bug: a headset left on the bidirectional profile after unsharing.
    releaseRecordingAudioSession();
    expect(target.audioSession.type).toBe("playback");
  });

  it("keeps recording while a second capture is still live", () => {
    claimPlaybackAudioSession();
    retainRecordingAudioSession();
    retainRecordingAudioSession();

    releaseRecordingAudioSession();
    expect(target.audioSession.type).toBe("play-and-record");

    releaseRecordingAudioSession();
    expect(target.audioSession.type).toBe("playback");
  });

  it("stays recording across the share handoff", () => {
    // The share path claims the session before getUserMedia, because iOS ends
    // a microphone track created under a playback-only session. The capture
    // then takes its own claim and the share's is dropped in a finally: if
    // that ordering ever inverts, the type dips to playback mid-handoff and
    // kills the track it was meant to enable.
    claimPlaybackAudioSession();

    retainRecordingAudioSession(); // share(), before getUserMedia
    expect(target.audioSession.type).toBe("play-and-record");

    retainRecordingAudioSession(); // PcmMicrophoneCapture.start()
    releaseRecordingAudioSession(); // share()'s finally
    expect(target.audioSession.type).toBe("play-and-record");

    releaseRecordingAudioSession(); // PcmMicrophoneCapture.stop()
    expect(target.audioSession.type).toBe("playback");
  });

  it("re-arms recording after a stop, so capture can be turned back on", () => {
    claimPlaybackAudioSession();
    retainRecordingAudioSession();
    releaseRecordingAudioSession();
    expect(target.audioSession.type).toBe("playback");

    // A restart (codec change, device change) shares again from playback.
    retainRecordingAudioSession();
    expect(target.audioSession.type).toBe("play-and-record");
  });

  it("ignores a release with no capture outstanding", () => {
    claimPlaybackAudioSession();
    releaseRecordingAudioSession();
    releaseRecordingAudioSession();
    expect(target.audioSession.type).toBe("playback");

    // A count driven negative would swallow the next real capture.
    retainRecordingAudioSession();
    expect(target.audioSession.type).toBe("play-and-record");
  });

  it("leaves the category alone until something plays", () => {
    retainRecordingAudioSession();
    releaseRecordingAudioSession();
    expect(target.audioSession.type).toBe("auto");
  });

  it("is inert on browsers without an audio session", () => {
    vi.stubGlobal("navigator", {});
    expect(() => {
      claimPlaybackAudioSession();
      retainRecordingAudioSession();
      releaseRecordingAudioSession();
    }).not.toThrow();
  });
});
