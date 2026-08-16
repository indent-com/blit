import {
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  onMount,
} from "solid-js";
import {
  ACTIVE_CAMERA,
  ACTIVE_MICROPHONE,
  AUDIO_CODEC_OPUS,
  AUDIO_CODEC_PCM,
  AudioPlayer,
  RUNTIME_CAMERA,
  RUNTIME_MICROPHONE,
  VIDEO_CODEC_MJPEG,
  probeCameraCodecs,
  probeOpusMicrophone,
  releaseRecordingAudioSession,
  retainRecordingAudioSession,
  type BlitConnectionSnapshot,
  type BlitWorkspace,
  type CameraQuality,
  type ScreenCastState,
} from "@blit-sh/core";
import {
  CAMERA_CHROMA_KEY,
  CAMERA_CODEC_KEY,
  CAMERA_DEVICE_KEY,
  CAMERA_FRAME_RATE_KEY,
  CAMERA_QUALITY_KEY,
  CAMERA_RESOLUTION_KEY,
  MEDIA_TARGET_KEY,
  MICROPHONE_CODEC_KEY,
  MICROPHONE_DEVICE_KEY,
  SPEAKER_DEVICE_KEY,
  preferredCameraChroma,
  preferredCameraCodec,
  preferredCameraDevice,
  preferredCameraFrameRate,
  preferredCameraQuality,
  preferredCameraResolution,
  preferredMediaTarget,
  preferredMicrophoneCodec,
  preferredMicrophoneDevice,
  preferredSpeakerDevice,
  writeStorage,
  type CameraChromaPreference,
  type CameraCodecPreference,
  type MicrophoneCodecPreference,
} from "./storage";

/** `BlitConnection` is not exported by name; this is the same type. */
type Connection = NonNullable<ReturnType<BlitWorkspace["getConnection"]>>;

/** A connection that speaks the desktop-media family, with its display name. */
export interface MediaDeviceEntry {
  snapshot: BlitConnectionSnapshot;
  connection: Connection;
  label: string;
  readOnly: boolean;
}

export type ScreenCastEntry = MediaDeviceEntry & { session: ScreenCastState };

/** Capture heights offered in the panel. `0` is "whatever the camera does". */
export const CAMERA_RESOLUTIONS: readonly number[] = [360, 480, 720, 1080];
/** Capture cadences offered in the panel. `0` is the codec's own default. */
export const CAMERA_FRAME_RATES: readonly number[] = [15, 24, 30, 60];
/** Asked for when no height is pinned — the mode almost every camera has. */
const DEFAULT_CAMERA_HEIGHT = 720;
/** The ceiling this client advertises, and so the most the server will take.
 *  Above it the compositor's own camera bounds reject the lease anyway. */
const MAX_CAMERA_WIDTH = 1920;
const MAX_CAMERA_HEIGHT = 1080;
/** Advertised cadence ceiling. Sending less is always allowed, so this is
 *  simply the highest the panel can offer — it does not commit us to it. */
const MAX_CAMERA_FPS = 60;

/**
 * Reconcile what the camera produced with what the protocol can carry.
 *
 * Three parties have a say and only one of them is asked politely: the
 * hardware picks a mode from our `ideal` hints, this fits it to what we
 * advertised, and the server checks the result again on the way in. Extents
 * are rounded down to even because the 4:2:0 codecs cannot subsample an odd
 * one, and the server refuses those outright.
 *
 * The fit scales both axes by one factor rather than capping each on its own.
 * Per-axis clamping is only right for a camera shaped like the limit box: a
 * phone or tablet held upright reports 720x1280, where the height alone is
 * over the cap, so trimming just the height announces 720x1080 for a picture
 * that is still 9:16 — and every face in it arrives squashed by a fifth.
 */
export function negotiatedCameraFormat(
  track: MediaStreamTrack,
  maxFps: number,
  /** What a `<video>` on this track actually paints, when it could be
   *  measured. Preferred over the settings — see `measuredFrameSize`. */
  measured?: { width: number; height: number } | null,
): [number, number, number] {
  const settings = track.getSettings();
  const sourceWidth = Math.max(
    1,
    Math.round(measured?.width || settings.width || 1280),
  );
  const sourceHeight = Math.max(
    1,
    Math.round(measured?.height || settings.height || DEFAULT_CAMERA_HEIGHT),
  );
  const fit = Math.min(
    1,
    MAX_CAMERA_WIDTH / sourceWidth,
    MAX_CAMERA_HEIGHT / sourceHeight,
  );
  const even = (value: number) => {
    const pixels = Math.max(2, Math.round(value * fit));
    return Math.max(2, pixels - (pixels % 2));
  };
  return [
    even(sourceWidth),
    even(sourceHeight),
    Math.max(1, Math.min(maxFps, Math.round(settings.frameRate ?? maxFps))),
  ];
}

/**
 * The frame size the encoders will actually sample, measured rather than asked.
 *
 * `getSettings()` reports the capture the browser negotiated, but both encoders
 * sample a `<video>` element — `drawImage` for Motion JPEG, `VideoFrame` for
 * the rest — and the two do not always agree. A tablet reports one orientation
 * and paints the other, so declaring the settings' size and then scaling the
 * element into it stretches every frame by the ratio between them, in every
 * codec. The element is what gets encoded, so the element is what counts.
 *
 * Null when there is no DOM, or when nothing was painted before the deadline;
 * the caller falls back to the settings, which is what it used to do always.
 */
async function measuredFrameSize(
  track: MediaStreamTrack,
): Promise<{ width: number; height: number } | null> {
  if (typeof document === "undefined") return null;
  const video = document.createElement("video");
  video.muted = true;
  video.playsInline = true;
  video.srcObject = new MediaStream([track]);
  try {
    await video.play().catch(() => {});
    const deadline = Date.now() + 2_000;
    while (!(video.videoWidth && video.videoHeight) && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
    return video.videoWidth && video.videoHeight
      ? { width: video.videoWidth, height: video.videoHeight }
      : null;
  } finally {
    video.pause();
    video.srcObject = null;
  }
}

/**
 * Whether two enumerations describe the same devices in the same order.
 *
 * `enumerateDevices` is a fresh allocation every call, so identity comparison
 * always reports a change and the panel's `For` rows are rebuilt for nothing.
 * Only the three fields the picker renders are compared: `groupId` moves
 * around on some browsers without any device having appeared or gone.
 */
export function sameDevices(
  a: readonly MediaDeviceInfo[],
  b: readonly MediaDeviceInfo[],
): boolean {
  return (
    a.length === b.length &&
    a.every(
      (device, index) =>
        device.deviceId === b[index].deviceId &&
        device.kind === b[index].kind &&
        device.label === b[index].label,
    )
  );
}

export type MediaDevices = ReturnType<typeof createMediaDevices>;

/**
 * Viewer camera/microphone capture, shared by the media panel (which owns
 * the controls) and the status bar (which owns the privacy indicator).
 *
 * Instantiate once, at workspace scope: the capability advertisement below
 * has to keep running whether or not the panel is open, and the encoder
 * probes are worth doing exactly once per page.
 */
export function createMediaDevices(props: {
  workspace: BlitWorkspace;
  connections: readonly BlitConnectionSnapshot[];
  connectionLabels: ReadonlyMap<string, string>;
  readOnlyConnections: ReadonlySet<string>;
}) {
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const [opusAvailable, setOpusAvailable] = createSignal(false);
  const [cameraCodecs, setCameraCodecs] = createSignal(0);
  const [ready, setReady] = createSignal(false);
  const [microphoneCodec, setMicrophoneCodecSignal] =
    createSignal<MicrophoneCodecPreference>(preferredMicrophoneCodec());
  const [cameraCodec, setCameraCodecSignal] =
    createSignal<CameraCodecPreference>(preferredCameraCodec());
  const [cameraChroma, setCameraChromaSignal] =
    createSignal<CameraChromaPreference>(preferredCameraChroma());
  /** Connection a shared device goes to, remembered across reloads. Empty
   *  means "no choice made": the first eligible connection wins. */
  const [target, setTargetSignal] = createSignal(preferredMediaTarget());
  // Which physical devices to use. `""` is the browser's default, which is
  // also what a remembered id falls back to once that device is unplugged.
  const [microphoneDevice, setMicrophoneDeviceSignal] = createSignal(
    preferredMicrophoneDevice(),
  );
  const [cameraDevice, setCameraDeviceSignal] = createSignal(
    preferredCameraDevice(),
  );
  const [speakerDevice, setSpeakerDeviceSignal] = createSignal(
    preferredSpeakerDevice(),
  );
  const [cameraResolution, setCameraResolutionSignal] = createSignal(
    preferredCameraResolution(),
  );
  const [cameraFrameRate, setCameraFrameRateSignal] = createSignal(
    preferredCameraFrameRate(),
  );
  const [cameraQuality, setCameraQualitySignal] = createSignal<CameraQuality>(
    preferredCameraQuality(),
  );
  const [devices, setDevices] = createSignal<readonly MediaDeviceInfo[]>([]);
  const advertised = new Map<string, string>();

  const entries = createMemo<MediaDeviceEntry[]>(() =>
    props.connections
      .map((snapshot) => ({
        snapshot,
        connection: props.workspace.getConnection(snapshot.id),
        label: props.connectionLabels.get(snapshot.id) ?? snapshot.id,
        readOnly: props.readOnlyConnections.has(snapshot.id),
      }))
      .filter(
        (entry): entry is MediaDeviceEntry =>
          entry.snapshot.supportsDesktopMedia && Boolean(entry.connection),
      ),
  );
  /** Connections that could take a microphone, in bar order. */
  const microphoneTargets = createMemo(() =>
    entries().filter(
      (entry) =>
        !entry.readOnly &&
        Boolean(
          entry.connection.mediaStore.state.runtimeFlags & RUNTIME_MICROPHONE,
        ),
    ),
  );
  const cameraTargets = createMemo(() =>
    entries().filter(
      (entry) =>
        !entry.readOnly &&
        Boolean(
          cameraCodecs() & entry.connection.mediaStore.serverVideoCodecs,
        ) &&
        Boolean(
          entry.connection.mediaStore.state.runtimeFlags & RUNTIME_CAMERA,
        ),
    ),
  );
  /** The remembered target when it can still take the device, else the first
   *  that can — a stored id must not silently disable sharing after the
   *  connection it names goes away. */
  const pick = (candidates: readonly MediaDeviceEntry[]) =>
    candidates.find((entry) => entry.snapshot.id === target()) ?? candidates[0];
  const microphoneAvailable = createMemo(() => pick(microphoneTargets()));
  const cameraAvailable = createMemo(() => pick(cameraTargets()));
  const localMicrophone = createMemo(() =>
    entries().find(
      (entry) => entry.connection.mediaStore.microphone.status !== "inactive",
    ),
  );
  const localCamera = createMemo(() =>
    entries().find(
      (entry) => entry.connection.mediaStore.camera.status !== "inactive",
    ),
  );
  const activeMicrophones = createMemo(() =>
    entries().filter(
      (entry) =>
        entry.connection.mediaStore.microphone.status !== "inactive" ||
        Boolean(
          entry.connection.mediaStore.state.activeFlags & ACTIVE_MICROPHONE,
        ),
    ),
  );
  const activeCameras = createMemo(() =>
    entries().filter(
      (entry) =>
        entry.connection.mediaStore.camera.status !== "inactive" ||
        Boolean(entry.connection.mediaStore.state.activeFlags & ACTIVE_CAMERA),
    ),
  );
  const activeScreenCasts = createMemo<ScreenCastEntry[]>(() =>
    entries().flatMap((entry) =>
      entry.connection.mediaStore.state.screencasts.map((session) => ({
        ...entry,
        session,
      })),
    ),
  );
  /** What the camera is actually sending, once a lease is live — the answer
   *  the three-way negotiation settled on, which is rarely what was asked. */
  /**
   * The cadence to ask for.
   *
   * Motion JPEG carries a whole intra frame every time, so when it is all the
   * two ends can agree on the default stays low — but it is only a default.
   * An explicit choice is sent as asked, and the server's ceiling (raise it
   * with `BLIT_MEDIA_CAMERA_MAX_FPS`) has the last word.
   */
  const targetFrameRate = () =>
    cameraFrameRate() ||
    (availableCameraCodecs() & ~VIDEO_CODEC_MJPEG ? 30 : 15);

  const cameraFormat = createMemo(() => {
    const entry = localCamera();
    const lease = entry?.connection.mediaStore.camera;
    return lease && lease.status === "active" && lease.width
      ? `${lease.width}×${lease.height} @ ${lease.fps} fps`
      : "";
  });

  /** Camera formats this browser can encode and the servers will accept. */
  const availableCameraCodecs = createMemo(() => {
    let mask = 0;
    for (const entry of entries()) {
      mask |= cameraCodecs() & entry.connection.mediaStore.serverVideoCodecs;
    }
    return mask;
  });

  createEffect(() => {
    // The server accepts at most one capability update per second. Wait for
    // the asynchronous codec probe so the initial advertisement is final
    // instead of racing a baseline-only update against the Opus result.
    if (!ready()) return;
    for (const entry of entries()) {
      const videoCodecs =
        cameraCodecs() & entry.connection.mediaStore.serverVideoCodecs;
      // Advertise the ceiling, not the current pick: this is what the server
      // measures MEDIA_START against, and re-advertising is rate-limited to
      // once a second — a viewer raising the cadence must not have to wait
      // for that round trip before the lease it just asked for is legal.
      const maxFps = MAX_CAMERA_FPS;
      const generation = `${entry.snapshot.generation}:${Number(opusAvailable())}:${videoCodecs}`;
      if (entry.readOnly || advertised.get(entry.snapshot.id) === generation) {
        continue;
      }
      entry.connection.mediaStore.setCapabilities({
        microphone:
          typeof AudioContext !== "undefined" &&
          typeof navigator.mediaDevices?.getUserMedia === "function",
        camera:
          Boolean(videoCodecs) &&
          typeof document !== "undefined" &&
          typeof navigator.mediaDevices?.getUserMedia === "function",
        portalUi: true,
        audioCodecs: AUDIO_CODEC_PCM | (opusAvailable() ? AUDIO_CODEC_OPUS : 0),
        videoCodecs,
        maxWidth: MAX_CAMERA_WIDTH,
        maxHeight: MAX_CAMERA_HEIGHT,
        maxFps,
      });
      advertised.set(entry.snapshot.id, generation);
    }
  });

  /**
   * One operation at a time per device, and none of them dropped.
   *
   * Every setting change tears a live lease down and builds a new one, and
   * opening a device is slow enough — `getUserMedia`, a codec probe, a lease
   * round trip — that a second change lands while the first is still running.
   * Refusing the newcomer is not safe: the teardown has already happened by
   * then, so the device ends up off with its rebuild discarded and nothing
   * said about it. Queueing keeps every teardown paired with its start, and
   * makes the last change the one that wins.
   */
  const inFlight: Record<"microphone" | "camera", Promise<void>> = {
    microphone: Promise.resolve(),
    camera: Promise.resolve(),
  };
  const serialize = (
    kind: "microphone" | "camera",
    work: () => Promise<void>,
  ): Promise<void> => {
    // `then(work, work)` rather than `finally`: a failed operation must not
    // stop the queue, and the next one re-reads the state for itself.
    const next = inFlight[kind].then(work, work);
    inFlight[kind] = next.catch(() => {});
    return next;
  };

  /**
   * Open the device and hand it straight to a lease.
   *
   * There is no confirmation step: the browser already asked for the device,
   * and pressing Share in a panel that names its target is the answer to the
   * only other question there was.
   */
  const shareNow = async (kind: "microphone" | "camera") => {
    const entry =
      kind === "microphone" ? microphoneAvailable() : cameraAvailable();
    if (!entry) return;
    setBusy(true);
    setError("");
    let stream: MediaStream | null = null;
    // iOS ends a microphone track outright when the audio session is still
    // declared playback-only — the category has to be recording-capable
    // before the device is asked for, not once a capture exists to hold it.
    // The capture takes its own claim while this one is still held, so the
    // handoff has no window where the session drops back and kills the track.
    const recording = kind === "microphone";
    if (recording) retainRecordingAudioSession();
    try {
      // `ideal`, not `exact`: a remembered device that is no longer plugged
      // in should fall back to the browser's default rather than fail the
      // share outright.
      if (kind === "microphone") {
        stream = await navigator.mediaDevices.getUserMedia({
          audio: {
            ...(microphoneDevice()
              ? { deviceId: { ideal: microphoneDevice() } }
              : {}),
            channelCount: { ideal: 1 },
            echoCancellation: { ideal: true },
            noiseSuppression: { ideal: true },
            autoGainControl: { ideal: true },
          },
          video: false,
        });
        const track = stream.getAudioTracks()[0];
        if (!track) throw new Error("No microphone track was returned");
        // "auto" prefers Opus and lets the store fall back to PCM; an explicit
        // choice is honored as given, including a "pcm" that costs bandwidth.
        const wanted = microphoneCodec();
        const useOpus = wanted !== "pcm" && (await probeOpusMicrophone());
        if (useOpus) setOpusAvailable(true);
        await entry.connection.mediaStore.startMicrophone(
          track,
          useOpus
            ? wanted === "opus"
              ? { codec: "opus" }
              : {}
            : { codec: "pcm" },
        );
      } else {
        const maxFps = targetFrameRate();
        const wanted = cameraResolution() || DEFAULT_CAMERA_HEIGHT;
        stream = await navigator.mediaDevices.getUserMedia({
          audio: false,
          video: {
            ...(cameraDevice()
              ? { deviceId: { ideal: cameraDevice() } }
              : { facingMode: { ideal: "user" } }),
            // `ideal`, so a camera without this exact mode still opens at
            // whatever it does have instead of failing the share.
            width: {
              ideal: Math.round((wanted * 16) / 9),
              max: MAX_CAMERA_WIDTH,
            },
            height: { ideal: wanted, max: MAX_CAMERA_HEIGHT },
            frameRate: { ideal: maxFps, max: maxFps },
          },
        });
        const track = stream.getVideoTracks()[0];
        if (!track) throw new Error("No camera track was returned");
        // What the hardware actually gave us, not what we asked for. A camera
        // that has no 720p mode hands back 640x480, and encoding — and
        // telling the server — 1280x720 would describe a picture nobody sent.
        // Measured off a real element rather than taken from the settings,
        // because those two disagree on a tablet and the element is the one
        // the encoders read.
        const [width, height, fps] = negotiatedCameraFormat(
          track,
          maxFps,
          await measuredFrameSize(track),
        );
        const codec = cameraCodec();
        const chroma = cameraChroma();
        await entry.connection.mediaStore.startCamera(track, {
          width,
          height,
          fps,
          quality: cameraQuality(),
          ...(codec === "auto" ? {} : { codec }),
          // Motion JPEG has no chroma selection to make; asking for one is an
          // error rather than a hint, so it is dropped for that codec.
          ...(chroma === "auto" || codec === "mjpeg" ? {} : { chroma }),
        });
      }
    } catch (reason) {
      stream?.getTracks().forEach((item) => item.stop());
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      // Dropped whether or not the share took: on success the live capture is
      // already holding the session, and on failure nothing should keep a
      // headset on the bidirectional profile.
      if (recording) releaseRecordingAudioSession();
      setBusy(false);
      // Device names only become readable once permission has been granted,
      // so this is where the picker stops saying "Microphone 2".
      void refreshDevices();
    }
  };

  const unshare = (kind: "microphone" | "camera") => {
    setError("");
    for (const entry of entries()) {
      const store = entry.connection.mediaStore;
      const lease = kind === "microphone" ? store.microphone : store.camera;
      if (lease.status !== "inactive") store.stop(kind);
    }
  };

  const sharing = (kind: "microphone" | "camera") =>
    kind === "microphone" ? localMicrophone() : localCamera();

  // Queued, and deciding which way to go only once it runs: a toggle pressed
  // twice in quick succession must end up in the state the second press asked
  // for, not race the first press's `getUserMedia`.
  const toggleShare = (kind: "microphone" | "camera") => {
    void serialize(kind, async () => {
      if (sharing(kind)) unshare(kind);
      else await shareNow(kind);
    });
  };

  /**
   * Why the last share did not take.
   *
   * A refused lease does not throw — `startMicrophone`/`startCamera` resolve
   * and record the reason on the lease state — so without this a server that
   * cannot open the device (no PipeWire graph, operator gate, another viewer
   * holding it) looks exactly like a button that does nothing.
   */
  const leaseError = createMemo(() => {
    for (const entry of entries()) {
      const store = entry.connection.mediaStore;
      const reason = store.microphone.error ?? store.camera.error;
      if (reason) return reason;
    }
    return "";
  });

  /**
   * Re-share with the settings that just changed.
   *
   * Codec and target are fixed when a lease starts, so a live one has to be
   * torn down and rebuilt to honour a change. The stop reaches the server
   * before the new start does — `getUserMedia` alone is longer than the round
   * trip — so the second lease does not race the first for the device.
   */
  const restartIfSharing = (kind: "microphone" | "camera") => {
    // The "is it sharing" question is asked inside the queued work, not here:
    // a change made while the device is still opening has to act on the lease
    // that operation is about to produce, not on the empty state it sees now.
    void serialize(kind, async () => {
      if (!sharing(kind)) return;
      unshare(kind);
      await shareNow(kind);
    });
  };

  const stopScreenCast = (entry: ScreenCastEntry) => {
    if (entry.readOnly) return;
    entry.connection.mediaStore.stopScreenCast(entry.session.sessionId);
  };

  // Every setting below takes effect immediately: a live device is re-shared
  // with the new choice rather than waiting for the next time you share one.
  const setMicrophoneCodec = (value: MicrophoneCodecPreference) => {
    if (value === microphoneCodec()) return;
    setMicrophoneCodecSignal(value);
    writeStorage(MICROPHONE_CODEC_KEY, value);
    restartIfSharing("microphone");
  };
  const setCameraCodec = (value: CameraCodecPreference) => {
    if (value === cameraCodec()) return;
    setCameraCodecSignal(value);
    writeStorage(CAMERA_CODEC_KEY, value);
    restartIfSharing("camera");
  };
  const setCameraChroma = (value: CameraChromaPreference) => {
    if (value === cameraChroma()) return;
    setCameraChromaSignal(value);
    writeStorage(CAMERA_CHROMA_KEY, value);
    restartIfSharing("camera");
  };
  const setTarget = (connectionId: string) => {
    if (connectionId === target()) return;
    setTargetSignal(connectionId);
    writeStorage(MEDIA_TARGET_KEY, connectionId);
    restartIfSharing("microphone");
    restartIfSharing("camera");
  };
  const setMicrophoneDevice = (deviceId: string) => {
    if (deviceId === microphoneDevice()) return;
    setMicrophoneDeviceSignal(deviceId);
    writeStorage(MICROPHONE_DEVICE_KEY, deviceId);
    restartIfSharing("microphone");
  };
  const setCameraDevice = (deviceId: string) => {
    if (deviceId === cameraDevice()) return;
    setCameraDeviceSignal(deviceId);
    writeStorage(CAMERA_DEVICE_KEY, deviceId);
    restartIfSharing("camera");
  };
  const setCameraQuality = (quality: CameraQuality) => {
    if (quality === cameraQuality()) return;
    setCameraQualitySignal(quality);
    writeStorage(CAMERA_QUALITY_KEY, quality);
    restartIfSharing("camera");
  };
  const setCameraFrameRate = (fps: number) => {
    if (fps === cameraFrameRate()) return;
    setCameraFrameRateSignal(fps);
    writeStorage(CAMERA_FRAME_RATE_KEY, String(fps));
    restartIfSharing("camera");
  };
  const setCameraResolution = (height: number) => {
    if (height === cameraResolution()) return;
    setCameraResolutionSignal(height);
    writeStorage(CAMERA_RESOLUTION_KEY, String(height));
    restartIfSharing("camera");
  };
  /** Playback is local, so this needs no lease — it re-routes every
   *  connection's player, including ones already playing. */
  const setSpeakerDevice = (deviceId: string) => {
    setSpeakerDeviceSignal(deviceId);
    writeStorage(SPEAKER_DEVICE_KEY, deviceId);
    applySpeakerDevice();
  };
  const applySpeakerDevice = () => {
    for (const entry of entries()) {
      entry.connection.audioPlayer.setOutputDevice(speakerDevice());
    }
  };

  /**
   * Refresh the device list.
   *
   * Labels are blank until the page has been granted a device of that kind,
   * so this is re-run after every successful share — that is the moment the
   * names appear, and a list of opaque ids is not a choice anyone can make.
   */
  const refreshDevices = async () => {
    if (typeof navigator.mediaDevices?.enumerateDevices !== "function") return;
    try {
      const next = await navigator.mediaDevices.enumerateDevices();
      // Enumeration allocates a fresh object per device every call, so a
      // straight `setDevices` republishes an identical list as brand-new
      // identities — and every `For` over it discards its rows and rebuilds
      // them. Nothing downstream survives that for free, so an unchanged list
      // is dropped here rather than defended against everywhere else.
      setDevices((previous) => (sameDevices(previous, next) ? previous : next));
    } catch {
      // Enumeration is refused in some embedded contexts; the panel then
      // offers only the browser default, which still works.
    }
  };
  /** Only ids the picker can act on.
   *
   *  Safari withholds the id along with the label until a capture of that kind
   *  has been granted, and an option whose value is `""` is indistinguishable
   *  from the "System default" option above it — picking it re-selects what was
   *  already chosen. Such a device is counted for the hint instead. */
  const devicesOfKind = (kind: MediaDeviceKind) =>
    devices().filter(
      (device) => device.kind === kind && device.deviceId !== "",
    );
  const unnamedOfKind = (kind: MediaDeviceKind) =>
    devices().filter((device) => device.kind === kind && device.deviceId === "")
      .length;
  const microphoneDevices = createMemo(() => devicesOfKind("audioinput"));
  const cameraDevices = createMemo(() => devicesOfKind("videoinput"));
  const speakerDevices = createMemo(() => devicesOfKind("audiooutput"));
  const unnamedMicrophones = createMemo(() => unnamedOfKind("audioinput"));
  const unnamedCameras = createMemo(() => unnamedOfKind("videoinput"));
  /** Firefox has no `AudioContext.setSinkId`, so the picker is hidden there
   *  rather than offered and silently ignored. */
  const speakerSelectionSupported = AudioPlayer.outputSelectionSupported;

  onMount(() => {
    void Promise.all([probeOpusMicrophone(), probeCameraCodecs()])
      .then(
        ([opus, videoCodecs]) => {
          setOpusAvailable(opus);
          setCameraCodecs(videoCodecs);
        },
        () => {},
      )
      .finally(() => setReady(true));
    void refreshDevices();
    const onDeviceChange = () => void refreshDevices();
    navigator.mediaDevices?.addEventListener?.("devicechange", onDeviceChange);
    onCleanup(() =>
      navigator.mediaDevices?.removeEventListener?.(
        "devicechange",
        onDeviceChange,
      ),
    );
  });

  // New connections start their player on the default sink, so the choice is
  // re-applied whenever the set of connections changes.
  createEffect(applySpeakerDevice);

  return {
    entries,
    busy,
    error,
    leaseError,
    opusAvailable,
    /** Camera formats this browser's encoder probe confirmed. */
    cameraCodecs,
    availableCameraCodecs,
    microphoneTargets,
    cameraTargets,
    microphoneAvailable,
    cameraAvailable,
    localMicrophone,
    localCamera,
    activeMicrophones,
    activeCameras,
    activeScreenCasts,
    microphoneCodec,
    cameraCodec,
    cameraChroma,
    target,
    microphoneDevices,
    cameraDevices,
    speakerDevices,
    unnamedMicrophones,
    unnamedCameras,
    microphoneDevice,
    cameraDevice,
    speakerDevice,
    cameraResolution,
    cameraFormat,
    speakerSelectionSupported,
    setMicrophoneCodec,
    setCameraCodec,
    setCameraChroma,
    setTarget,
    setMicrophoneDevice,
    setCameraDevice,
    setSpeakerDevice,
    setCameraResolution,
    setCameraFrameRate,
    setCameraQuality,
    cameraFrameRate,
    cameraQuality,
    sharing,
    toggleShare,
    stopScreenCast,
  };
}
