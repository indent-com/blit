import { afterEach, describe, expect, it, vi } from "vitest";
import { fsCompressLiteral } from "../fs";
import {
  ACTIVE_CAMERA,
  ACTIVE_SCREENCAST,
  AUDIO_CODEC_OPUS,
  AUDIO_CODEC_PCM,
  CAPTURE_CAMERA,
  CAPTURE_MICROPHONE,
  CAPTURE_PORTAL_UI,
  C2S_MEDIA_CONTROL,
  C2S_MEDIA_DATA,
  MPRIS_UPDATE_RESET,
  MPRIS_UPDATE_SYNC,
  MediaStore,
  MprisStore,
  S2C_MEDIA_CONTROL,
  VIDEO_CODEC_AV1,
  VIDEO_CODEC_AV1_444,
  VIDEO_CODEC_H264,
  VIDEO_CODEC_H264_444,
  VIDEO_CODEC_MJPEG,
  VIDEO_CODECS_ALL,
  buildMediaCapabilitiesMessage,
  buildMediaDataMessage,
  buildMediaStartMessage,
  buildMediaStopMessage,
  buildMprisActionMessage,
  buildPortalReplyMessage,
  buildScreenCastStopMessage,
  parseMediaControl,
  probeCameraCodecs,
  type PortalAccessRequest,
  type PortalScreenCastRequest,
} from "../media";

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

const u16 = (out: number[], value: number) =>
  out.push(value & 0xff, (value >>> 8) & 0xff);
const u32 = (out: number[], value: number) =>
  out.push(
    value & 0xff,
    (value >>> 8) & 0xff,
    (value >>> 16) & 0xff,
    (value >>> 24) & 0xff,
  );
const i32 = (out: number[], value: number) => u32(out, value >>> 0);
const i64 = (out: number[], value: bigint) => {
  for (let shift = 0n; shift < 64n; shift += 8n) {
    out.push(Number((BigInt.asUintN(64, value) >> shift) & 0xffn));
  }
};
const str16 = (out: number[], value: string) => {
  const bytes = new TextEncoder().encode(value);
  u16(out, bytes.length);
  out.push(...bytes);
};
const str32 = (out: number[], value: string) => {
  const bytes = new TextEncoder().encode(value);
  u32(out, bytes.length);
  out.push(...bytes);
};
const bytes32 = (out: number[], value: readonly number[]) => {
  u32(out, value.length);
  out.push(...value);
};

function mprisUpdate(
  flags: number,
  playerIds: readonly number[],
  options: {
    capabilityFlags?: number;
    artworkWidth?: number;
    artworkHeight?: number;
    artworkPng?: readonly number[];
  } = {},
): Uint8Array {
  const raw: number[] = [playerIds.length];
  for (const playerId of playerIds) {
    raw.push(1);
    u32(raw, playerId);
    u32(raw, 7);
    u32(raw, 3);
    raw.push(playerId === 2 ? 1 : 0, 2, 0, 0);
    u16(raw, options.capabilityFlags ?? 1);
    i32(raw, 1_000_000);
    i32(raw, 500_000);
    i32(raw, 2_000_000);
    u32(raw, 750_000);
    i64(raw, 2_000_000n);
    i64(raw, 10_000_000n);
    str16(raw, `Player ${playerId}`);
    str16(raw, "player.desktop");
    str16(raw, "Track");
    str16(raw, "Album");
    raw.push(1);
    str16(raw, "Artist");
    u16(raw, options.artworkWidth ?? 0);
    u16(raw, options.artworkHeight ?? 0);
    bytes32(raw, options.artworkPng ?? []);
  }
  const compressed = fsCompressLiteral(new Uint8Array(raw));
  const message = new Uint8Array(3 + compressed.length);
  message.set([S2C_MEDIA_CONTROL, 6, flags]);
  message.set(compressed, 3);
  return message;
}

function mediaControl(subtype: number, body: readonly number[]): Uint8Array {
  return new Uint8Array([S2C_MEDIA_CONTROL, subtype, ...body]);
}

function portalAccessRequest(requestId = 1): PortalAccessRequest {
  return {
    kind: "access",
    requestId,
    deadlineMs: 60_000,
    parentSurfaceId: null,
    appId: "app",
    title: "Title",
    subtitle: "",
    body: "Body",
    denyLabel: "No",
    grantLabel: "Yes",
    iconName: "camera",
    choices: [],
  };
}

function portalScreenCastRequest(requestId = 1): PortalScreenCastRequest {
  return {
    kind: "screencast",
    requestId,
    deadlineMs: 120_000,
    parentSurfaceId: null,
    appId: "app",
    multiple: true,
    candidates: [],
  };
}

describe("desktop media wire format", () => {
  it("builds capability and 28-byte data headers", () => {
    expect(
      Array.from(
        buildMediaCapabilitiesMessage({
          microphone: true,
          camera: true,
          portalUi: true,
          audioCodecs: AUDIO_CODEC_PCM | AUDIO_CODEC_OPUS,
          videoCodecs: 1,
          maxWidth: 1920,
          maxHeight: 1080,
          maxFps: 15,
        }),
      ),
    ).toEqual([
      C2S_MEDIA_CONTROL,
      0,
      CAPTURE_MICROPHONE | CAPTURE_CAMERA | CAPTURE_PORTAL_UI,
      3,
      1,
      0x80,
      0x07,
      0x38,
      0x04,
      15,
    ]);

    const message = buildMediaDataMessage({
      leaseId: 4,
      sequence: 5,
      captureUs: 6,
      kind: "microphone",
      codec: 1,
      flags: 2,
      fragmentIndex: 0,
      fragmentCount: 1,
      frameLength: 3,
      data: new Uint8Array([7, 8, 9]),
    });
    expect(message.length).toBe(31);
    expect(message[0]).toBe(C2S_MEDIA_DATA);
    expect(Array.from(message.subarray(28))).toEqual([7, 8, 9]);
    expect(new DataView(message.buffer).getUint32(24, true)).toBe(3);
  });

  it("parses the extended server camera codec registry", () => {
    expect(
      parseMediaControl(
        new Uint8Array([S2C_MEDIA_CONTROL, 8, VIDEO_CODECS_ALL]),
      ),
    ).toEqual({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });
    expect(
      parseMediaControl(new Uint8Array([S2C_MEDIA_CONTROL, 8, 0x80])),
    ).toBeNull();
    expect(
      parseMediaControl(
        new Uint8Array([S2C_MEDIA_CONTROL, 8, VIDEO_CODEC_H264]),
      ),
    ).toBeNull();
    expect(
      parseMediaControl(new Uint8Array([S2C_MEDIA_CONTROL, 8, 0])),
    ).toBeNull();

    const extended = buildMediaCapabilitiesMessage({
      microphone: false,
      camera: true,
      portalUi: false,
      audioCodecs: 0,
      videoCodecs: VIDEO_CODECS_ALL,
      maxWidth: 1920,
      maxHeight: 1080,
      maxFps: 30,
    });
    expect(extended[4]).toBe(VIDEO_CODECS_ALL);
  });

  it("safely re-advertises extended codecs after a late server announcement", async () => {
    vi.useFakeTimers();
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setCapabilities({
      microphone: false,
      camera: true,
      portalUi: false,
      audioCodecs: 0,
      videoCodecs: VIDEO_CODECS_ALL,
      maxWidth: 1920,
      maxHeight: 1080,
      maxFps: 30,
    });
    expect(messages).toHaveLength(0);
    store.setSender((message) => messages.push(message));
    expect(messages).toHaveLength(1);
    expect(messages[0]![4]).toBe(VIDEO_CODEC_MJPEG | VIDEO_CODEC_H264);

    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });
    expect(store.serverVideoCodecs).toBe(VIDEO_CODECS_ALL);
    expect(messages).toHaveLength(1);
    await vi.advanceTimersByTimeAsync(1_050);
    expect(messages).toHaveLength(2);
    expect(messages[1]![4]).toBe(VIDEO_CODECS_ALL);
  });

  it("parses aggregate state and rejects inconsistent active flags", () => {
    const message = new Uint8Array(21);
    message.set([S2C_MEDIA_CONTROL, 0, 7, ACTIVE_CAMERA]);
    const view = new DataView(message.buffer);
    view.setBigUint64(4, 0n, true);
    view.setBigUint64(12, 42n, true);
    message[20] = 0;
    expect(parseMediaControl(message)).toEqual({
      kind: "state",
      state: {
        runtimeFlags: 7,
        activeFlags: ACTIVE_CAMERA,
        microphoneOwner: 0n,
        cameraOwner: 42n,
        screencasts: [],
      },
    });
    message[3] = 0;
    expect(parseMediaControl(message)).toBeNull();
  });

  it("round-trips normalized Access prompt choices", () => {
    const body: number[] = [];
    u32(body, 9);
    body.push(0);
    u32(body, 60_000);
    u16(body, 0);
    str16(body, "app");
    str16(body, "Title");
    str16(body, "");
    str32(body, "Body");
    str16(body, "No");
    str16(body, "Yes");
    str16(body, "camera");
    body.push(1);
    str16(body, "mode");
    str16(body, "Mode");
    body.push(2);
    str16(body, "a");
    str16(body, "A");
    str16(body, "b");
    str16(body, "B");
    str16(body, "a");
    const requestMessage = new Uint8Array(2 + body.length);
    requestMessage.set([S2C_MEDIA_CONTROL, 4]);
    requestMessage.set(body, 2);
    const parsed = parseMediaControl(requestMessage);
    expect(parsed?.kind).toBe("portalRequest");
    if (parsed?.kind !== "portalRequest")
      throw new Error("prompt did not parse");
    const reply = buildPortalReplyMessage(
      parsed.request,
      "grant",
      [],
      [{ id: "mode", value: "b" }],
    );
    expect(reply.slice(0, 9)).toEqual(
      new Uint8Array([C2S_MEDIA_CONTROL, 3, 9, 0, 0, 0, 1, 0, 1]),
    );
  });

  it("builds an idempotent ScreenCast stop through MediaStore", () => {
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));

    store.stopScreenCast(0x12345678);

    expect(messages).toEqual([
      new Uint8Array([C2S_MEDIA_CONTROL, 4, 0x78, 0x56, 0x34, 0x12]),
    ]);
  });

  it("rejects the Rust parser's malformed S2C control corpus", () => {
    const unknownRuntime = new Uint8Array(21);
    unknownRuntime.set([S2C_MEDIA_CONTROL, 0, 0x80, 0]);
    const unknownActive = new Uint8Array(21);
    unknownActive.set([S2C_MEDIA_CONTROL, 0, 0, 0x08]);

    const zeroNonceLease: number[] = [];
    u32(zeroNonceLease, 0);
    zeroNonceLease.push(0, 0);
    u32(zeroNonceLease, 1);
    zeroNonceLease.push(0);
    u16(zeroNonceLease, 0);
    u16(zeroNonceLease, 0);
    zeroNonceLease.push(0);
    u32(zeroNonceLease, 1);

    const zeroRequest: number[] = [];
    u32(zeroRequest, 0);
    zeroRequest.push(0);
    u32(zeroRequest, 1);
    u16(zeroRequest, 0);
    str16(zeroRequest, "");
    str16(zeroRequest, "");
    str16(zeroRequest, "");
    str32(zeroRequest, "");
    str16(zeroRequest, "");
    str16(zeroRequest, "");
    str16(zeroRequest, "");
    zeroRequest.push(0);

    const zeroCandidate: number[] = [];
    u32(zeroCandidate, 1);
    zeroCandidate.push(1);
    u32(zeroCandidate, 1);
    u16(zeroCandidate, 0);
    str16(zeroCandidate, "");
    zeroCandidate.push(0, 1);
    u16(zeroCandidate, 0);
    u16(zeroCandidate, 1);
    u16(zeroCandidate, 1);
    str16(zeroCandidate, "");
    str16(zeroCandidate, "");
    bytes32(zeroCandidate, []);

    const zeroResultNonce: number[] = [];
    u32(zeroResultNonce, 0);
    zeroResultNonce.push(0);
    u32(zeroResultNonce, 1);
    u32(zeroResultNonce, 1);

    const zeroResultPlayer: number[] = [];
    u32(zeroResultPlayer, 1);
    zeroResultPlayer.push(0);
    u32(zeroResultPlayer, 0);
    u32(zeroResultPlayer, 1);

    const oversizedPortal = new Uint8Array(4 * 1024 * 1024 + 1);
    oversizedPortal.set([S2C_MEDIA_CONTROL, 4]);

    const zeroScreenCastSession: number[] = [0, 4];
    i64(zeroScreenCastSession, 0n);
    i64(zeroScreenCastSession, 0n);
    zeroScreenCastSession.push(1);
    u32(zeroScreenCastSession, 0);
    str16(zeroScreenCastSession, "");
    zeroScreenCastSession.push(1);
    u16(zeroScreenCastSession, 1);

    const duplicateScreenCastSessions: number[] = [0, ACTIVE_SCREENCAST];
    i64(duplicateScreenCastSessions, 0n);
    i64(duplicateScreenCastSessions, 0n);
    duplicateScreenCastSessions.push(2);
    for (const surfaceId of [1, 2]) {
      u32(duplicateScreenCastSessions, 1);
      str16(duplicateScreenCastSessions, "");
      duplicateScreenCastSessions.push(1);
      u16(duplicateScreenCastSessions, surfaceId);
    }

    const duplicateCandidates: number[] = [];
    u32(duplicateCandidates, 1);
    duplicateCandidates.push(1);
    u32(duplicateCandidates, 1);
    u16(duplicateCandidates, 0);
    str16(duplicateCandidates, "");
    duplicateCandidates.push(1, 2);
    for (let index = 0; index < 2; index++) {
      u16(duplicateCandidates, 1);
      u16(duplicateCandidates, 1);
      u16(duplicateCandidates, 1);
      str16(duplicateCandidates, "");
      str16(duplicateCandidates, "");
      bytes32(duplicateCandidates, []);
    }

    const corpus = [
      ["unknown runtime bit", unknownRuntime],
      ["unknown active bit", unknownActive],
      ["zero ScreenCast session", mediaControl(0, zeroScreenCastSession)],
      [
        "duplicate ScreenCast sessions",
        mediaControl(0, duplicateScreenCastSessions),
      ],
      ["zero lease nonce", mediaControl(1, zeroNonceLease)],
      ["zero revoked lease", mediaControl(2, [0, 0, 0, 0, 0])],
      ["unknown revoke reason", mediaControl(2, [1, 0, 0, 0, 8])],
      ["zero credit lease", mediaControl(3, [0, 0, 0, 0, 1, 0, 0, 0, 0])],
      ["unknown credit flag", mediaControl(3, [1, 0, 0, 0, 1, 0, 0, 0, 2])],
      ["zero portal request", mediaControl(4, zeroRequest)],
      ["zero ScreenCast candidate", mediaControl(4, zeroCandidate)],
      ["duplicate ScreenCast candidates", mediaControl(4, duplicateCandidates)],
      ["oversized portal", oversizedPortal],
      ["zero portal cancellation", mediaControl(5, [0, 0, 0, 0, 0])],
      ["zero MPRIS result nonce", mediaControl(7, zeroResultNonce)],
      ["zero MPRIS result player", mediaControl(7, zeroResultPlayer)],
    ] as const;

    for (const [name, message] of corpus) {
      expect(parseMediaControl(message), name).toBeNull();
    }
  });

  it("rejects reserved MPRIS capabilities and inconsistent artwork", () => {
    const corpus = [
      mprisUpdate(0x80, [1]),
      mprisUpdate(0, [0]),
      mprisUpdate(0, [1], { capabilityFlags: 1 << 11 }),
      mprisUpdate(0, [1], { artworkWidth: 1 }),
      mprisUpdate(0, [1], { artworkHeight: 1 }),
      mprisUpdate(0, [1], { artworkWidth: 1, artworkHeight: 1 }),
      mprisUpdate(0, [1], { artworkPng: [1] }),
    ];
    for (const message of corpus) expect(parseMediaControl(message)).toBeNull();

    const valid = parseMediaControl(
      mprisUpdate(0, [1], {
        artworkWidth: 1,
        artworkHeight: 1,
        artworkPng: [0x89, 0x50, 0x4e, 0x47],
      }),
    );
    expect(valid?.kind).toBe("mprisUpdate");
    if (valid?.kind !== "mprisUpdate") throw new Error("artwork did not parse");
    const record = valid.records[0];
    expect(record?.kind).toBe("upsert");
    if (record?.kind !== "upsert") throw new Error("player did not parse");
    expect(record.player.artwork).toMatchObject({ width: 1, height: 1 });
  });

  it("emits bounded UTF-8 portal choices without splitting code points", () => {
    const splitAtLimit = `${"a".repeat(4095)}😀`;
    const request: PortalAccessRequest = {
      ...portalAccessRequest(),
      choices: [
        {
          id: splitAtLimit,
          label: "",
          options: [{ id: "é", value: "accent" }],
          initialValue: "é",
        },
      ],
    };
    const reply = buildPortalReplyMessage(
      request,
      "grant",
      [],
      [{ id: splitAtLimit, value: "é" }],
    );
    const view = new DataView(reply.buffer, reply.byteOffset, reply.byteLength);
    const idLength = view.getUint16(9, true);
    expect(idLength).toBe(4095);
    expect(
      new TextDecoder("utf-8", { fatal: true }).decode(
        reply.subarray(11, 11 + idLength),
      ),
    ).toBe("a".repeat(4095));
    const valueOffset = 11 + idLength;
    const valueLength = view.getUint16(valueOffset, true);
    expect(valueLength).toBe(2);
    expect(
      new TextDecoder("utf-8", { fatal: true }).decode(
        reply.subarray(valueOffset + 2, valueOffset + 2 + valueLength),
      ),
    ).toBe("é");
  });

  it("keeps public builders inside server acceptance invariants", () => {
    expect(
      buildMediaStartMessage(0x12345678, "camera", 1, 640, 480, 15),
    ).toEqual(
      new Uint8Array([
        C2S_MEDIA_CONTROL,
        1,
        0x78,
        0x56,
        0x34,
        0x12,
        1,
        1,
        0x80,
        0x02,
        0xe0,
        0x01,
        15,
      ]),
    );
    const validData = {
      leaseId: 1,
      sequence: 0,
      captureUs: 0,
      kind: "microphone" as const,
      codec: 0,
      flags: 0,
      fragmentIndex: 0,
      fragmentCount: 1,
      frameLength: 1,
      data: new Uint8Array([0]),
    };
    const invalidBuilders = [
      () => buildMediaStartMessage(0, "microphone", 0),
      () => buildMediaStartMessage(1, "microphone", 0, 1, 0, 0),
      () => buildMediaStartMessage(1, "camera", 0),
      () => buildMediaStopMessage(0),
      () => buildMediaDataMessage({ ...validData, leaseId: 0 }),
      () => buildMediaDataMessage({ ...validData, flags: 0x08 }),
      () => buildMediaDataMessage({ ...validData, fragmentCount: 0 }),
      () => buildMediaDataMessage({ ...validData, frameLength: 2 }),
      () => buildMprisActionMessage(0, 1, { kind: "play" }),
      () => buildMprisActionMessage(1, 0, { kind: "play" }),
      () =>
        buildMprisActionMessage(1, 1, {
          kind: "setPosition",
          positionUs: 0,
          trackRevision: 0,
        }),
      () => buildPortalReplyMessage(portalAccessRequest(0), "deny"),
      () => buildPortalReplyMessage(portalScreenCastRequest(), "grant", [1, 1]),
      () => buildPortalReplyMessage(portalScreenCastRequest(), "grant", [0]),
      () => buildPortalReplyMessage(portalScreenCastRequest(), "grant", []),
      () =>
        buildPortalReplyMessage(
          {
            ...portalAccessRequest(),
            choices: [
              {
                id: "mode",
                label: "Mode",
                options: [{ id: "safe", value: "Safe" }],
                initialValue: "safe",
              },
            ],
          },
          "grant",
          [],
          [{ id: "mode", value: "unknown" }],
        ),
      () => buildScreenCastStopMessage(0),
    ];
    for (const build of invalidBuilders) expect(build).toThrow(RangeError);

    expect(
      buildPortalReplyMessage(
        portalScreenCastRequest(),
        "deny",
        [1, 2],
        [{ id: "ignored", value: "ignored" }],
      ),
    ).toEqual(new Uint8Array([C2S_MEDIA_CONTROL, 3, 1, 0, 0, 0, 0, 0, 0]));
  });
});

describe("MprisStore", () => {
  it("publishes staged snapshots atomically and extrapolates position", () => {
    const store = new MprisStore();
    const changed = vi.fn();
    store.subscribe(changed);
    const first = parseMediaControl(mprisUpdate(MPRIS_UPDATE_RESET, [1]));
    const second = parseMediaControl(mprisUpdate(MPRIS_UPDATE_SYNC, [2]));
    if (!first || !second) throw new Error("MPRIS update did not parse");
    store.handle(first);
    expect(store.players.size).toBe(0);
    expect(changed).not.toHaveBeenCalled();
    store.handle(second);
    expect([...store.players.keys()]).toEqual([1, 2]);
    expect(store.activePlayerId).toBe(2);
    expect(changed).toHaveBeenCalledOnce();
    const player = store.players.get(2)!;
    expect(store.positionUs(2, player.receivedAtMs + 500)).toBe(2_500_000);
  });

  it("correlates action results by nonce and player", async () => {
    const messages: Uint8Array[] = [];
    const store = new MprisStore();
    store.setSender((message) => messages.push(message));
    const completed = vi.fn();
    const action = store.act(7, { kind: "play" }).then(completed);
    const nonce = new DataView(
      messages[0]!.buffer,
      messages[0]!.byteOffset,
      messages[0]!.byteLength,
    ).getUint32(2, true);

    store.handle({
      kind: "mprisResult",
      nonce,
      status: 0,
      playerId: 8,
      revision: 1,
    });
    await Promise.resolve();
    expect(completed).not.toHaveBeenCalled();

    store.handle({
      kind: "mprisResult",
      nonce,
      status: 0,
      playerId: 7,
      revision: 1,
    });
    await expect(action).resolves.toBeUndefined();
    expect(completed).toHaveBeenCalledOnce();
  });
});

type FakeMicrophoneTrack = EventTarget & {
  kind: "audio";
  readyState: MediaStreamTrackState;
  stop: ReturnType<typeof vi.fn>;
};

function installMicrophoneMocks(opusSupported = true): {
  track: FakeMicrophoneTrack;
  failEncoder: (error: DOMException) => void;
  emitPcm: (samples: Float32Array) => void;
} {
  class FakeAudioNode {
    connect<T>(destination: T): T {
      return destination;
    }
    disconnect(): void {}
  }
  class FakeGainNode extends FakeAudioNode {
    readonly gain = { value: 1 };
  }
  let workletMessage: ((event: MessageEvent<ArrayBuffer>) => void) | null =
    null;
  class FakeAudioWorkletNode extends FakeAudioNode {
    readonly port = {
      get onmessage() {
        return workletMessage;
      },
      set onmessage(
        value: ((event: MessageEvent<ArrayBuffer>) => void) | null,
      ) {
        workletMessage = value;
      },
    };
  }
  class FakeAudioContext {
    readonly sampleRate = 48_000;
    readonly destination = {};
    readonly audioWorklet = { addModule: async () => undefined };
    createMediaStreamSource(): FakeAudioNode {
      return new FakeAudioNode();
    }
    createGain(): FakeGainNode {
      return new FakeGainNode();
    }
    async resume(): Promise<void> {}
    async close(): Promise<void> {}
  }
  class FakeMediaStream {
    constructor(_tracks: readonly unknown[]) {}
  }
  class FakeAudioData {
    close(): void {}
  }

  let encoderError: ((error: DOMException) => void) | null = null;
  class FakeAudioEncoder {
    static async isConfigSupported(): Promise<{ supported: boolean }> {
      return { supported: opusSupported };
    }
    readonly encodeQueueSize = 0;
    constructor(init: { error: (error: DOMException) => void }) {
      encoderError = init.error;
    }
    configure(): void {}
    encode(): void {}
    close(): void {}
  }

  vi.stubGlobal("AudioContext", FakeAudioContext);
  vi.stubGlobal("AudioWorkletNode", FakeAudioWorkletNode);
  vi.stubGlobal("MediaStream", FakeMediaStream);
  vi.stubGlobal("AudioEncoder", FakeAudioEncoder);
  vi.stubGlobal("AudioData", FakeAudioData);
  vi.stubGlobal("URL", {
    createObjectURL: () => "blob:blit-microphone-test",
    revokeObjectURL: () => undefined,
  });

  const track = new EventTarget() as FakeMicrophoneTrack;
  track.kind = "audio";
  track.readyState = "live";
  track.stop = vi.fn(() => {
    track.readyState = "ended";
  });
  return {
    track,
    failEncoder: (error) => {
      if (!encoderError) throw new Error("Opus encoder was not created");
      encoderError(error);
    },
    emitPcm: (samples) => {
      if (!workletMessage)
        throw new Error("microphone worklet was not created");
      const data = samples.slice().buffer as ArrayBuffer;
      workletMessage({ data } as MessageEvent<ArrayBuffer>);
    },
  };
}

async function waitForMediaStart(
  messages: readonly Uint8Array[],
): Promise<Uint8Array> {
  await vi.waitFor(() => {
    expect(
      messages.some(
        (message) => message[0] === C2S_MEDIA_CONTROL && message[1] === 1,
      ),
    ).toBe(true);
  });
  return messages.find(
    (message) => message[0] === C2S_MEDIA_CONTROL && message[1] === 1,
  )!;
}

function startNonce(message: Uint8Array): number {
  return new DataView(
    message.buffer,
    message.byteOffset,
    message.byteLength,
  ).getUint32(2, true);
}

type FakeCameraTrack = EventTarget & {
  kind: "video";
  readyState: MediaStreamTrackState;
  stop: ReturnType<typeof vi.fn>;
  getSettings: () => MediaTrackSettings;
};

function installCameraMocks(): {
  track: FakeCameraTrack;
  drawImage: ReturnType<typeof vi.fn>;
  getContext: ReturnType<typeof vi.spyOn>;
  toBlob: ReturnType<typeof vi.spyOn>;
} {
  class FakeMediaStream {
    constructor(_tracks: readonly unknown[]) {}
  }
  vi.stubGlobal("MediaStream", FakeMediaStream);

  const drawImage = vi.fn();
  const context = { drawImage } as unknown as CanvasRenderingContext2D;
  const getContext = vi
    .spyOn(HTMLCanvasElement.prototype, "getContext")
    .mockImplementation(() => context);
  const jpeg = {
    size: 1,
    arrayBuffer: async () => new Uint8Array([0xff]).buffer,
  } as Blob;
  const toBlob = vi
    .spyOn(HTMLCanvasElement.prototype, "toBlob")
    .mockImplementation((callback) => callback(jpeg));
  vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue();
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => {});
  vi.spyOn(HTMLMediaElement.prototype, "readyState", "get").mockReturnValue(2);

  const track = new EventTarget() as FakeCameraTrack;
  track.kind = "video";
  track.readyState = "live";
  track.stop = vi.fn(() => {
    track.readyState = "ended";
  });
  track.getSettings = () => ({ width: 320, height: 240, frameRate: 15 });
  return { track, drawImage, getContext, toBlob };
}

function cameraChunkBytes(
  codec: string,
  includeHeader: boolean,
  wrongProfile = false,
  wrongChroma = false,
): Uint8Array {
  if (codec.startsWith("avc1")) {
    const profile = wrongProfile
      ? 0x64
      : codec.startsWith("avc1.F4")
        ? 0xf4
        : 0x42;
    const idr = [0, 0, 0, 1, 0x65, 0x80];
    return new Uint8Array(
      includeHeader
        ? [
            0,
            0,
            0,
            1,
            0x67,
            profile,
            0,
            0x1f,
            profile === 0x42 ? 0x80 : wrongChroma ? 0xa0 : 0x90,
            0,
            0,
            0,
            1,
            0x68,
            0x80,
            ...idr,
          ]
        : idr,
    );
  }
  const profile = wrongProfile ? 2 : codec.startsWith("av01.1") ? 1 : 0;
  const frame = [0x32, 1, 0];
  return new Uint8Array(
    includeHeader ? [0x0a, 1, profile << 5, ...frame] : frame,
  );
}

function installVideoEncoderMocks(
  options: {
    supported?: (codec: string) => boolean;
    wrongProfile?: (codec: string) => boolean;
    wrongChroma?: (codec: string) => boolean;
    omitHeadersAfterFirstKey?: boolean;
  } = {},
): {
  encoders: Array<{
    config: VideoEncoderConfig | null;
    encodeQueueSize: number;
    encodeOptions: VideoEncoderEncodeOptions[];
    closed: boolean;
  }>;
} {
  const encoders: Array<{
    config: VideoEncoderConfig | null;
    encodeQueueSize: number;
    encodeOptions: VideoEncoderEncodeOptions[];
    closed: boolean;
  }> = [];

  class FakeVideoFrame {
    readonly timestamp: number;
    constructor(_source: unknown, init: VideoFrameInit = {}) {
      this.timestamp = init.timestamp ?? 0;
    }
    close() {}
  }

  class FakeVideoEncoder {
    static async isConfigSupported(config: VideoEncoderConfig) {
      return { supported: options.supported?.(config.codec) ?? true, config };
    }
    config: VideoEncoderConfig | null = null;
    encodeQueueSize = 0;
    encodeOptions: VideoEncoderEncodeOptions[] = [];
    closed = false;
    #output: (chunk: EncodedVideoChunk) => void;
    #keyframes = 0;

    constructor(init: VideoEncoderInit) {
      this.#output = init.output;
      encoders.push(this);
    }
    configure(config: VideoEncoderConfig) {
      this.config = config;
    }
    encode(
      frame: FakeVideoFrame,
      encodeOptions: VideoEncoderEncodeOptions = {},
    ) {
      this.encodeOptions.push(encodeOptions);
      const keyframe = Boolean(encodeOptions.keyFrame);
      if (keyframe) this.#keyframes++;
      const codec = this.config?.codec ?? "";
      const includeHeader =
        keyframe &&
        (!options.omitHeadersAfterFirstKey || this.#keyframes === 1);
      const bytes = cameraChunkBytes(
        codec,
        includeHeader,
        options.wrongProfile?.(codec) ?? false,
        options.wrongChroma?.(codec) ?? false,
      );
      this.#output({
        byteLength: bytes.length,
        duration: null,
        timestamp: frame.timestamp,
        type: keyframe ? "key" : "delta",
        copyTo: (destination: AllowSharedBufferSource) => {
          new Uint8Array(
            destination instanceof ArrayBuffer
              ? destination
              : destination.buffer,
            destination instanceof ArrayBuffer ? 0 : destination.byteOffset,
            bytes.length,
          ).set(bytes);
        },
      } as EncodedVideoChunk);
    }
    flush() {
      return Promise.resolve();
    }
    close() {
      this.closed = true;
    }
  }

  vi.stubGlobal("VideoFrame", FakeVideoFrame);
  vi.stubGlobal("VideoEncoder", FakeVideoEncoder);
  return { encoders };
}

function cameraDataMessages(messages: readonly Uint8Array[]): Uint8Array[] {
  return messages.filter(
    (message) => message[0] === C2S_MEDIA_DATA && message[17] === 1,
  );
}

describe("MediaStore camera capture", () => {
  it("probes exact H.264/AV1 4:2:0 and 4:4:4 encoder profiles", async () => {
    installCameraMocks();
    installVideoEncoderMocks();
    await expect(probeCameraCodecs()).resolves.toBe(VIDEO_CODECS_ALL);
  });

  it("does not advertise profiles an encoder claims but does not emit", async () => {
    installCameraMocks();
    installVideoEncoderMocks({
      wrongProfile: (codec) => codec.startsWith("av01.1"),
      wrongChroma: (codec) => codec.startsWith("avc1.F4"),
    });
    await expect(probeCameraCodecs()).resolves.toBe(
      VIDEO_CODEC_MJPEG | VIDEO_CODEC_H264 | VIDEO_CODEC_AV1,
    );
  });

  it("uses the best exact supported format and falls back automatically", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    installVideoEncoderMocks({
      supported: (codec) => codec.startsWith("avc1.4200"),
    });
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });

    const started = store.startCamera(track as unknown as MediaStreamTrack);
    const start = await waitForMediaStart(messages);
    expect(start[7]).toBe(1);
    store.stop("camera");
    await expect(started).rejects.toThrow("cancelled");
    expect(track.stop).toHaveBeenCalledOnce();
  });

  it("keeps explicit profiles strict", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    installVideoEncoderMocks({
      wrongProfile: (codec) => codec.startsWith("av01.1"),
    });
    const store = new MediaStore();
    store.setSender(() => {});
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });

    await expect(
      store.startCamera(track as unknown as MediaStreamTrack, {
        codec: "av1",
        chroma: "444",
        width: 320,
        height: 240,
        fps: 15,
      }),
    ).rejects.toThrow("AV1 4:4:4 encoding is unavailable");
    expect(track.stop).toHaveBeenCalledOnce();
  });

  it("falls back between codecs while preserving an explicit chroma format", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    installVideoEncoderMocks({
      supported: (codec) => codec.startsWith("avc1.F4"),
    });
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });

    const started = store.startCamera(track as unknown as MediaStreamTrack, {
      chroma: "444",
      width: 320,
      height: 240,
      fps: 15,
    });
    const start = await waitForMediaStart(messages);
    expect(start[7]).toBe(3);
    store.stop("camera");
    await expect(started).rejects.toThrow("cancelled");
    expect(track.stop).toHaveBeenCalledOnce();
  });

  it("rejects unknown explicit camera formats before acquiring a lease", async () => {
    const { track } = installCameraMocks();
    const store = new MediaStore();
    store.setSender(() => {});
    await expect(
      store.startCamera(
        track as unknown as MediaStreamTrack,
        {
          codec: "vp9",
        } as unknown as Parameters<MediaStore["startCamera"]>[1],
      ),
    ).rejects.toThrow("unknown camera codec");
    expect(track.stop).toHaveBeenCalledOnce();
  });

  it("orders an extended start after the rate-limited capability refresh", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    installVideoEncoderMocks();
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    store.setCapabilities({
      microphone: false,
      camera: true,
      portalUi: false,
      audioCodecs: 0,
      videoCodecs: VIDEO_CODECS_ALL,
      maxWidth: 1920,
      maxHeight: 1080,
      maxFps: 30,
    });
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });
    const started = store.startCamera(track as unknown as MediaStreamTrack, {
      codec: "av1",
      chroma: "444",
      width: 320,
      height: 240,
      fps: 15,
    });

    await vi.advanceTimersByTimeAsync(1_049);
    expect(
      messages.some(
        (message) => message[0] === C2S_MEDIA_CONTROL && message[1] === 1,
      ),
    ).toBe(false);
    await vi.advanceTimersByTimeAsync(1);
    const start = await waitForMediaStart(messages);
    expect(messages.at(-2)?.[4]).toBe(VIDEO_CODECS_ALL);
    expect(start[7]).toBe(4);
    store.stop("camera");
    await expect(started).rejects.toThrow("cancelled");
  });

  it("sends AV1 4:4:4 frames with bounded keyframe recovery", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    const { encoders } = installVideoEncoderMocks({
      omitHeadersAfterFirstKey: true,
    });
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });
    const started = store.startCamera(track as unknown as MediaStreamTrack, {
      codec: "av1",
      chroma: "444",
      width: 320,
      height: 240,
      fps: 15,
    });
    const start = await waitForMediaStart(messages);
    expect(start[7]).toBe(4);
    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 1,
      leaseId: 88,
      codec: 4,
      width: 320,
      height: 240,
      fps: 15,
      initialCredit: 8 * 1024 * 1024,
    });
    await expect(started).resolves.toBeUndefined();

    await vi.advanceTimersByTimeAsync(70);
    let data = cameraDataMessages(messages);
    expect(data).toHaveLength(1);
    expect(data[0]![18]).toBe(4);
    expect(data[0]![19]).toBe(1);
    expect(data[0]!.subarray(28, 31)).toEqual(new Uint8Array([0x0a, 1, 0x20]));

    const liveEncoder = encoders.at(-1)!;
    liveEncoder.encodeQueueSize = 2;
    await vi.advanceTimersByTimeAsync(70);
    liveEncoder.encodeQueueSize = 0;
    await vi.advanceTimersByTimeAsync(70);
    data = cameraDataMessages(messages);
    expect(data).toHaveLength(2);
    expect(data[1]![19]).toBe(3);
    // The second encoder key omitted its sequence header. Capture prepends
    // the cached profile-1 sequence OBU before putting it on the wire.
    expect(data[1]!.subarray(28, 31)).toEqual(new Uint8Array([0x0a, 1, 0x20]));

    const dataBeforeCredit = data.length;
    const optionsBeforeCredit = liveEncoder.encodeOptions.length;
    store.handle({ kind: "credit", leaseId: 88, bytes: 1024, flags: 1 });
    await vi.advanceTimersByTimeAsync(70);
    data = cameraDataMessages(messages);
    expect(
      data.slice(dataBeforeCredit).some((item) => Boolean(item[19]! & 1)),
    ).toBe(true);
    expect(
      liveEncoder.encodeOptions
        .slice(optionsBeforeCredit)
        .some((item) => item.keyFrame === true),
    ).toBe(true);
    store.stop("camera");
    expect(liveEncoder.closed).toBe(true);
  });

  it("prepends cached H.264 SPS/PPS to recovery keyframes", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    installVideoEncoderMocks({ omitHeadersAfterFirstKey: true });
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });
    const started = store.startCamera(track as unknown as MediaStreamTrack, {
      codec: "h264",
      chroma: "444",
      width: 320,
      height: 240,
      fps: 15,
    });
    const start = await waitForMediaStart(messages);
    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 1,
      leaseId: 89,
      codec: 3,
      width: 320,
      height: 240,
      fps: 15,
      initialCredit: 8 * 1024 * 1024,
    });
    await started;
    await vi.advanceTimersByTimeAsync(70);
    store.handle({ kind: "credit", leaseId: 89, bytes: 1024, flags: 1 });
    await vi.advanceTimersByTimeAsync(70);

    const data = cameraDataMessages(messages);
    expect(data).toHaveLength(2);
    const recovery = data[1]!.subarray(28);
    expect(Array.from(recovery)).toContain(0x67);
    expect(Array.from(recovery)).toContain(0x68);
    expect(Array.from(recovery)).toContain(0x65);
    store.stop("camera");
  });

  it("emits a keyframe at least every two seconds", async () => {
    vi.useFakeTimers();
    const { track } = installCameraMocks();
    const { encoders } = installVideoEncoderMocks();
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    store.handle({ kind: "serverCapabilities", videoCodecs: VIDEO_CODECS_ALL });
    const started = store.startCamera(track as unknown as MediaStreamTrack, {
      codec: "h264",
      width: 320,
      height: 240,
      fps: 1,
    });
    const start = await waitForMediaStart(messages);
    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 1,
      leaseId: 90,
      codec: 1,
      width: 320,
      height: 240,
      fps: 1,
      initialCredit: 8 * 1024 * 1024,
    });
    await started;
    await vi.advanceTimersByTimeAsync(3_100);
    expect(encoders.at(-1)!.encodeOptions.map((item) => item.keyFrame)).toEqual(
      [true, false, true],
    );
    store.stop("camera");
  });

  it("does no MJPEG canvas work until an active lease has credit", async () => {
    vi.useFakeTimers();
    const { track, drawImage, getContext, toBlob } = installCameraMocks();
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    const started = store.startCamera(track as unknown as MediaStreamTrack, {
      codec: "mjpeg",
      width: 320,
      height: 240,
      fps: 15,
    });
    const start = await waitForMediaStart(messages);

    await vi.advanceTimersByTimeAsync(70);
    expect(getContext).toHaveBeenCalledOnce();
    expect(drawImage).not.toHaveBeenCalled();
    expect(toBlob).not.toHaveBeenCalled();

    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 1,
      leaseId: 88,
      codec: 0,
      width: 320,
      height: 240,
      fps: 15,
      initialCredit: 0,
    });
    await expect(started).resolves.toBeUndefined();
    await vi.advanceTimersByTimeAsync(70);
    expect(getContext).toHaveBeenCalledOnce();
    expect(drawImage).not.toHaveBeenCalled();
    expect(toBlob).not.toHaveBeenCalled();

    store.handle({ kind: "credit", leaseId: 88, bytes: 1024, flags: 0 });
    await vi.advanceTimersByTimeAsync(70);
    expect(getContext).toHaveBeenCalledTimes(2);
    expect(drawImage).toHaveBeenCalledOnce();
    expect(toBlob).toHaveBeenCalledOnce();
    store.stop("camera");
  });
});

describe("MediaStore microphone capture", () => {
  it("timestamps PCM from emitted sample frames instead of callback time", async () => {
    const { track, emitPcm } = installMicrophoneMocks(false);
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    const started = store.startMicrophone(
      track as unknown as MediaStreamTrack,
      {
        codec: "pcm",
      },
    );
    const start = await waitForMediaStart(messages);
    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 0,
      leaseId: 77,
      codec: 0,
      width: 0,
      height: 0,
      fps: 0,
      initialCredit: 64 * 1024,
    });
    await expect(started).resolves.toBeUndefined();

    emitPcm(new Float32Array(1921));

    const timestamps = messages
      .filter((message) => message[0] === C2S_MEDIA_DATA)
      .map((message) =>
        new DataView(
          message.buffer,
          message.byteOffset,
          message.byteLength,
        ).getBigUint64(9, true),
      );
    expect(timestamps).toEqual([0n, 20_000n]);
    store.stop("microphone");
  });

  it("stops a still-live microphone track when the Opus encoder fails", async () => {
    const { track, failEncoder } = installMicrophoneMocks();

    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    const started = store.startMicrophone(
      track as unknown as MediaStreamTrack,
      {
        codec: "opus",
      },
    );
    const start = await waitForMediaStart(messages);
    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 0,
      leaseId: 99,
      codec: 1,
      width: 0,
      height: 0,
      fps: 0,
      initialCredit: 64 * 1024,
    });
    await expect(started).resolves.toBeUndefined();

    failEncoder(new DOMException("encoder failed", "EncodingError"));
    expect(track.stop).toHaveBeenCalledOnce();
    expect(store.microphone.status).toBe("inactive");
    expect(store.microphone.error).toBe("encoder failed");
    expect(
      messages.some(
        (message) => message[0] === C2S_MEDIA_CONTROL && message[1] === 2,
      ),
    ).toBe(true);
  });

  it("defaults to Opus when supported and falls back to PCM when it is not", async () => {
    const supported = installMicrophoneMocks(true);
    const opusMessages: Uint8Array[] = [];
    const opusStore = new MediaStore();
    opusStore.setSender((message) => opusMessages.push(message));
    const opusStarted = opusStore.startMicrophone(
      supported.track as unknown as MediaStreamTrack,
    );
    const opusStart = await waitForMediaStart(opusMessages);
    expect(opusStart[7]).toBe(1);
    opusStore.stop("microphone");
    await expect(opusStarted).rejects.toThrow("cancelled");

    vi.unstubAllGlobals();
    const unsupported = installMicrophoneMocks(false);
    const pcmMessages: Uint8Array[] = [];
    const pcmStore = new MediaStore();
    pcmStore.setSender((message) => pcmMessages.push(message));
    const pcmStarted = pcmStore.startMicrophone(
      unsupported.track as unknown as MediaStreamTrack,
    );
    const pcmStart = await waitForMediaStart(pcmMessages);
    expect(pcmStart[7]).toBe(0);
    pcmStore.stop("microphone");
    await expect(pcmStarted).rejects.toThrow("cancelled");
  });

  it("stops a successful lease reply that arrives after local cancellation", async () => {
    const { track } = installMicrophoneMocks(false);
    const messages: Uint8Array[] = [];
    const store = new MediaStore();
    store.setSender((message) => messages.push(message));
    const started = store.startMicrophone(
      track as unknown as MediaStreamTrack,
      {
        codec: "pcm",
      },
    );
    const start = await waitForMediaStart(messages);
    store.stop("microphone");
    await expect(started).rejects.toThrow("cancelled");

    store.handle({
      kind: "lease",
      nonce: startNonce(start),
      status: 0,
      mediaKind: 0,
      leaseId: 123,
      codec: 0,
      width: 0,
      height: 0,
      fps: 0,
      initialCredit: 64 * 1024,
    });
    const stop = messages.find(
      (message) =>
        message[0] === C2S_MEDIA_CONTROL &&
        message[1] === 2 &&
        new DataView(
          message.buffer,
          message.byteOffset,
          message.byteLength,
        ).getUint32(2, true) === 123,
    );
    expect(stop).toBeDefined();
    expect(store.microphone.status).toBe("inactive");
  });
});
