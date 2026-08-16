import {
  releaseRecordingAudioSession,
  retainRecordingAudioSession,
} from "./audioSession";
import { fsDecompress } from "./fs";
import { Notifier, type ReactiveStore } from "./reactive";
import { av1LevelString } from "./videoCodec";

export const FEATURE_DESKTOP_MEDIA = 1 << 22;
export const C2S_MEDIA_CONTROL = 0x3e;
export const C2S_MEDIA_DATA = 0x3f;
export const S2C_MEDIA_CONTROL = 0x35;

export const RUNTIME_PIPEWIRE = 1 << 0;
export const RUNTIME_MICROPHONE = 1 << 1;
export const RUNTIME_CAMERA = 1 << 2;
export const RUNTIME_PORTAL_FRONTEND = 1 << 3;
export const RUNTIME_PORTAL_ACCESS = 1 << 4;
export const RUNTIME_PORTAL_SCREENCAST = 1 << 5;
export const RUNTIME_MPRIS = 1 << 6;

export const ACTIVE_MICROPHONE = 1 << 0;
export const ACTIVE_CAMERA = 1 << 1;
export const ACTIVE_SCREENCAST = 1 << 2;

export const CAPTURE_MICROPHONE = 1 << 0;
export const CAPTURE_CAMERA = 1 << 1;
export const CAPTURE_PORTAL_UI = 1 << 2;
export const AUDIO_CODEC_PCM = 1 << 0;
export const AUDIO_CODEC_OPUS = 1 << 1;
export const VIDEO_CODEC_MJPEG = 1 << 0;
export const VIDEO_CODEC_H264 = 1 << 1;
export const VIDEO_CODEC_AV1 = 1 << 2;
export const VIDEO_CODEC_H264_444 = 1 << 3;
export const VIDEO_CODEC_AV1_444 = 1 << 4;
export const VIDEO_CODECS_LEGACY = VIDEO_CODEC_MJPEG | VIDEO_CODEC_H264;
export const VIDEO_CODECS_ALL =
  VIDEO_CODECS_LEGACY |
  VIDEO_CODEC_AV1 |
  VIDEO_CODEC_H264_444 |
  VIDEO_CODEC_AV1_444;

export const MPRIS_UPDATE_RESET = 1 << 0;
export const MPRIS_UPDATE_SYNC = 1 << 1;
export const MPRIS_UPDATE_REPLAY = 1 << 2;
export const MPRIS_UPDATE_MAX_DECOMPRESSED = 16 * 1024 * 1024;
export const MPRIS_PLAYER_MAX = 32;
export const MPRIS_ARTIST_MAX = 16;
export const MPRIS_STRING_MAX = 4 * 1024;
export const MPRIS_ARTWORK_MAX = 512 * 1024;

export const MPRIS_CAN_CONTROL = 1 << 0;
export const MPRIS_CAN_PLAY = 1 << 1;
export const MPRIS_CAN_PAUSE = 1 << 2;
export const MPRIS_CAN_GO_NEXT = 1 << 3;
export const MPRIS_CAN_GO_PREVIOUS = 1 << 4;
export const MPRIS_CAN_SEEK = 1 << 5;
export const MPRIS_CAN_RAISE = 1 << 6;
export const MPRIS_CAN_SET_VOLUME = 1 << 7;
export const MPRIS_CAN_SET_SHUFFLE = 1 << 8;
export const MPRIS_CAN_SET_LOOP_STATUS = 1 << 9;
export const MPRIS_CAN_SET_RATE = 1 << 10;

const STATUS_OK = 0;
const RUNTIME_FLAGS_ALL =
  RUNTIME_PIPEWIRE |
  RUNTIME_MICROPHONE |
  RUNTIME_CAMERA |
  RUNTIME_PORTAL_FRONTEND |
  RUNTIME_PORTAL_ACCESS |
  RUNTIME_PORTAL_SCREENCAST |
  RUNTIME_MPRIS;
const ACTIVE_FLAGS_ALL = ACTIVE_MICROPHONE | ACTIVE_CAMERA | ACTIVE_SCREENCAST;
const MEDIA_DATA_FLAGS_ALL = 0b111;
const MEDIA_CREDIT_KEYFRAME = 1 << 0;
const MPRIS_CAPABILITIES_ALL = (1 << 11) - 1;
const MEDIA_FRAGMENT_MAX = 256 * 1024;
const MICROPHONE_FRAME_MAX = 64 * 1024;
const CAMERA_FRAME_MAX = 4 * 1024 * 1024;
/**
 * How much unsent camera video the transport may hold before the capture
 * stops adding to it, as time rather than bytes.
 *
 * Shorter than the server's lease window on purpose: credit only returns
 * once a frame has been decoded, so it reacts a whole round trip late, while
 * the send queue says the link is too slow the moment it is.
 */
const CAMERA_QUEUE_TARGET_MS = 200;
/** Allowance for a lease with no negotiated cadence to scale by. */
const CAMERA_QUEUE_MIN_BYTES = 64 * 1024;
/** How often the rate governor looks at the link. */
const CAMERA_GOVERNOR_INTERVAL_MS = 1_000;
const MEDIA_FRAGMENT_COUNT_MAX = 16;
const PORTAL_MESSAGE_MAX = 4 * 1024 * 1024;
const REVOKE_REASON_MAX = 7;
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

export interface MediaCapabilities {
  microphone: boolean;
  camera: boolean;
  portalUi: boolean;
  audioCodecs: number;
  videoCodecs: number;
  maxWidth: number;
  maxHeight: number;
  maxFps: number;
}

export interface ScreenCastState {
  sessionId: number;
  appId: string;
  surfaceIds: readonly number[];
}

export interface DesktopMediaState {
  runtimeFlags: number;
  activeFlags: number;
  microphoneOwner: bigint;
  cameraOwner: bigint;
  screencasts: readonly ScreenCastState[];
}

export type MediaLeaseStatus = "inactive" | "starting" | "active";

export interface MediaLeaseState {
  kind: "microphone" | "camera";
  status: MediaLeaseStatus;
  leaseId: number;
  codec: number;
  width: number;
  height: number;
  fps: number;
  credit: number;
  error: string | null;
}

export interface MicrophoneOptions {
  /** Defaults to Opus when WebCodecs can encode it, otherwise PCM. */
  codec?: "pcm" | "opus";
}

export interface CameraOptions {
  /**
   * Omit to choose the best exact format supported by this browser. `h264`
   * and `av1` retain their 8-bit 4:2:0 meaning unless `chroma` is explicit.
   */
  codec?: "mjpeg" | "h264" | "av1";
  /** Exact chroma sampling for H.264/AV1. Motion JPEG does not expose this. */
  chroma?: "420" | "444";
  width?: number;
  height?: number;
  fps?: number;
  /**
   * How many bits the picture is worth. Scales the computed bitrate for the
   * compressed codecs and the JPEG quantizer for Motion JPEG — the same
   * intent expressed in whichever currency the codec takes.
   */
  quality?: CameraQuality;
}

export type CameraQuality = "low" | "balanced" | "high";

/** Bitrate multiplier and JPEG quality per quality step. */
const CAMERA_QUALITY: Record<CameraQuality, { scale: number; jpeg: number }> = {
  low: { scale: 0.5, jpeg: 0.6 },
  balanced: { scale: 1, jpeg: 0.8 },
  high: { scale: 2, jpeg: 0.92 },
};

function cameraQuality(quality: CameraQuality | undefined) {
  return CAMERA_QUALITY[quality ?? "balanced"] ?? CAMERA_QUALITY.balanced;
}

export interface PortalChoiceValue {
  id: string;
  value: string;
}

export interface PortalChoice {
  id: string;
  label: string;
  options: readonly PortalChoiceValue[];
  initialValue: string;
}

interface PortalRequestBase {
  requestId: number;
  deadlineMs: number;
  parentSurfaceId: number | null;
  appId: string;
}

export interface PortalAccessRequest extends PortalRequestBase {
  kind: "access";
  title: string;
  subtitle: string;
  body: string;
  denyLabel: string;
  grantLabel: string;
  iconName: string;
  choices: readonly PortalChoice[];
}

export interface ScreenCastCandidate {
  surfaceId: number;
  width: number;
  height: number;
  title: string;
  appId: string;
  thumbnailPng: Uint8Array;
}

export interface PortalScreenCastRequest extends PortalRequestBase {
  kind: "screencast";
  multiple: boolean;
  candidates: readonly ScreenCastCandidate[];
}

export type PortalRequest = PortalAccessRequest | PortalScreenCastRequest;

export type PlaybackStatus = "stopped" | "paused" | "playing";
export type LoopStatus = "none" | "track" | "playlist";

/**
 * How a player's cover arrives.
 *
 * Catalogue-backed players (Spotify and friends) name their cover with an
 * `https:` URL and keep no local copy, so the server forwards that URL and the
 * browser loads and caches it: re-encoding it server-side would put ~150 KiB of
 * PNG in every upsert. Art that exists only on the server's disk cannot be
 * named to a browser, so it still arrives as bytes.
 */
export type MprisArtwork =
  | { kind: "url"; url: string }
  | { kind: "png"; png: Uint8Array };

export const ARTWORK_KIND_NONE = 0;
export const ARTWORK_KIND_URL = 1;
export const ARTWORK_KIND_PNG = 2;

/**
 * The only schemes this client will put in an image source. Enforced here as
 * well as on the server, because the value reaches the DOM.
 */
export function artworkUrlAllowed(url: string): boolean {
  if (url.length === 0 || url.length > MPRIS_STRING_MAX) return false;
  const separator = url.indexOf("://");
  if (separator <= 0 || separator + 3 >= url.length) return false;
  const scheme = url.slice(0, separator).toLowerCase();
  return scheme === "https" || scheme === "http";
}

export interface MprisPlayer {
  playerId: number;
  revision: number;
  trackRevision: number;
  active: boolean;
  playbackStatus: PlaybackStatus;
  loopStatus: LoopStatus;
  shuffle: boolean;
  capabilityFlags: number;
  rate: number;
  minimumRate: number;
  maximumRate: number;
  volume: number;
  positionUs: number;
  lengthUs: number;
  identity: string;
  desktopEntry: string;
  title: string;
  album: string;
  artists: readonly string[];
  artwork: MprisArtwork | null;
  /** Local monotonic receipt anchor; never compared with a server clock. */
  receivedAtMs: number;
}

export type MprisAction =
  | { kind: "select" }
  | { kind: "play" }
  | { kind: "pause" }
  | { kind: "playPause" }
  | { kind: "stop" }
  | { kind: "next" }
  | { kind: "previous" }
  | { kind: "seek"; offsetUs: number }
  | { kind: "setPosition"; positionUs: number; trackRevision: number }
  | { kind: "volume"; volume: number }
  | { kind: "shuffle"; shuffle: boolean }
  | { kind: "loopStatus"; loopStatus: LoopStatus }
  | { kind: "rate"; rate: number }
  | { kind: "raise" };

type MprisRecord =
  | { kind: "delete"; playerId: number }
  | { kind: "upsert"; player: MprisPlayer };

class Reader {
  readonly #data: Uint8Array;
  readonly #view: DataView;
  offset = 0;

  constructor(data: Uint8Array) {
    this.#data = data;
    this.#view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  }

  take(length: number): Uint8Array {
    const end = this.offset + length;
    if (!Number.isSafeInteger(end) || length < 0 || end > this.#data.length) {
      throw new Error("media record overrun");
    }
    const value = this.#data.subarray(this.offset, end);
    this.offset = end;
    return value;
  }

  u8(): number {
    return this.take(1)[0]!;
  }
  bool(): boolean {
    const value = this.u8();
    if (value > 1) throw new Error("invalid boolean");
    return value === 1;
  }
  u16(): number {
    const value = this.#view.getUint16(this.offset, true);
    this.take(2);
    return value;
  }
  u32(): number {
    const value = this.#view.getUint32(this.offset, true);
    this.take(4);
    return value;
  }
  i32(): number {
    const value = this.#view.getInt32(this.offset, true);
    this.take(4);
    return value;
  }
  u64(): bigint {
    const value = this.#view.getBigUint64(this.offset, true);
    this.take(8);
    return value;
  }
  i64(): number {
    const value = this.#view.getBigInt64(this.offset, true);
    this.take(8);
    const max = BigInt(Number.MAX_SAFE_INTEGER);
    const min = BigInt(Number.MIN_SAFE_INTEGER);
    return Number(value > max ? max : value < min ? min : value);
  }
  string16(max = 0xffff): string {
    const length = this.u16();
    if (length > max) throw new Error("string too large");
    return decoder.decode(this.take(length));
  }
  string32(max: number): string {
    const length = this.u32();
    if (length > max) throw new Error("string too large");
    return decoder.decode(this.take(length));
  }
  bytes32(max: number): Uint8Array {
    const length = this.u32();
    if (length > max) throw new Error("byte field too large");
    return this.take(length).slice();
  }
  get done(): boolean {
    return this.offset === this.#data.length;
  }
}

export type ParsedControl =
  | { kind: "state"; state: DesktopMediaState }
  | {
      kind: "lease";
      nonce: number;
      status: number;
      mediaKind: number;
      leaseId: number;
      codec: number;
      width: number;
      height: number;
      fps: number;
      initialCredit: number;
    }
  | { kind: "revoked"; leaseId: number; reason: number }
  | { kind: "credit"; leaseId: number; bytes: number; flags: number }
  | { kind: "portalRequest"; request: PortalRequest }
  | { kind: "portalCancel"; requestId: number; reason: number }
  | { kind: "mprisUpdate"; flags: number; records: MprisRecord[] }
  | {
      kind: "mprisResult";
      nonce: number;
      status: number;
      playerId: number;
      revision: number;
    }
  | { kind: "serverCapabilities"; videoCodecs: number };

export function parseMediaControl(message: Uint8Array): ParsedControl | null {
  if (message.length < 2 || message[0] !== S2C_MEDIA_CONTROL) return null;
  try {
    const subtype = message[1]!;
    const reader = new Reader(message.subarray(2));
    let parsed: ParsedControl | null = null;
    if (subtype === 0) {
      const runtimeFlags = reader.u8();
      const activeFlags = reader.u8();
      if (
        runtimeFlags & ~RUNTIME_FLAGS_ALL ||
        activeFlags & ~ACTIVE_FLAGS_ALL
      ) {
        return null;
      }
      const microphoneOwner = reader.u64();
      const cameraOwner = reader.u64();
      const count = reader.u8();
      if (count > 4) return null;
      const screencasts: ScreenCastState[] = [];
      const sessionIds = new Set<number>();
      for (let i = 0; i < count; i++) {
        const sessionId = reader.u32();
        const appId = reader.string16(MPRIS_STRING_MAX);
        const surfaceCount = reader.u8();
        if (
          !sessionId ||
          sessionIds.has(sessionId) ||
          !surfaceCount ||
          surfaceCount > 4
        ) {
          return null;
        }
        sessionIds.add(sessionId);
        const surfaceIds: number[] = [];
        for (let j = 0; j < surfaceCount; j++) {
          const id = reader.u16();
          if (!id || surfaceIds.includes(id)) return null;
          surfaceIds.push(id);
        }
        screencasts.push({ sessionId, appId, surfaceIds });
      }
      if (
        Boolean(activeFlags & ACTIVE_MICROPHONE) !== (microphoneOwner !== 0n) ||
        Boolean(activeFlags & ACTIVE_CAMERA) !== (cameraOwner !== 0n) ||
        Boolean(activeFlags & ACTIVE_SCREENCAST) !== screencasts.length > 0
      ) {
        return null;
      }
      parsed = {
        kind: "state",
        state: {
          runtimeFlags,
          activeFlags,
          microphoneOwner,
          cameraOwner,
          screencasts,
        },
      };
    } else if (subtype === 1) {
      const nonce = reader.u32();
      const status = reader.u8();
      const mediaKind = reader.u8();
      const leaseId = reader.u32();
      const codec = reader.u8();
      const width = reader.u16();
      const height = reader.u16();
      const fps = reader.u8();
      const initialCredit = reader.u32();
      if (
        !nonce ||
        mediaKind > 1 ||
        (status === STATUS_OK) !== (leaseId !== 0)
      ) {
        return null;
      }
      parsed = {
        kind: "lease",
        nonce,
        status,
        mediaKind,
        leaseId,
        codec,
        width,
        height,
        fps,
        initialCredit,
      };
    } else if (subtype === 2) {
      const leaseId = reader.u32();
      const reason = reader.u8();
      if (!leaseId || reason > REVOKE_REASON_MAX) return null;
      parsed = {
        kind: "revoked",
        leaseId,
        reason,
      };
    } else if (subtype === 3) {
      const leaseId = reader.u32();
      const bytes = reader.u32();
      const flags = reader.u8();
      if (!leaseId || flags & ~MEDIA_CREDIT_KEYFRAME) return null;
      parsed = {
        kind: "credit",
        leaseId,
        bytes,
        flags,
      };
    } else if (subtype === 4) {
      if (message.length > PORTAL_MESSAGE_MAX) return null;
      const requestId = reader.u32();
      if (!requestId) return null;
      const portalKind = reader.u8();
      const deadlineMs = reader.u32();
      const parent = reader.u16();
      if (portalKind > 1) return null;
      const appId = reader.string16(MPRIS_STRING_MAX);
      if (portalKind === 0) {
        const title = reader.string16(MPRIS_STRING_MAX);
        const subtitle = reader.string16(MPRIS_STRING_MAX);
        const body = reader.string32(16 * 1024);
        const denyLabel = reader.string16(MPRIS_STRING_MAX);
        const grantLabel = reader.string16(MPRIS_STRING_MAX);
        const iconName = reader.string16(MPRIS_STRING_MAX);
        const count = reader.u8();
        if (count > 16) return null;
        const choices: PortalChoice[] = [];
        for (let i = 0; i < count; i++) {
          const id = reader.string16(MPRIS_STRING_MAX);
          const label = reader.string16(MPRIS_STRING_MAX);
          const optionCount = reader.u8();
          if (optionCount > 32) return null;
          const options: PortalChoiceValue[] = [];
          for (let j = 0; j < optionCount; j++) {
            options.push({
              id: reader.string16(MPRIS_STRING_MAX),
              value: reader.string16(MPRIS_STRING_MAX),
            });
          }
          choices.push({
            id,
            label,
            options,
            initialValue: reader.string16(MPRIS_STRING_MAX),
          });
        }
        parsed = {
          kind: "portalRequest",
          request: {
            kind: "access",
            requestId,
            deadlineMs,
            parentSurfaceId: parent || null,
            appId,
            title,
            subtitle,
            body,
            denyLabel,
            grantLabel,
            iconName,
            choices,
          },
        };
      } else if (portalKind === 1) {
        const multiple = reader.bool();
        const count = reader.u8();
        if (count > 64) return null;
        const candidates: ScreenCastCandidate[] = [];
        const candidateIds = new Set<number>();
        for (let i = 0; i < count; i++) {
          const surfaceId = reader.u16();
          if (!surfaceId || candidateIds.has(surfaceId)) return null;
          candidateIds.add(surfaceId);
          candidates.push({
            surfaceId,
            width: reader.u16(),
            height: reader.u16(),
            title: reader.string16(MPRIS_STRING_MAX),
            appId: reader.string16(MPRIS_STRING_MAX),
            thumbnailPng: reader.bytes32(64 * 1024),
          });
        }
        parsed = {
          kind: "portalRequest",
          request: {
            kind: "screencast",
            requestId,
            deadlineMs,
            parentSurfaceId: parent || null,
            appId,
            multiple,
            candidates,
          },
        };
      }
    } else if (subtype === 5) {
      const requestId = reader.u32();
      const reason = reader.u8();
      if (!requestId) return null;
      parsed = {
        kind: "portalCancel",
        requestId,
        reason,
      };
    } else if (subtype === 6) {
      return parseMprisUpdate(message);
    } else if (subtype === 7) {
      const nonce = reader.u32();
      const status = reader.u8();
      const playerId = reader.u32();
      const revision = reader.u32();
      if (!nonce || !playerId) return null;
      parsed = {
        kind: "mprisResult",
        nonce,
        status,
        playerId,
        revision,
      };
    } else if (subtype === 8) {
      const videoCodecs = reader.u8();
      if (
        videoCodecs & ~VIDEO_CODECS_ALL ||
        !(videoCodecs & VIDEO_CODEC_MJPEG)
      ) {
        return null;
      }
      parsed = { kind: "serverCapabilities", videoCodecs };
    } else {
      return null;
    }
    return reader.done ? parsed : null;
  } catch {
    return null;
  }
}

function parseMprisUpdate(message: Uint8Array): ParsedControl | null {
  if (message.length < 8) return null;
  const flags = message[2]!;
  if (flags & ~(MPRIS_UPDATE_RESET | MPRIS_UPDATE_SYNC | MPRIS_UPDATE_REPLAY)) {
    return null;
  }
  const declared = new DataView(
    message.buffer,
    message.byteOffset + 3,
    4,
  ).getUint32(0, true);
  if (declared > MPRIS_UPDATE_MAX_DECOMPRESSED) return null;
  const data = fsDecompress(message.subarray(3));
  if (!data || data.length !== declared) return null;
  try {
    const reader = new Reader(data);
    const count = reader.u8();
    if (count > MPRIS_PLAYER_MAX) return null;
    const records: MprisRecord[] = [];
    for (let i = 0; i < count; i++) {
      const op = reader.u8();
      const playerId = reader.u32();
      if (!playerId) return null;
      if (op === 0) {
        records.push({ kind: "delete", playerId });
        continue;
      }
      if (op !== 1) return null;
      const revision = reader.u32();
      const trackRevision = reader.u32();
      const active = reader.bool();
      const playback = reader.u8();
      const loop = reader.u8();
      const shuffle = reader.bool();
      if (playback > 2 || loop > 2) return null;
      const capabilityFlags = reader.u16();
      if (capabilityFlags & ~MPRIS_CAPABILITIES_ALL) return null;
      const rate = reader.i32() / 1_000_000;
      const minimumRate = reader.i32() / 1_000_000;
      const maximumRate = reader.i32() / 1_000_000;
      const volume = reader.u32() / 1_000_000;
      const positionUs = Math.max(0, reader.i64());
      const lengthUs = reader.i64();
      const identity = reader.string16(MPRIS_STRING_MAX);
      const desktopEntry = reader.string16(MPRIS_STRING_MAX);
      const title = reader.string16(MPRIS_STRING_MAX);
      const album = reader.string16(MPRIS_STRING_MAX);
      const artistCount = reader.u8();
      if (artistCount > MPRIS_ARTIST_MAX) return null;
      const artists: string[] = [];
      for (let j = 0; j < artistCount; j++) {
        artists.push(reader.string16(MPRIS_STRING_MAX));
      }
      let artwork: MprisArtwork | null = null;
      switch (reader.u8()) {
        case ARTWORK_KIND_NONE:
          break;
        case ARTWORK_KIND_URL: {
          const url = reader.string16(MPRIS_STRING_MAX);
          if (!artworkUrlAllowed(url)) return null;
          artwork = { kind: "url", url };
          break;
        }
        case ARTWORK_KIND_PNG: {
          const png = reader.bytes32(MPRIS_ARTWORK_MAX);
          if (png.length === 0) return null;
          artwork = { kind: "png", png };
          break;
        }
        default:
          return null;
      }
      records.push({
        kind: "upsert",
        player: {
          playerId,
          revision,
          trackRevision,
          active,
          playbackStatus: ["stopped", "paused", "playing"][
            playback
          ] as PlaybackStatus,
          loopStatus: ["none", "track", "playlist"][loop] as LoopStatus,
          shuffle,
          capabilityFlags,
          rate,
          minimumRate,
          maximumRate,
          volume,
          positionUs,
          lengthUs,
          identity,
          desktopEntry,
          title,
          album,
          artists,
          artwork,
          receivedAtMs: monotonicNow(),
        },
      });
    }
    return reader.done ? { kind: "mprisUpdate", flags, records } : null;
  } catch {
    return null;
  }
}

export function buildMediaCapabilitiesMessage(
  capabilities: MediaCapabilities,
): Uint8Array {
  const message = new Uint8Array(10);
  const view = new DataView(message.buffer);
  message.set([
    C2S_MEDIA_CONTROL,
    0,
    (capabilities.microphone ? CAPTURE_MICROPHONE : 0) |
      (capabilities.camera ? CAPTURE_CAMERA : 0) |
      (capabilities.portalUi ? CAPTURE_PORTAL_UI : 0),
    capabilities.audioCodecs & 3,
    capabilities.videoCodecs & VIDEO_CODECS_ALL,
  ]);
  view.setUint16(5, clampInt(capabilities.maxWidth, 0xffff), true);
  view.setUint16(7, clampInt(capabilities.maxHeight, 0xffff), true);
  message[9] = clampInt(capabilities.maxFps, 0xff);
  return message;
}

export function buildMediaStartMessage(
  nonce: number,
  kind: "microphone" | "camera",
  codec: number,
  width = 0,
  height = 0,
  fps = 0,
): Uint8Array {
  const encodedNonce = requireUnsigned(nonce, 0xffffffff, "nonce", true);
  if (kind !== "microphone" && kind !== "camera") {
    throw new RangeError("unknown media kind");
  }
  const encodedCodec = requireUnsigned(codec, 0xff, "codec");
  const encodedWidth = requireUnsigned(width, 0xffff, "width");
  const encodedHeight = requireUnsigned(height, 0xffff, "height");
  const encodedFps = requireUnsigned(fps, 0xff, "fps");
  if (
    (kind === "microphone" &&
      (encodedWidth !== 0 || encodedHeight !== 0 || encodedFps !== 0)) ||
    (kind === "camera" &&
      (encodedWidth === 0 || encodedHeight === 0 || encodedFps === 0))
  ) {
    throw new RangeError("invalid media format");
  }
  const message = new Uint8Array(13);
  const view = new DataView(message.buffer);
  message.set([C2S_MEDIA_CONTROL, 1]);
  view.setUint32(2, encodedNonce, true);
  message[6] = kind === "microphone" ? 0 : 1;
  message[7] = encodedCodec;
  view.setUint16(8, encodedWidth, true);
  view.setUint16(10, encodedHeight, true);
  message[12] = encodedFps;
  return message;
}

export function buildMediaStopMessage(leaseId: number): Uint8Array {
  const encodedLeaseId = requireUnsigned(leaseId, 0xffffffff, "lease id", true);
  const message = new Uint8Array(6);
  message.set([C2S_MEDIA_CONTROL, 2]);
  new DataView(message.buffer).setUint32(2, encodedLeaseId, true);
  return message;
}

export function buildMediaDataMessage(fields: {
  leaseId: number;
  sequence: number;
  captureUs: number;
  kind: "microphone" | "camera";
  codec: number;
  flags: number;
  fragmentIndex: number;
  fragmentCount: number;
  frameLength: number;
  data: Uint8Array;
}): Uint8Array {
  const leaseId = requireUnsigned(fields.leaseId, 0xffffffff, "lease id", true);
  const sequence = requireUnsigned(fields.sequence, 0xffffffff, "sequence");
  const captureUs = requireUnsigned(
    fields.captureUs,
    Number.MAX_SAFE_INTEGER,
    "capture timestamp",
  );
  if (fields.kind !== "microphone" && fields.kind !== "camera") {
    throw new RangeError("unknown media kind");
  }
  const codec = requireUnsigned(fields.codec, 0xff, "codec");
  const flags = requireUnsigned(fields.flags, 0xff, "media data flags");
  if (flags & ~MEDIA_DATA_FLAGS_ALL) {
    throw new RangeError("unknown media data flags");
  }
  const fragmentIndex = requireUnsigned(
    fields.fragmentIndex,
    0xffff,
    "fragment index",
  );
  const fragmentCount = requireUnsigned(
    fields.fragmentCount,
    0xffff,
    "fragment count",
    true,
  );
  const frameLength = requireUnsigned(
    fields.frameLength,
    0xffffffff,
    "frame length",
  );
  const frameMax =
    fields.kind === "microphone" ? MICROPHONE_FRAME_MAX : CAMERA_FRAME_MAX;
  if (
    fragmentCount > MEDIA_FRAGMENT_COUNT_MAX ||
    fragmentIndex >= fragmentCount ||
    fields.data.length > MEDIA_FRAGMENT_MAX ||
    frameLength > frameMax ||
    fields.data.length > frameLength ||
    (fragmentCount === 1 && fields.data.length !== frameLength)
  ) {
    throw new RangeError("invalid media fragmentation");
  }
  const message = new Uint8Array(28 + fields.data.length);
  const view = new DataView(message.buffer);
  message[0] = C2S_MEDIA_DATA;
  view.setUint32(1, leaseId, true);
  view.setUint32(5, sequence, true);
  view.setBigUint64(9, BigInt(captureUs), true);
  message[17] = fields.kind === "microphone" ? 0 : 1;
  message[18] = codec;
  message[19] = flags;
  view.setUint16(20, fragmentIndex, true);
  view.setUint16(22, fragmentCount, true);
  view.setUint32(24, frameLength, true);
  message.set(fields.data, 28);
  return message;
}

export function buildMprisSubscribeMessage(enabled: boolean): Uint8Array {
  return new Uint8Array([C2S_MEDIA_CONTROL, 5, enabled ? 1 : 0]);
}

function actionFields(action: MprisAction): {
  kind: number;
  trackRevision: number;
  value: bigint;
} {
  switch (action.kind) {
    case "select":
      return { kind: 0, trackRevision: 0, value: 0n };
    case "play":
      return { kind: 1, trackRevision: 0, value: 0n };
    case "pause":
      return { kind: 2, trackRevision: 0, value: 0n };
    case "playPause":
      return { kind: 3, trackRevision: 0, value: 0n };
    case "stop":
      return { kind: 4, trackRevision: 0, value: 0n };
    case "next":
      return { kind: 5, trackRevision: 0, value: 0n };
    case "previous":
      return { kind: 6, trackRevision: 0, value: 0n };
    case "seek":
      return {
        kind: 7,
        trackRevision: 0,
        value: actionBigInt(action.offsetUs, "seek offset"),
      };
    case "setPosition":
      return {
        kind: 8,
        trackRevision: action.trackRevision,
        value: actionBigInt(action.positionUs, "position"),
      };
    case "volume":
      return {
        kind: 9,
        trackRevision: 0,
        value: actionBigInt(action.volume, "volume", 1_000_000, Math.round),
      };
    case "shuffle":
      return { kind: 10, trackRevision: 0, value: action.shuffle ? 1n : 0n };
    case "loopStatus":
      if (
        !(["none", "track", "playlist"] as const).includes(action.loopStatus)
      ) {
        throw new RangeError("unknown loop status");
      }
      return {
        kind: 11,
        trackRevision: 0,
        value: BigInt(["none", "track", "playlist"].indexOf(action.loopStatus)),
      };
    case "rate":
      return {
        kind: 12,
        trackRevision: 0,
        value: actionBigInt(action.rate, "rate", 1_000_000, Math.round),
      };
    case "raise":
      return { kind: 13, trackRevision: 0, value: 0n };
    default:
      throw new RangeError("unknown MPRIS action");
  }
}

export function buildMprisActionMessage(
  nonce: number,
  playerId: number,
  action: MprisAction,
): Uint8Array {
  const encodedNonce = requireUnsigned(nonce, 0xffffffff, "nonce", true);
  const encodedPlayerId = requireUnsigned(
    playerId,
    0xffffffff,
    "player id",
    true,
  );
  const fields = actionFields(action);
  const trackRevision = requireUnsigned(
    fields.trackRevision,
    0xffffffff,
    "track revision",
    fields.kind === 8,
  );
  if (fields.kind !== 8 && trackRevision !== 0) {
    throw new RangeError("unexpected track revision");
  }
  if (fields.value < -(1n << 63n) || fields.value >= 1n << 63n) {
    throw new RangeError("MPRIS action value is outside i64");
  }
  const message = new Uint8Array(23);
  const view = new DataView(message.buffer);
  message.set([C2S_MEDIA_CONTROL, 6]);
  view.setUint32(2, encodedNonce, true);
  view.setUint32(6, encodedPlayerId, true);
  message[10] = fields.kind;
  view.setUint32(11, trackRevision, true);
  view.setBigInt64(15, fields.value, true);
  return message;
}

export function buildPortalReplyMessage(
  request: PortalRequest,
  decision: "deny" | "grant" | "cancelled",
  surfaceIds: readonly number[] = [],
  choices: readonly PortalChoiceValue[] = [],
): Uint8Array {
  if (request.kind !== "access" && request.kind !== "screencast") {
    throw new RangeError("unknown portal request kind");
  }
  const decisionCode = { deny: 0, grant: 1, cancelled: 2 }[decision];
  if (decisionCode === undefined) {
    throw new RangeError("unknown portal decision");
  }
  const requestId = requireUnsigned(
    request.requestId,
    0xffffffff,
    "request id",
    true,
  );
  const bytes: number[] = [C2S_MEDIA_CONTROL, 3];
  pushU32(bytes, requestId);
  bytes.push(decisionCode);
  let surfaces: readonly number[] = [];
  let values: readonly PortalChoiceValue[] = [];
  if (decision === "grant" && request.kind === "screencast") {
    if (
      surfaceIds.length < 1 ||
      surfaceIds.length > 4 ||
      (!request.multiple && surfaceIds.length !== 1) ||
      choices.length !== 0
    ) {
      throw new RangeError("invalid ScreenCast portal grant");
    }
    const candidates = new Set(
      request.candidates.map((candidate) => candidate.surfaceId),
    );
    if (surfaceIds.some((id) => !candidates.has(id))) {
      throw new RangeError("unknown ScreenCast surface");
    }
    surfaces = surfaceIds;
  } else if (decision === "grant" && request.kind === "access") {
    if (
      surfaceIds.length !== 0 ||
      request.choices.length > 16 ||
      choices.length !== request.choices.length
    ) {
      throw new RangeError("invalid Access portal grant");
    }
    const supplied = new Map<string, string>();
    for (const choice of choices) {
      if (supplied.has(choice.id)) {
        throw new RangeError("duplicate portal choice");
      }
      supplied.set(choice.id, choice.value);
    }
    values = request.choices.map((choice) => {
      const value = supplied.get(choice.id);
      if (
        value === undefined ||
        !choice.options.some((option) => option.id === value)
      ) {
        throw new RangeError("invalid portal choice");
      }
      return { id: choice.id, value };
    });
  }
  bytes.push(surfaces.length);
  const seenSurfaces = new Set<number>();
  for (const id of surfaces) {
    const encodedId = requireUnsigned(id, 0xffff, "surface id", true);
    if (seenSurfaces.has(encodedId)) {
      throw new RangeError("duplicate surface id");
    }
    seenSurfaces.add(encodedId);
    pushU16(bytes, encodedId);
  }
  bytes.push(values.length);
  for (const choice of values) {
    pushString16(bytes, choice.id, MPRIS_STRING_MAX);
    pushString16(bytes, choice.value, MPRIS_STRING_MAX);
  }
  return new Uint8Array(bytes);
}

export function buildScreenCastStopMessage(sessionId: number): Uint8Array {
  const encodedSessionId = requireUnsigned(
    sessionId,
    0xffffffff,
    "session id",
    true,
  );
  const message = new Uint8Array(6);
  message.set([C2S_MEDIA_CONTROL, 4]);
  new DataView(message.buffer).setUint32(2, encodedSessionId, true);
  return message;
}

export class MprisStore implements ReactiveStore {
  readonly #notifier = new Notifier();
  readonly #players = new Map<number, MprisPlayer>();
  #staging: Map<number, MprisPlayer> | null = null;
  #sender: ((message: Uint8Array) => void) | null = null;
  #pending = new Map<
    number,
    {
      playerId: number;
      resolve: () => void;
      reject: (error: Error) => void;
      timer: ReturnType<typeof setTimeout>;
    }
  >();
  #nextNonce = 0;
  #subscribed = false;

  get revision(): number {
    return this.#notifier.revision;
  }

  get players(): ReadonlyMap<number, MprisPlayer> {
    return this.#players;
  }

  get activePlayerId(): number | null {
    for (const player of this.#players.values()) {
      if (player.active) return player.playerId;
    }
    return null;
  }

  get activePlayer(): MprisPlayer | null {
    const id = this.activePlayerId;
    return id === null ? null : (this.#players.get(id) ?? null);
  }

  subscribe(listener: () => void): () => void;
  subscribe(enabled: boolean): void;
  subscribe(value: boolean | (() => void)): void | (() => void) {
    if (typeof value === "function") return this.#notifier.subscribe(value);
    this.#subscribed = value;
    this.#sender?.(buildMprisSubscribeMessage(value));
  }

  setSender(sender: ((message: Uint8Array) => void) | null): void {
    this.#sender = sender;
  }

  select(playerId: number): Promise<void> {
    return this.act(playerId, { kind: "select" });
  }

  act(playerId: number, action: MprisAction): Promise<void> {
    if (!this.#sender)
      return Promise.reject(new Error("connection unavailable"));
    const nonce = this.#allocateNonce();
    let message: Uint8Array;
    try {
      message = buildMprisActionMessage(nonce, playerId, action);
    } catch (error) {
      return Promise.reject(
        error instanceof Error ? error : new Error(String(error)),
      );
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const pending = this.#pending.get(nonce);
        if (!pending) return;
        this.#pending.delete(nonce);
        pending.reject(new Error("MPRIS action timed out"));
      }, 10_000);
      this.#pending.set(nonce, { playerId, resolve, reject, timer });
      this.#sender!(message);
    });
  }

  positionUs(playerId: number, nowMs = monotonicNow()): number {
    const player = this.#players.get(playerId);
    if (!player) return 0;
    let position = player.positionUs;
    if (player.playbackStatus === "playing") {
      position +=
        Math.max(0, nowMs - player.receivedAtMs) * 1_000 * player.rate;
    }
    if (player.lengthUs >= 0) position = Math.min(position, player.lengthUs);
    return Math.max(0, Math.round(position));
  }

  handle(control: ParsedControl): boolean {
    if (control.kind === "mprisResult") {
      const pending = this.#pending.get(control.nonce);
      if (!pending) return true;
      if (pending.playerId !== control.playerId) return true;
      this.#pending.delete(control.nonce);
      clearTimeout(pending.timer);
      if (control.status === STATUS_OK) pending.resolve();
      else pending.reject(new Error(`MPRIS action failed (${control.status})`));
      return true;
    }
    if (control.kind !== "mprisUpdate") return false;
    if (control.flags & MPRIS_UPDATE_RESET) this.#staging = new Map();
    const target = this.#staging ?? this.#players;
    for (const record of control.records) {
      if (record.kind === "delete") target.delete(record.playerId);
      else target.set(record.player.playerId, record.player);
    }
    if (control.flags & MPRIS_UPDATE_SYNC && this.#staging) {
      this.#players.clear();
      for (const [id, player] of this.#staging) this.#players.set(id, player);
      this.#staging = null;
      this.#notifier.emit();
    } else if (!this.#staging && control.records.length) {
      this.#notifier.emit();
    }
    return true;
  }

  reconnect(): void {
    this.reset();
    if (this.#subscribed) this.#sender?.(buildMprisSubscribeMessage(true));
  }

  reset(error = new Error("MPRIS state reset")): void {
    const changed = this.#players.size > 0;
    this.#players.clear();
    this.#staging = null;
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
    if (changed) this.#notifier.emit();
  }

  #allocateNonce(): number {
    do this.#nextNonce = (this.#nextNonce + 1) >>> 0 || 1;
    while (this.#pending.has(this.#nextNonce));
    return this.#nextNonce;
  }
}

const microphoneWorklet = `
class BlitMicrophoneProcessor extends AudioWorkletProcessor {
  process(inputs) {
    const channel = inputs[0]?.[0];
    if (channel?.length) {
      const copy = channel.slice();
      this.port.postMessage(copy.buffer, [copy.buffer]);
    }
    return true;
  }
}
registerProcessor("blit-microphone", BlitMicrophoneProcessor);
`;

class PcmMicrophoneCapture {
  readonly track: MediaStreamTrack;
  readonly #frame: (pcm: Uint8Array, captureUs: number) => void;
  readonly #ended: () => void;
  #context: AudioContext | null = null;
  #source: MediaStreamAudioSourceNode | null = null;
  #node: AudioWorkletNode | null = null;
  #sink: GainNode | null = null;
  #pending = new Float32Array(0);
  #cursor = 0;
  #samples: number[] = [];
  #emittedSamples = 0;
  /** Whether this capture still owns a recording claim. `stop()` runs on
   *  several paths — server revoke, device ended, teardown — and a claim
   *  released twice would strand a second capture's session on playback. */
  #recordingClaim = false;

  constructor(
    track: MediaStreamTrack,
    frame: (pcm: Uint8Array, captureUs: number) => void,
    ended: () => void,
  ) {
    this.track = track;
    this.#frame = frame;
    this.#ended = ended;
  }

  async start(): Promise<void> {
    if (this.track.kind !== "audio" || this.track.readyState !== "live") {
      throw new Error("microphone track is not live");
    }
    // Before the context exists: iOS routes Bluetooth when a capture-carrying
    // context is created, so the category has to be recording-capable by then
    // rather than once samples start flowing.
    this.#recordingClaim = true;
    retainRecordingAudioSession();
    const context = new AudioContext({ latencyHint: "interactive" });
    this.#context = context;
    const url = URL.createObjectURL(
      new Blob([microphoneWorklet], { type: "text/javascript" }),
    );
    try {
      await context.audioWorklet.addModule(url);
    } finally {
      URL.revokeObjectURL(url);
    }
    if (this.track.readyState !== "live") {
      await context.close();
      throw new Error("microphone track ended during initialization");
    }
    this.#source = context.createMediaStreamSource(
      new MediaStream([this.track]),
    );
    this.#node = new AudioWorkletNode(context, "blit-microphone", {
      numberOfInputs: 1,
      numberOfOutputs: 1,
      outputChannelCount: [1],
    });
    this.#sink = context.createGain();
    this.#sink.gain.value = 0;
    this.#node.port.onmessage = (event: MessageEvent<ArrayBuffer>) => {
      this.#push(new Float32Array(event.data), context.sampleRate);
    };
    this.#source
      .connect(this.#node)
      .connect(this.#sink)
      .connect(context.destination);
    this.track.addEventListener("ended", this.#ended, { once: true });
    await context.resume();
  }

  stop(stopTrack = true): void {
    if (this.#recordingClaim) {
      this.#recordingClaim = false;
      releaseRecordingAudioSession();
    }
    this.track.removeEventListener("ended", this.#ended);
    this.#node?.disconnect();
    this.#source?.disconnect();
    this.#sink?.disconnect();
    this.#node = null;
    this.#source = null;
    this.#sink = null;
    if (this.#context) void this.#context.close();
    this.#context = null;
    if (stopTrack) this.track.stop();
  }

  #push(input: Float32Array, sampleRate: number): void {
    const joined = new Float32Array(this.#pending.length + input.length);
    joined.set(this.#pending);
    joined.set(input, this.#pending.length);
    const step = sampleRate / 48_000;
    while (this.#cursor + 1 < joined.length) {
      const index = Math.floor(this.#cursor);
      const fraction = this.#cursor - index;
      this.#samples.push(
        joined[index]! + (joined[index + 1]! - joined[index]!) * fraction,
      );
      this.#cursor += step;
      if (this.#samples.length === 960) {
        const pcm = new Uint8Array(960 * 2);
        const view = new DataView(pcm.buffer);
        for (let i = 0; i < 960; i++) {
          const sample = Math.max(-1, Math.min(1, this.#samples[i]!));
          view.setInt16(
            i * 2,
            sample < 0
              ? Math.round(sample * 32768)
              : Math.round(sample * 32767),
            true,
          );
        }
        this.#samples.length = 0;
        const captureUs = Math.round(
          (this.#emittedSamples * 1_000_000) / 48_000,
        );
        this.#emittedSamples += 960;
        this.#frame(pcm, captureUs);
      }
    }
    const consumed = Math.floor(this.#cursor);
    this.#pending = joined.slice(consumed);
    this.#cursor -= consumed;
  }
}

type EncodedAudioChunkLike = {
  readonly byteLength: number;
  readonly timestamp: number;
  copyTo(destination: Uint8Array): void;
};
type AudioEncoderLike = {
  readonly encodeQueueSize: number;
  configure(config: object): void;
  encode(data: AudioDataLike): void;
  close(): void;
};
type AudioDataLike = { close(): void };
type AudioEncoderConstructor = {
  new (init: {
    output: (chunk: EncodedAudioChunkLike) => void;
    error: (error: DOMException) => void;
  }): AudioEncoderLike;
  isConfigSupported(config: object): Promise<{ supported?: boolean }>;
};
type AudioDataConstructor = new (init: object) => AudioDataLike;

function webCodecsAudio(): {
  Encoder: AudioEncoderConstructor;
  Data: AudioDataConstructor;
} | null {
  const globals = globalThis as typeof globalThis & {
    AudioEncoder?: AudioEncoderConstructor;
    AudioData?: AudioDataConstructor;
  };
  return globals.AudioEncoder && globals.AudioData
    ? { Encoder: globals.AudioEncoder, Data: globals.AudioData }
    : null;
}

export function supportsOpusMicrophone(): boolean {
  return webCodecsAudio() !== null;
}

let opusSupportProbe: Promise<boolean> | null = null;

function opusEncoderConfig(): object {
  return {
    codec: "opus",
    sampleRate: 48_000,
    numberOfChannels: 1,
    bitrate: 32_000,
    opus: { frameDuration: 20_000 },
  };
}

/** Performs the asynchronous WebCodecs codec check without opening a device. */
export function probeOpusMicrophone(): Promise<boolean> {
  if (opusSupportProbe) return opusSupportProbe;
  const codecs = webCodecsAudio();
  opusSupportProbe = codecs
    ? codecs.Encoder.isConfigSupported(opusEncoderConfig()).then(
        (support) => Boolean(support.supported),
        () => false,
      )
    : Promise.resolve(false);
  return opusSupportProbe;
}

class OpusMicrophoneEncoder {
  readonly #encoder: AudioEncoderLike;
  readonly #Data: AudioDataConstructor;
  readonly #output: (packet: Uint8Array, captureUs: number) => void;

  private constructor(
    encoder: AudioEncoderLike,
    Data: AudioDataConstructor,
    output: (packet: Uint8Array, captureUs: number) => void,
  ) {
    this.#encoder = encoder;
    this.#Data = Data;
    this.#output = output;
  }

  static async create(
    output: (packet: Uint8Array, captureUs: number) => void,
    failed: (error: Error) => void,
  ): Promise<OpusMicrophoneEncoder> {
    const codecs = webCodecsAudio();
    if (!codecs) throw new Error("WebCodecs audio encoding is unavailable");
    const config = opusEncoderConfig();
    const support = await codecs.Encoder.isConfigSupported(config);
    if (!support.supported) throw new Error("This browser cannot encode Opus");
    let instance: OpusMicrophoneEncoder | null = null;
    const encoder = new codecs.Encoder({
      output: (chunk) => {
        const packet = new Uint8Array(chunk.byteLength);
        chunk.copyTo(packet);
        if (instance) instance.#output(packet, chunk.timestamp);
      },
      error: (error) => failed(error),
    });
    encoder.configure(config);
    instance = new OpusMicrophoneEncoder(encoder, codecs.Data, output);
    return instance;
  }

  encode(pcm: Uint8Array, captureUs: number): void {
    if (this.#encoder.encodeQueueSize >= 3) return;
    const data = new this.#Data({
      format: "s16",
      sampleRate: 48_000,
      numberOfFrames: 960,
      numberOfChannels: 1,
      timestamp: captureUs,
      data: pcm,
    });
    try {
      this.#encoder.encode(data);
    } finally {
      data.close();
    }
  }

  stop(): void {
    try {
      this.#encoder.close();
    } catch {
      // WebCodecs may already have closed the encoder after its error callback.
    }
  }
}

type CameraWireCodec = 0 | 1 | 2 | 3 | 4;

const CAMERA_CODEC_AUTO_ORDER: readonly CameraWireCodec[] = [4, 2, 3, 1, 0];
const CAMERA_KEYFRAME_INTERVAL_US = 2_000_000;

function cameraCodecBit(codec: CameraWireCodec): number {
  return 1 << codec;
}

function cameraCodecLabel(codec: CameraWireCodec): string {
  switch (codec) {
    case 0:
      return "Motion JPEG";
    case 1:
      return "H.264 4:2:0";
    case 2:
      return "AV1 4:2:0";
    case 3:
      return "H.264 4:4:4";
    case 4:
      return "AV1 4:4:4";
  }
}

function cameraCodecCandidates(
  options: CameraOptions,
): readonly CameraWireCodec[] {
  if (
    options.codec !== undefined &&
    options.codec !== "mjpeg" &&
    options.codec !== "h264" &&
    options.codec !== "av1"
  ) {
    throw new Error("unknown camera codec");
  }
  if (
    options.chroma !== undefined &&
    options.chroma !== "420" &&
    options.chroma !== "444"
  ) {
    throw new Error("unknown camera chroma format");
  }
  if (options.codec === "mjpeg") {
    if (options.chroma !== undefined) {
      throw new Error("Motion JPEG does not expose an exact chroma selection");
    }
    return [0];
  }
  if (options.codec === "h264") return [options.chroma === "444" ? 3 : 1];
  if (options.codec === "av1") return [options.chroma === "444" ? 4 : 2];
  if (options.chroma === "444") return [4, 3];
  if (options.chroma === "420") return [2, 1];
  return CAMERA_CODEC_AUTO_ORDER;
}

function h264CameraLevel(width: number, height: number): string {
  return width <= 1280 && height <= 720 ? "1f" : "28";
}

/** Bits each codec spends per pixel per frame, before any quality scale. */
function cameraBitsPerPixel(codec: CameraWireCodec): number {
  switch (codec) {
    // Motion JPEG configures no bitrate: every picture is a whole intra
    // frame, and this is what one costs.
    case 0:
      return 1.2;
    case 1:
      return 0.11;
    case 2:
      return 0.075;
    case 3:
      return 0.16;
    case 4:
      return 0.11;
  }
}

/**
 * Bytes per second this camera configuration is expected to produce.
 *
 * The server sizes the lease window from the same arithmetic, so the two
 * agree on what a second of video costs — keep them in step.
 */
export function cameraBytesPerSecond(
  codec: CameraWireCodec,
  width: number,
  height: number,
  fps: number,
  scale = 1,
): number {
  const bits = width * height * fps * cameraBitsPerPixel(codec) * scale;
  return Math.max(0, bits / 8);
}

/**
 * Chooses how hard the camera encoder should push, from whether the link is
 * keeping up.
 *
 * Dropping frames keeps the picture current but spends the whole shortfall
 * on stutter; encoding smaller frames instead spends it on detail, which is
 * the better trade for a webcam. So congestion should lower the bitrate, not
 * just thin the stream.
 *
 * The two arms are deliberately asymmetric in speed but both present: back
 * off quickly, because the delay is already being felt, and recover slowly,
 * because probing upward costs another round of congestion when it is wrong.
 * An arm that can only ever degrade is the failure this is written against —
 * a link that recovers has to be able to earn its quality back, or one bad
 * minute quietly sets the quality for the rest of the session.
 */
export class CameraRateGovernor {
  static readonly MIN_SCALE = 0.25;
  static readonly MAX_SCALE = 1;
  static readonly BACKOFF = 0.75;
  static readonly RECOVER = 1.15;
  /** Consecutive clear intervals required before probing upward again. */
  static readonly RECOVER_AFTER = 5;

  #scale = 1;
  #clear = 0;

  get scale(): number {
    return this.#scale;
  }

  /** Fold in one observation interval; returns the scale to encode at. */
  observe(congested: boolean): number {
    if (congested) {
      this.#clear = 0;
      this.#scale = Math.max(
        CameraRateGovernor.MIN_SCALE,
        this.#scale * CameraRateGovernor.BACKOFF,
      );
      return this.#scale;
    }
    this.#clear += 1;
    if (this.#clear >= CameraRateGovernor.RECOVER_AFTER) {
      this.#clear = 0;
      this.#scale = Math.min(
        CameraRateGovernor.MAX_SCALE,
        this.#scale * CameraRateGovernor.RECOVER,
      );
    }
    return this.#scale;
  }

  reset(): void {
    this.#scale = 1;
    this.#clear = 0;
  }
}

function cameraEncoderConfig(
  codec: Exclude<CameraWireCodec, 0>,
  width: number,
  height: number,
  fps: number,
  /** Quality multiplier on the computed bitrate; 1 is the balanced default.
   *  The support probe leaves it at 1 — a codec is not supported or not
   *  supported at a different bitrate. */
  scale = 1,
): VideoEncoderConfig {
  const av1 = codec === 2 || codec === 4;
  const chroma444 = codec === 3 || codec === 4;
  const bitsPerPixel = av1
    ? chroma444
      ? 0.11
      : 0.075
    : chroma444
      ? 0.16
      : 0.11;
  const bitrate = Math.max(
    150_000,
    Math.min(
      8_000_000,
      Math.round(width * height * fps * bitsPerPixel * scale),
    ),
  );
  return {
    codec: av1
      ? `av01.${chroma444 ? 1 : 0}.${av1LevelString(width, height)}M.08`
      : `avc1.${chroma444 ? "F400" : "4200"}${h264CameraLevel(width, height)}`,
    width,
    height,
    displayWidth: width,
    displayHeight: height,
    framerate: fps,
    bitrate,
    bitrateMode: "variable",
    latencyMode: "realtime",
    hardwareAcceleration: "no-preference",
    ...(av1 ? {} : { avc: { format: "annexb" as const } }),
  };
}

type H264NalRange = {
  start: number;
  end: number;
  nal: number;
  kind: number;
};

function h264NalRanges(data: Uint8Array): H264NalRange[] {
  const ranges: H264NalRange[] = [];
  let offset = 0;
  while (offset + 3 < data.length) {
    let start = -1;
    let prefix = 0;
    for (let i = offset; i + 3 < data.length; i++) {
      if (data[i] !== 0 || data[i + 1] !== 0) continue;
      if (data[i + 2] === 1) {
        start = i;
        prefix = 3;
        break;
      }
      if (i + 3 < data.length && data[i + 2] === 0 && data[i + 3] === 1) {
        start = i;
        prefix = 4;
        break;
      }
    }
    if (start < 0) break;
    if (ranges.length) ranges[ranges.length - 1]!.end = start;
    const nal = start + prefix;
    if (nal >= data.length) break;
    ranges.push({ start, end: data.length, nal, kind: data[nal]! & 0x1f });
    offset = nal + 1;
  }
  return ranges;
}

function h264SpsFormat(
  data: Uint8Array,
): { profile: number; chromaFormat: number } | null {
  const ranges = h264NalRanges(data);
  if (
    !ranges.some((range) => range.kind === 8) ||
    !ranges.some((range) => range.kind === 5)
  ) {
    return null;
  }
  const sps = ranges.find((range) => range.kind === 7);
  if (!sps) return null;
  const escaped = data.subarray(sps.nal + 1, sps.end);
  const rbsp: number[] = [];
  let zeros = 0;
  for (const byte of escaped) {
    if (zeros >= 2 && byte === 3) {
      continue;
    }
    rbsp.push(byte);
    zeros = byte === 0 ? zeros + 1 : 0;
  }
  if (rbsp.length < 3) return null;
  const profile = rbsp[0]!;
  const highProfiles = new Set([
    100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135,
  ]);
  if (!highProfiles.has(profile)) return { profile, chromaFormat: 1 };
  let bit = 24;
  const readBit = (): number | null => {
    if (bit >= rbsp.length * 8) return null;
    const value = (rbsp[bit >>> 3]! >>> (7 - (bit & 7))) & 1;
    bit++;
    return value;
  };
  const readUe = (): number | null => {
    let zeros = 0;
    for (;;) {
      const value = readBit();
      if (value === null || zeros > 30) return null;
      if (value === 1) break;
      zeros++;
    }
    let suffix = 0;
    for (let index = 0; index < zeros; index++) {
      const value = readBit();
      if (value === null) return null;
      suffix = (suffix << 1) | value;
    }
    return 2 ** zeros - 1 + suffix;
  };
  if (readUe() === null) return null; // seq_parameter_set_id
  const chromaFormat = readUe();
  return chromaFormat !== null && chromaFormat <= 3
    ? { profile, chromaFormat }
    : null;
}

function h264ParameterSets(data: Uint8Array): Uint8Array | null {
  const ranges = h264NalRanges(data);
  const selected = ranges.filter(
    (range) => range.kind === 7 || range.kind === 8,
  );
  if (
    !selected.some((range) => range.kind === 7) ||
    !selected.some((range) => range.kind === 8)
  ) {
    return null;
  }
  const length = selected.reduce(
    (sum, range) => sum + range.end - range.start,
    0,
  );
  const out = new Uint8Array(length);
  let cursor = 0;
  for (const range of selected) {
    const nal = data.subarray(range.start, range.end);
    out.set(nal, cursor);
    cursor += nal.length;
  }
  return out;
}

type Av1SequenceHeader = {
  profile: number;
  obu: Uint8Array;
};

function av1SequenceHeader(data: Uint8Array): Av1SequenceHeader | null {
  let offset = 0;
  while (offset < data.length) {
    const start = offset;
    const header = data[offset++]!;
    if (header & 0x81) return null;
    const type = (header >>> 3) & 0x0f;
    if (header & 0x04) {
      if (offset >= data.length) return null;
      offset++;
    }
    let size = data.length - offset;
    if (header & 0x02) {
      size = 0;
      let shift = 0;
      for (;;) {
        if (offset >= data.length || shift > 28) return null;
        const byte = data[offset++]!;
        size |= (byte & 0x7f) << shift;
        if (!(byte & 0x80)) break;
        shift += 7;
      }
    }
    if (size <= 0 || offset + size > data.length) return null;
    const end = offset + size;
    if (type === 1) {
      return {
        profile: data[offset]! >>> 5,
        obu: data.slice(start, end),
      };
    }
    offset = end;
  }
  return null;
}

function av1SequenceProfile(data: Uint8Array): number | null {
  return av1SequenceHeader(data)?.profile ?? null;
}

function encodedCameraProfileMatches(
  codec: Exclude<CameraWireCodec, 0>,
  chunk: EncodedVideoChunk,
): boolean {
  if (chunk.type !== "key" || chunk.byteLength === 0) return false;
  const data = new Uint8Array(chunk.byteLength);
  chunk.copyTo(data);
  return cameraBitstreamMatchesCodec(codec, data);
}

/**
 * Whether a keyframe's bitstream carries what the wire codec promises.
 *
 * Split out from the probe so it can be tested without a `VideoEncoder`: the
 * rule it encodes is the whole reason a browser keeps or loses a codec.
 */
export function cameraBitstreamMatchesCodec(
  codec: Exclude<CameraWireCodec, 0>,
  data: Uint8Array,
): boolean {
  if (codec === 1 || codec === 3) {
    // Chroma, not profile. The wire codec distinguishes 4:2:0 from 4:4:4 and
    // nothing else — the server maps it to `(H264, Cs420)` and hands the
    // bitstream to a decoder that reads the profile out of the SPS like any
    // other. Requiring the exact profile we *asked* for rejects encoders that
    // honour the request with a superset: VideoToolbox answers a Baseline
    // request with Main or High, so Safari on macOS failed this probe, lost
    // H.264 and AV1, and fell back to Motion JPEG — a whole intra frame per
    // picture — for a stream it could have encoded properly all along.
    const format = h264SpsFormat(data);
    return format?.chromaFormat === (codec === 3 ? 3 : 1);
  }
  return av1SequenceProfile(data) === (codec === 4 ? 1 : 0);
}

function cameraProbeSource(): CanvasImageSource | null {
  try {
    if (typeof OffscreenCanvas !== "undefined") {
      return new OffscreenCanvas(320, 240) as unknown as CanvasImageSource;
    }
    if (typeof document !== "undefined") {
      const canvas = document.createElement("canvas");
      canvas.width = 320;
      canvas.height = 240;
      return canvas;
    }
  } catch {
    // A locked-down embedding can expose the constructors but deny canvases.
  }
  return null;
}

function supportsMjpegCamera(): boolean {
  return (
    typeof document !== "undefined" &&
    typeof document.createElement === "function" &&
    typeof HTMLCanvasElement !== "undefined" &&
    typeof HTMLCanvasElement.prototype.toBlob === "function"
  );
}

let cameraProbeEncoder: typeof VideoEncoder | undefined;
let cameraProbeFrame: typeof VideoFrame | undefined;
const cameraCodecProbes = new Map<string, Promise<boolean>>();

async function emitCameraProbeFrame(
  codec: Exclude<CameraWireCodec, 0>,
): Promise<boolean> {
  if (
    typeof VideoEncoder === "undefined" ||
    typeof VideoFrame === "undefined"
  ) {
    return false;
  }
  const source = cameraProbeSource();
  if (!source) return false;
  const config = cameraEncoderConfig(codec, 320, 240, 15);
  return new Promise<boolean>((resolve) => {
    let encoder: VideoEncoder | null = null;
    let settled = false;
    let valid = false;
    const finish = (result: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      try {
        encoder?.close();
      } catch {
        // An encoder error may have closed it already.
      }
      resolve(result);
    };
    const timer = setTimeout(() => finish(false), 1_500);
    try {
      encoder = new VideoEncoder({
        output: (chunk) => {
          valid ||= encodedCameraProfileMatches(codec, chunk);
        },
        error: () => finish(false),
      });
      encoder.configure(config);
      const frame = new VideoFrame(source, { timestamp: 0 });
      try {
        encoder.encode(frame, { keyFrame: true });
      } finally {
        frame.close();
      }
      void encoder.flush().then(
        () => finish(valid),
        () => finish(false),
      );
    } catch {
      finish(false);
    }
  });
}

function probeCameraCodec(
  codec: Exclude<CameraWireCodec, 0>,
  width: number,
  height: number,
  fps: number,
): Promise<boolean> {
  if (
    typeof VideoEncoder === "undefined" ||
    typeof VideoFrame === "undefined"
  ) {
    return Promise.resolve(false);
  }
  if (cameraProbeEncoder !== VideoEncoder || cameraProbeFrame !== VideoFrame) {
    cameraProbeEncoder = VideoEncoder;
    cameraProbeFrame = VideoFrame;
    cameraCodecProbes.clear();
  }
  const key = `${codec}:${width}x${height}@${fps}`;
  const cached = cameraCodecProbes.get(key);
  if (cached) return cached;
  const probe = VideoEncoder.isConfigSupported(
    cameraEncoderConfig(codec, width, height, fps),
  )
    .then(
      (support) => Boolean(support.supported) && emitCameraProbeFrame(codec),
    )
    .catch(() => false);
  cameraCodecProbes.set(key, probe);
  return probe;
}

/**
 * Probe exact camera encoder profiles. A bit is returned only after the
 * browser both accepts the requested config and emits the matching profile.
 */
export async function probeCameraCodecs(
  maxWidth = 1920,
  maxHeight = 1080,
  maxFps = 30,
): Promise<number> {
  const width = Math.max(1, Math.min(1920, Math.trunc(maxWidth)));
  const height = Math.max(1, Math.min(1080, Math.trunc(maxHeight)));
  const fps = Math.max(1, Math.min(30, Math.trunc(maxFps)));
  let mask = supportsMjpegCamera() ? VIDEO_CODEC_MJPEG : 0;
  const codecs = [1, 2, 3, 4] as const;
  // Probe sequentially: some hardware exposes fewer simultaneous encoder
  // sessions than formats, and a parallel capability test would create false
  // negatives by competing with itself.
  for (const codec of codecs) {
    if (await probeCameraCodec(codec, width, height, fps)) {
      mask |= cameraCodecBit(codec);
    }
  }
  return mask;
}

interface CameraCapture {
  readonly track: MediaStreamTrack;
  start(): Promise<void>;
  stop(stopTrack?: boolean): void;
  requestKeyframe(): void;
  /** Re-aim the encoder at `scale` times its configured bitrate. */
  setBitrateScale(scale: number): void;
}

class MjpegCameraCapture implements CameraCapture {
  readonly track: MediaStreamTrack;
  readonly #width: number;
  readonly #height: number;
  readonly #fps: number;
  readonly #frame: (jpeg: Uint8Array, captureUs: number) => void;
  readonly #ended: () => void;
  readonly #canEncode: () => boolean;
  readonly #baseQuality: number;
  #quality: number;
  #video: HTMLVideoElement | null = null;
  #canvas: HTMLCanvasElement | null = null;
  #timer: ReturnType<typeof setInterval> | null = null;
  #encoding = false;
  #startedAt = 0;

  constructor(
    track: MediaStreamTrack,
    width: number,
    height: number,
    fps: number,
    frame: (jpeg: Uint8Array, captureUs: number) => void,
    ended: () => void,
    canEncode: () => boolean,
    jpegQuality = 0.8,
  ) {
    this.track = track;
    this.#width = width;
    this.#height = height;
    this.#fps = fps;
    this.#frame = frame;
    this.#ended = ended;
    this.#canEncode = canEncode;
    this.#baseQuality = jpegQuality;
    this.#quality = jpegQuality;
  }

  async start(): Promise<void> {
    if (this.track.kind !== "video" || this.track.readyState !== "live") {
      throw new Error("camera track is not live");
    }
    if (typeof document === "undefined") {
      throw new Error("camera JPEG encoding requires a document canvas");
    }
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = new MediaStream([this.track]);
    const canvas = document.createElement("canvas");
    canvas.width = this.#width;
    canvas.height = this.#height;
    if (!canvas.getContext("2d", { alpha: false })) {
      throw new Error("2D canvas is unavailable");
    }
    this.#video = video;
    this.#canvas = canvas;
    this.track.addEventListener("ended", this.#ended, { once: true });
    await video.play();
    this.#startedAt = monotonicNow();
    this.#timer = setInterval(() => void this.#encode(), 1_000 / this.#fps);
  }

  stop(stopTrack = true): void {
    this.track.removeEventListener("ended", this.#ended);
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = null;
    if (this.#video) {
      this.#video.pause();
      this.#video.srcObject = null;
    }
    this.#video = null;
    this.#canvas = null;
    if (stopTrack) this.track.stop();
  }

  requestKeyframe(): void {
    // Every Motion JPEG image is independently decodable.
  }

  setBitrateScale(scale: number): void {
    // JPEG quality is the only dial here, and it moves with the governor so
    // a congested link sends smaller pictures rather than fewer.
    this.#quality = Math.min(0.95, Math.max(0.3, this.#baseQuality * scale));
  }

  async #encode(): Promise<void> {
    if (this.#encoding || !this.#video || !this.#canvas || !this.#canEncode()) {
      return;
    }
    this.#encoding = true;
    try {
      const context = this.#canvas.getContext("2d", { alpha: false });
      if (
        !context ||
        this.#video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA
      )
        return;
      context.drawImage(this.#video, 0, 0, this.#width, this.#height);
      const blob = await new Promise<Blob | null>((resolve) =>
        this.#canvas!.toBlob(resolve, "image/jpeg", this.#quality),
      );
      if (!blob || blob.size > 4 * 1024 * 1024) return;
      this.#frame(
        new Uint8Array(await blob.arrayBuffer()),
        Math.round((monotonicNow() - this.#startedAt) * 1_000),
      );
    } finally {
      this.#encoding = false;
    }
  }
}

class WebCodecsCameraCapture implements CameraCapture {
  readonly track: MediaStreamTrack;
  readonly #codec: Exclude<CameraWireCodec, 0>;
  readonly #width: number;
  readonly #height: number;
  readonly #fps: number;
  readonly #frame: (
    data: Uint8Array,
    captureUs: number,
    keyframe: boolean,
  ) => void;
  readonly #dropped: () => void;
  readonly #ended: () => void;
  readonly #canEncode: () => boolean;
  readonly #failed: (error: Error) => void;
  readonly #baseScale: number;
  #appliedScale: number;
  readonly #encoder: VideoEncoder;
  #video: HTMLVideoElement | null = null;
  /** Target-sized scratch the element is drawn into before encoding. */
  #canvas: HTMLCanvasElement | null = null;
  #timer: ReturnType<typeof setInterval> | null = null;
  #startedAt = 0;
  #lastKeyframeUs = -CAMERA_KEYFRAME_INTERVAL_US;
  #forceKeyframe = true;
  #stopped = false;
  #keyframeHeader: Uint8Array | null = null;

  constructor(
    track: MediaStreamTrack,
    codec: Exclude<CameraWireCodec, 0>,
    width: number,
    height: number,
    fps: number,
    frame: (data: Uint8Array, captureUs: number, keyframe: boolean) => void,
    dropped: () => void,
    ended: () => void,
    canEncode: () => boolean,
    failed: (error: Error) => void,
    bitrateScale = 1,
  ) {
    this.track = track;
    this.#codec = codec;
    this.#width = width;
    this.#height = height;
    this.#fps = fps;
    this.#frame = frame;
    this.#dropped = dropped;
    this.#ended = ended;
    this.#canEncode = canEncode;
    this.#failed = failed;
    this.#encoder = new VideoEncoder({
      output: (chunk) => this.#output(chunk),
      error: (error) => {
        if (!this.#stopped) this.#failed(error);
      },
    });
    this.#baseScale = bitrateScale;
    this.#appliedScale = bitrateScale;
    this.#encoder.configure(
      cameraEncoderConfig(codec, width, height, fps, bitrateScale),
    );
  }

  async start(): Promise<void> {
    if (this.track.kind !== "video" || this.track.readyState !== "live") {
      throw new Error("camera track is not live");
    }
    if (typeof document === "undefined") {
      throw new Error(
        "camera video encoding requires a document video element",
      );
    }
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = new MediaStream([this.track]);
    this.#video = video;
    this.track.addEventListener("ended", this.#ended, { once: true });
    await video.play();
    if (this.#stopped || this.track.readyState !== "live") {
      throw new Error("camera track ended during initialization");
    }
    const canvas = document.createElement("canvas");
    canvas.width = this.#width;
    canvas.height = this.#height;
    if (!canvas.getContext("2d", { alpha: false })) {
      throw new Error("2D canvas is unavailable");
    }
    this.#canvas = canvas;
    this.#startedAt = monotonicNow();
    this.#timer = setInterval(() => this.#encode(), 1_000 / this.#fps);
  }

  stop(stopTrack = true): void {
    if (this.#stopped) {
      if (stopTrack && this.track.readyState === "live") this.track.stop();
      return;
    }
    this.#stopped = true;
    this.track.removeEventListener("ended", this.#ended);
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = null;
    if (this.#video) {
      this.#video.pause();
      this.#video.srcObject = null;
    }
    this.#canvas = null;
    this.#video = null;
    try {
      this.#encoder.close();
    } catch {
      // WebCodecs may already have closed after its error callback.
    }
    if (stopTrack) this.track.stop();
  }

  requestKeyframe(): void {
    this.#forceKeyframe = true;
  }

  setBitrateScale(scale: number): void {
    const next = this.#baseScale * scale;
    // Reconfiguring costs the encoder its reference state, so ignore the
    // noise and act on real moves only.
    if (Math.abs(next - this.#appliedScale) < this.#appliedScale * 0.1) return;
    this.#appliedScale = next;
    try {
      this.#encoder.configure(
        cameraEncoderConfig(
          this.#codec,
          this.#width,
          this.#height,
          this.#fps,
          next,
        ),
      );
      // A reconfigured encoder starts a new sequence; the decoder on the far
      // side needs a keyframe to follow it.
      this.#forceKeyframe = true;
    } catch {
      // An encoder that refuses the new bitrate keeps the old one, which is
      // survivable — the frame drop path still bounds the delay.
    }
  }

  #encode(): void {
    if (
      this.#stopped ||
      !this.#video ||
      !this.#canEncode() ||
      this.#video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA
    ) {
      return;
    }
    if (this.#encoder.encodeQueueSize >= 2) {
      this.#dropped();
      return;
    }
    const captureUs = Math.max(
      0,
      Math.round((monotonicNow() - this.#startedAt) * 1_000),
    );
    const keyframe =
      this.#forceKeyframe ||
      captureUs - this.#lastKeyframeUs >= CAMERA_KEYFRAME_INTERVAL_US;
    let frame: VideoFrame | null = null;
    try {
      // Via a canvas, not straight off the element.
      //
      // `new VideoFrame(video)` takes the frame as decoded and ignores the
      // rotation the element applies when it paints, so a tablet whose camera
      // is mounted against the way it is held encodes upside down while its
      // own preview — and Motion JPEG, which has always gone through
      // `drawImage` — look right. Drawing first puts both codecs on the one
      // path that honours it, and costs a copy the JPEG path already paid.
      const context = this.#canvas?.getContext("2d", { alpha: false });
      if (!context || !this.#canvas) return;
      context.drawImage(this.#video, 0, 0, this.#width, this.#height);
      frame = new VideoFrame(this.#canvas, { timestamp: captureUs });
      this.#encoder.encode(frame, { keyFrame: keyframe });
      if (keyframe) {
        this.#forceKeyframe = false;
        this.#lastKeyframeUs = captureUs;
      }
    } catch (error) {
      this.#failed(error instanceof Error ? error : new Error(String(error)));
    } finally {
      frame?.close();
    }
  }

  #output(chunk: EncodedVideoChunk): void {
    if (this.#stopped) return;
    if (chunk.byteLength === 0 || chunk.byteLength > CAMERA_FRAME_MAX) {
      this.#forceKeyframe = true;
      this.#dropped();
      return;
    }
    let data = new Uint8Array(chunk.byteLength);
    chunk.copyTo(data);
    if (chunk.type === "key") {
      let header: Uint8Array | null;
      let formatMatches: boolean;
      if (this.#codec === 1 || this.#codec === 3) {
        header = h264ParameterSets(data);
        const format = h264SpsFormat(data);
        formatMatches =
          format?.profile === (this.#codec === 3 ? 0xf4 : 0x42) &&
          format.chromaFormat === (this.#codec === 3 ? 3 : 1);
      } else {
        const sequence = av1SequenceHeader(data);
        header = sequence?.obu ?? null;
        formatMatches = sequence?.profile === (this.#codec === 4 ? 1 : 0);
      }
      if (header) {
        if (!formatMatches) {
          this.#failed(
            new Error(
              `${cameraCodecLabel(this.#codec)} encoder emitted the wrong profile`,
            ),
          );
          return;
        }
        this.#keyframeHeader = header;
      } else if (this.#keyframeHeader) {
        const selfContained = new Uint8Array(
          this.#keyframeHeader.length + data.length,
        );
        selfContained.set(this.#keyframeHeader);
        selfContained.set(data, this.#keyframeHeader.length);
        data = selfContained;
      } else {
        this.#forceKeyframe = true;
        this.#dropped();
        return;
      }
    }
    this.#frame(data, chunk.timestamp, chunk.type === "key");
  }
}

export class MediaStore implements ReactiveStore {
  readonly #notifier = new Notifier();
  readonly #requests = new Map<number, PortalRequest>();
  readonly #requestListeners = new Set<(request: PortalRequest) => void>();
  #sender: ((message: Uint8Array) => void) | null = null;
  #backpressure: (() => number | undefined) | null = null;
  /** The lease's whole in-flight window; 0 until one is granted. */
  #cameraCreditWindow = 0;
  readonly #cameraGovernor = new CameraRateGovernor();
  #cameraGovernorTimer: ReturnType<typeof setInterval> | null = null;
  /** Whether the link showed backpressure since the last governor tick. */
  #cameraCongestedSinceTick = false;
  #state: DesktopMediaState = emptyState();
  #serverVideoCodecs = VIDEO_CODECS_LEGACY;
  #serverVideoCodecsAnnounced = false;
  #requestedCapabilities: MediaCapabilities | null = null;
  #capabilitiesSentAt: number | null = null;
  #capabilitiesTimer: ReturnType<typeof setTimeout> | null = null;
  readonly #capabilitiesWaiters = new Set<() => void>();
  #microphone: MediaLeaseState = emptyLease("microphone");
  #camera: MediaLeaseState = emptyLease("camera");
  #microphoneCapture: PcmMicrophoneCapture | null = null;
  #microphoneEncoder: OpusMicrophoneEncoder | null = null;
  #cameraCapture: CameraCapture | null = null;
  #cameraStartingTrack: MediaStreamTrack | null = null;
  #pendingStarts = new Map<
    number,
    {
      kind: "microphone" | "camera";
      codec: number;
      width: number;
      height: number;
      fps: number;
      resolve: () => void;
      reject: (error: Error) => void;
      timer: ReturnType<typeof setTimeout>;
    }
  >();
  #nextNonce = 0;
  #microphoneSequence = 0;
  #cameraSequence = 0;
  #microphoneDiscontinuity = false;
  #cameraDiscontinuity = false;
  #cameraNeedsKeyframe = false;
  #cameraRequiredCredit = 1;

  get revision(): number {
    return this.#notifier.revision;
  }

  get state(): DesktopMediaState {
    return this.#state;
  }
  /**
   * Camera formats understood by the peer. Old servers never announce this,
   * so the safe initial value contains only the two legacy registry entries.
   */
  get serverVideoCodecs(): number {
    return this.#serverVideoCodecs;
  }
  get microphone(): MediaLeaseState {
    return this.#microphone;
  }
  get camera(): MediaLeaseState {
    return this.#camera;
  }
  get cameraTrack(): MediaStreamTrack | null {
    return this.#cameraCapture?.track ?? this.#cameraStartingTrack;
  }
  get requests(): ReadonlyMap<number, PortalRequest> {
    return this.#requests;
  }
  subscribe(listener: () => void): () => void {
    return this.#notifier.subscribe(listener);
  }
  setSender(sender: ((message: Uint8Array) => void) | null): void {
    this.#sender = sender;
    if (!sender && this.#capabilitiesTimer) {
      clearTimeout(this.#capabilitiesTimer);
      this.#capabilitiesTimer = null;
      this.#resolveCapabilityWaiters();
    }
    if (!sender) this.#capabilitiesSentAt = null;
    if (sender) this.#scheduleCapabilities();
  }
  /**
   * Whether the link is already carrying more camera video than it should.
   *
   * The allowance is a time, converted through the stream's own bitrate: a
   * byte ceiling would mean a different delay on every link, which is the
   * mistake the old flat credit window made. Anything already queued is in
   * front of the frame about to be captured, so past the allowance the right
   * move is to drop at the source rather than lengthen the queue — a viewer
   * would rather lose a frame than watch a stale one.
   *
   * A transport that cannot report its queue answers `undefined`; that is
   * "unknown", not "congested", and lease credit remains the backstop.
   */
  #linkCongested(): boolean {
    const queued = this.#backpressure?.();
    if (queued === undefined || !Number.isFinite(queued)) return false;
    // Remember it for the governor: congestion between two ticks is still
    // congestion, and a link that stalls briefly every second would
    // otherwise read as clear at every sample.
    const congested = this.#queueOverAllowance(queued);
    if (congested) this.#cameraCongestedSinceTick = true;
    return congested;
  }
  #queueOverAllowance(queued: number): boolean {
    const lease = this.#camera;
    const perSecond = cameraBytesPerSecond(
      lease.codec as CameraWireCodec,
      lease.width,
      lease.height,
      lease.fps,
    );
    // An unnegotiated lease has no cadence to scale by; a small absolute
    // allowance still beats letting the queue run away.
    const allowance =
      perSecond > 0
        ? Math.max(
            CAMERA_QUEUE_MIN_BYTES,
            (perSecond * CAMERA_QUEUE_TARGET_MS) / 1000,
          )
        : CAMERA_QUEUE_MIN_BYTES;
    return queued > allowance;
  }
  /**
   * How to ask the transport what it still owes the network.
   *
   * Lease credit alone cannot keep the camera current: it is returned only
   * once the server has *decoded* a frame, so a whole window's worth can be
   * sitting in this socket before any of it is acknowledged, and every byte
   * of it is delay in front of the picture. The queue length is the one
   * number that says so while it is happening.
   */
  setBackpressureProbe(probe: (() => number | undefined) | null): void {
    this.#backpressure = probe;
  }
  advertise(capabilities: MediaCapabilities): void {
    this.#requestedCapabilities = { ...capabilities };
    this.#scheduleCapabilities();
  }
  setCapabilities(capabilities: MediaCapabilities): void {
    this.advertise(capabilities);
  }
  async startMicrophone(
    track: MediaStreamTrack,
    options: MicrophoneOptions = {},
  ): Promise<void> {
    if (this.#microphone.status !== "inactive") {
      track.stop();
      throw new Error("microphone capture is already starting or active");
    }
    if (!this.#sender) {
      track.stop();
      throw new Error("connection unavailable");
    }
    let codec = options.codec ?? (supportsOpusMicrophone() ? "opus" : "pcm");
    const capture = new PcmMicrophoneCapture(
      track,
      (pcm, captureUs) => {
        if (this.#microphoneEncoder) {
          this.#microphoneEncoder.encode(pcm, captureUs);
        } else {
          this.#sendMicrophoneFrame(pcm, captureUs);
        }
      },
      () => this.#microphoneEnded(),
    );
    this.#microphoneCapture = capture;
    this.#microphone = { ...emptyLease("microphone"), status: "starting" };
    this.#notifier.emit();
    try {
      await capture.start();
      if (codec === "opus") {
        try {
          this.#microphoneEncoder = await OpusMicrophoneEncoder.create(
            (packet, captureUs) => this.#sendMicrophoneFrame(packet, captureUs),
            (encoderError) => this.#microphoneEncoderFailed(encoderError),
          );
        } catch (error) {
          if (options.codec === "opus") throw error;
          // The synchronous API check only establishes that WebCodecs exists.
          // A browser may still reject this Opus configuration; omitted codec
          // means compatibility fallback, while explicit Opus stays strict.
          codec = "pcm";
          this.#microphoneEncoder = null;
        }
      }
      if (
        this.#microphoneCapture !== capture ||
        this.#microphone.status !== "starting" ||
        track.readyState !== "live"
      ) {
        throw new Error(
          this.#microphone.error ??
            "microphone capture ended during initialization",
        );
      }
    } catch (error) {
      this.#stopLocalMicrophone(
        error instanceof Error ? error.message : String(error),
      );
      throw error;
    }
    const nonce = this.#allocateNonce();
    return new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        const pending = this.#pendingStarts.get(nonce);
        if (!pending) return;
        this.#pendingStarts.delete(nonce);
        pending.reject(new Error("microphone lease timed out"));
        this.#stopLocalMicrophone();
      }, 10_000);
      this.#pendingStarts.set(nonce, {
        kind: "microphone",
        codec: codec === "opus" ? 1 : 0,
        width: 0,
        height: 0,
        fps: 0,
        resolve,
        reject,
        timer,
      });
      this.#sender!(
        buildMediaStartMessage(nonce, "microphone", codec === "opus" ? 1 : 0),
      );
    });
  }
  async startCamera(
    track: MediaStreamTrack,
    options: CameraOptions = {},
  ): Promise<void> {
    if (this.#camera.status !== "inactive") {
      track.stop();
      throw new Error("camera capture is already starting or active");
    }
    if (!this.#sender) {
      track.stop();
      throw new Error("connection unavailable");
    }
    let settings: MediaTrackSettings;
    try {
      settings = track.getSettings();
    } catch (error) {
      track.stop();
      throw error;
    }
    const width = clampInt(options.width ?? settings.width ?? 1280, 1920);
    const height = clampInt(options.height ?? settings.height ?? 720, 1080);
    const requestedFps = Math.max(
      1,
      clampInt(options.fps ?? settings.frameRate ?? 30, 30),
    );
    if (!width || !height || !Number.isInteger(requestedFps)) {
      track.stop();
      throw new Error("camera dimensions or frame rate are unavailable");
    }
    const explicitCodec = options.codec !== undefined;
    const constrainedFormat = explicitCodec || options.chroma !== undefined;
    let candidates: readonly CameraWireCodec[];
    try {
      const usableServerCodecs =
        constrainedFormat || this.#serverVideoCodecsAnnounced
          ? this.#serverVideoCodecs
          : VIDEO_CODEC_MJPEG;
      candidates = cameraCodecCandidates(options).filter(
        (codec) => usableServerCodecs & cameraCodecBit(codec),
      );
    } catch (error) {
      track.stop();
      throw error;
    }
    if (!candidates.length) {
      track.stop();
      throw new Error(
        "the server did not advertise the requested camera format",
      );
    }

    this.#cameraStartingTrack = track;
    this.#camera = {
      ...emptyLease("camera"),
      status: "starting",
      width,
      height,
      fps: requestedFps,
    };
    this.#notifier.emit();

    let selected: CameraWireCodec | null = null;
    let selectedFps = requestedFps;
    let lastError: Error | null = null;
    try {
      await this.#waitForScheduledCapabilities();
      if (!this.#sender) throw new Error("connection unavailable");
      for (const codec of candidates) {
        if (this.#camera.status !== "starting" || track.readyState !== "live") {
          throw new Error("camera capture was cancelled during initialization");
        }
        const fps =
          codec === 0 && options.fps === undefined
            ? Math.min(15, requestedFps)
            : requestedFps;
        let capture: CameraCapture | null = null;
        try {
          if (codec === 0) {
            if (!supportsMjpegCamera()) {
              throw new Error("Motion JPEG camera encoding is unavailable");
            }
            capture = new MjpegCameraCapture(
              track,
              width,
              height,
              fps,
              (jpeg, captureUs) => this.#sendCameraFrame(jpeg, captureUs, true),
              () => this.#cameraEnded(),
              () =>
                this.#camera.status === "active" &&
                this.#camera.credit >= this.#cameraRequiredCredit &&
                !this.#linkCongested(),
              cameraQuality(options.quality).jpeg,
            );
          } else {
            if (!(await probeCameraCodec(codec, width, height, fps))) {
              throw new Error(
                `${cameraCodecLabel(codec)} encoding is unavailable`,
              );
            }
            capture = new WebCodecsCameraCapture(
              track,
              codec,
              width,
              height,
              fps,
              (data, captureUs, keyframe) =>
                this.#sendCameraFrame(data, captureUs, keyframe),
              () => this.#cameraFrameDropped(),
              () => this.#cameraEnded(),
              () =>
                this.#camera.status === "active" &&
                this.#camera.credit >= this.#cameraRequiredCredit &&
                !this.#linkCongested(),
              (error) => this.#cameraEncoderFailed(error),
              cameraQuality(options.quality).scale,
            );
          }
          this.#cameraCapture = capture;
          await capture.start();
          if (
            this.#cameraCapture !== capture ||
            this.#camera.status !== "starting" ||
            track.readyState !== "live"
          ) {
            throw new Error(
              "camera capture was cancelled during initialization",
            );
          }
          selected = codec;
          selectedFps = fps;
          break;
        } catch (error) {
          capture?.stop(false);
          if (this.#cameraCapture === capture) this.#cameraCapture = null;
          lastError = error instanceof Error ? error : new Error(String(error));
          if (this.#camera.status !== "starting" || explicitCodec) {
            throw lastError;
          }
        }
      }
      if (selected === null) {
        throw (
          lastError ?? new Error("no supported camera encoder is available")
        );
      }
      if (!this.#sender) throw new Error("connection unavailable");
      this.#camera = { ...this.#camera, codec: selected, fps: selectedFps };
    } catch (error) {
      if (this.#camera.status !== "inactive") {
        this.#stopLocalCamera(
          error instanceof Error ? error.message : String(error),
        );
      }
      throw error;
    }
    const codec = selected;
    const fps = selectedFps;
    const nonce = this.#allocateNonce();
    return new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => {
        const pending = this.#pendingStarts.get(nonce);
        if (!pending) return;
        this.#pendingStarts.delete(nonce);
        pending.reject(new Error("camera lease timed out"));
        this.#stopLocalCamera();
      }, 10_000);
      this.#pendingStarts.set(nonce, {
        kind: "camera",
        codec,
        width,
        height,
        fps,
        resolve,
        reject,
        timer,
      });
      this.#sender!(
        buildMediaStartMessage(nonce, "camera", codec, width, height, fps),
      );
    });
  }
  stop(kind: "microphone" | "camera"): void {
    for (const [nonce, pending] of this.#pendingStarts) {
      if (pending.kind !== kind) continue;
      this.#pendingStarts.delete(nonce);
      clearTimeout(pending.timer);
      pending.reject(new Error(`${kind} lease cancelled`));
    }
    const lease = kind === "microphone" ? this.#microphone : this.#camera;
    if (lease.leaseId) this.#sender?.(buildMediaStopMessage(lease.leaseId));
    if (kind === "microphone") this.#stopLocalMicrophone();
    else this.#stopLocalCamera();
  }
  onPortalRequest(listener: (request: PortalRequest) => void): () => void {
    this.#requestListeners.add(listener);
    return () => this.#requestListeners.delete(listener);
  }
  reply(
    requestId: number,
    decision: "deny" | "grant" | "cancelled",
    surfaceIds: readonly number[] = [],
    choices: readonly PortalChoiceValue[] = [],
  ): void {
    const request = this.#requests.get(requestId);
    if (!request) return;
    this.#sender?.(
      buildPortalReplyMessage(request, decision, surfaceIds, choices),
    );
    this.#requests.delete(requestId);
    this.#notifier.emit();
  }
  stopScreenCast(sessionId: number): void {
    this.#sender?.(buildScreenCastStopMessage(sessionId));
  }
  handle(control: ParsedControl): boolean {
    if (control.kind === "serverCapabilities") {
      if (
        !this.#serverVideoCodecsAnnounced ||
        this.#serverVideoCodecs !== control.videoCodecs
      ) {
        this.#serverVideoCodecsAnnounced = true;
        this.#serverVideoCodecs = control.videoCodecs;
        this.#scheduleCapabilities();
        this.#notifier.emit();
      }
      return true;
    }
    if (control.kind === "state") {
      this.#state = control.state;
      this.#notifier.emit();
      return true;
    }
    if (control.kind === "portalRequest") {
      this.#requests.set(control.request.requestId, control.request);
      this.#notifier.emit();
      for (const listener of [...this.#requestListeners]) {
        listener(control.request);
      }
      return true;
    }
    if (control.kind === "portalCancel") {
      if (this.#requests.delete(control.requestId)) this.#notifier.emit();
      return true;
    }
    if (control.kind === "lease") {
      const pending = this.#pendingStarts.get(control.nonce);
      if (!pending) {
        // A cancelled/timed-out request may still receive a successful reply.
        // Never leave that now-unowned server lease live.
        if (control.status === STATUS_OK && control.leaseId) {
          this.#sender?.(buildMediaStopMessage(control.leaseId));
        }
        return true;
      }
      this.#pendingStarts.delete(control.nonce);
      clearTimeout(pending.timer);
      if (control.status !== STATUS_OK || !control.leaseId) {
        const error = new Error(`media lease failed (${control.status})`);
        pending.reject(error);
        if (pending.kind === "microphone") {
          this.#stopLocalMicrophone(error.message);
        } else this.#stopLocalCamera(error.message);
        return true;
      }
      if (
        control.codec !== pending.codec ||
        control.width !== pending.width ||
        control.height !== pending.height ||
        control.fps !== pending.fps
      ) {
        this.#sender?.(buildMediaStopMessage(control.leaseId));
        const message = "server returned a different media format";
        pending.reject(new Error(message));
        if (pending.kind === "microphone") this.#stopLocalMicrophone(message);
        else this.#stopLocalCamera(message);
        return true;
      }
      if (pending.kind === "microphone" && control.mediaKind === 0) {
        this.#microphone = {
          kind: "microphone",
          status: "active",
          leaseId: control.leaseId,
          codec: control.codec,
          width: control.width,
          height: control.height,
          fps: control.fps,
          credit: control.initialCredit,
          error: null,
        };
        this.#microphoneSequence = 0;
        this.#microphoneDiscontinuity = false;
        pending.resolve();
        this.#notifier.emit();
      } else if (pending.kind === "camera" && control.mediaKind === 1) {
        this.#camera = {
          kind: "camera",
          status: "active",
          leaseId: control.leaseId,
          codec: control.codec,
          width: control.width,
          height: control.height,
          fps: control.fps,
          credit: control.initialCredit,
          error: null,
        };
        this.#cameraSequence = 0;
        this.#cameraDiscontinuity = false;
        this.#cameraNeedsKeyframe = control.codec !== 0;
        this.#cameraRequiredCredit = 1;
        this.#cameraCreditWindow = control.initialCredit;
        // The lease is open and the capture is running: start watching the
        // link. Not before — there is nothing to govern until frames flow.
        this.#startCameraGovernor();
        pending.resolve();
        this.#notifier.emit();
      } else {
        this.#sender?.(buildMediaStopMessage(control.leaseId));
        pending.reject(new Error("server returned the wrong media kind"));
        if (pending.kind === "microphone") {
          this.#stopLocalMicrophone("server returned the wrong media kind");
        } else {
          this.#stopLocalCamera("server returned the wrong media kind");
        }
      }
      return true;
    }
    if (control.kind === "credit") {
      if (control.leaseId === this.#microphone.leaseId) {
        this.#microphone = {
          ...this.#microphone,
          credit: Math.min(0xffffffff, this.#microphone.credit + control.bytes),
        };
      }
      if (control.leaseId === this.#camera.leaseId) {
        this.#camera = {
          ...this.#camera,
          credit: Math.min(0xffffffff, this.#camera.credit + control.bytes),
        };
        if (control.flags & MEDIA_CREDIT_KEYFRAME) {
          this.#cameraNeedsKeyframe = this.#camera.codec !== 0;
          this.#cameraCapture?.requestKeyframe();
        }
      }
      return true;
    }
    if (control.kind === "revoked") {
      if (control.leaseId === this.#microphone.leaseId) {
        this.#stopLocalMicrophone(
          `microphone lease revoked (${control.reason})`,
        );
      }
      if (control.leaseId === this.#camera.leaseId) {
        this.#stopLocalCamera(`camera lease revoked (${control.reason})`);
      }
      return true;
    }
    return false;
  }
  reset(error = new Error("media state reset")): void {
    const changed =
      this.#state.runtimeFlags !== 0 ||
      this.#state.activeFlags !== 0 ||
      this.#requests.size > 0 ||
      this.#serverVideoCodecsAnnounced ||
      this.#serverVideoCodecs !== VIDEO_CODECS_LEGACY;
    this.#state = emptyState();
    this.#serverVideoCodecs = VIDEO_CODECS_LEGACY;
    this.#serverVideoCodecsAnnounced = false;
    this.#requestedCapabilities = null;
    this.#capabilitiesSentAt = null;
    if (this.#capabilitiesTimer) clearTimeout(this.#capabilitiesTimer);
    this.#capabilitiesTimer = null;
    this.#resolveCapabilityWaiters();
    this.#requests.clear();
    for (const pending of this.#pendingStarts.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pendingStarts.clear();
    this.#stopLocalMicrophone();
    this.#stopLocalCamera();
    if (changed) this.#notifier.emit();
  }

  #sendMicrophoneFrame(pcm: Uint8Array, captureUs: number): void {
    const lease = this.#microphone;
    if (lease.status !== "active" || !lease.leaseId || !this.#sender) return;
    if (lease.credit < pcm.length) {
      this.#microphoneDiscontinuity = true;
      return;
    }
    this.#microphoneSequence = (this.#microphoneSequence + 1) >>> 0;
    this.#microphone = { ...lease, credit: lease.credit - pcm.length };
    this.#sender(
      buildMediaDataMessage({
        leaseId: lease.leaseId,
        sequence: this.#microphoneSequence,
        captureUs,
        kind: "microphone",
        codec: lease.codec,
        flags: this.#microphoneDiscontinuity ? 2 : 0,
        fragmentIndex: 0,
        fragmentCount: 1,
        frameLength: pcm.length,
        data: pcm,
      }),
    );
    this.#microphoneDiscontinuity = false;
  }
  #microphoneEnded(): void {
    const lease = this.#microphone;
    if (lease.status === "active" && lease.leaseId && this.#sender) {
      this.#microphoneSequence = (this.#microphoneSequence + 1) >>> 0;
      this.#sender(
        buildMediaDataMessage({
          leaseId: lease.leaseId,
          sequence: this.#microphoneSequence,
          captureUs: 0,
          kind: "microphone",
          codec: lease.codec,
          flags: 4,
          fragmentIndex: 0,
          fragmentCount: 1,
          frameLength: 0,
          data: new Uint8Array(),
        }),
      );
    }
    this.#stopLocalMicrophone("microphone device ended", false);
  }
  #microphoneEncoderFailed(error: Error): void {
    // Encoder failure is not a device-ended event: the physical track is still
    // live and must be stopped explicitly so the browser privacy indicator and
    // hardware capture both end. stop() also cancels a lease still in flight.
    this.stop("microphone");
    this.#microphone = {
      ...this.#microphone,
      error: error.message,
    };
    this.#notifier.emit();
  }
  #cameraEncoderFailed(error: Error): void {
    this.stop("camera");
    this.#camera = { ...this.#camera, error: error.message };
    this.#notifier.emit();
  }
  #cameraFrameDropped(): void {
    this.#cameraDiscontinuity = true;
    if (this.#camera.codec !== 0) {
      this.#cameraNeedsKeyframe = true;
      this.#cameraCapture?.requestKeyframe();
    }
  }
  /**
   * A frame the lease window could never carry.
   *
   * Distinct from ordinary congestion: this one cannot be waited out, so it
   * counts as congestion for the governor immediately rather than at the
   * next tick, and the encoder is aimed lower on the spot.
   */
  #cameraOverBudget(): void {
    this.#cameraCongestedSinceTick = true;
    this.#cameraCapture?.setBitrateScale(this.#cameraGovernor.observe(true));
  }
  /**
   * Watch the link once a second and re-aim the encoder.
   *
   * A tick, not a per-frame reaction: bitrate changes cost the encoder its
   * reference state, and reacting to single frames would chase noise.
   */
  #startCameraGovernor(): void {
    this.#stopCameraGovernor();
    this.#cameraGovernor.reset();
    this.#cameraCongestedSinceTick = false;
    this.#cameraGovernorTimer = setInterval(() => {
      const congested = this.#cameraCongestedSinceTick || this.#linkCongested();
      this.#cameraCongestedSinceTick = false;
      const scale = this.#cameraGovernor.observe(congested);
      this.#cameraCapture?.setBitrateScale(scale);
    }, CAMERA_GOVERNOR_INTERVAL_MS);
  }
  #stopCameraGovernor(): void {
    if (this.#cameraGovernorTimer) clearInterval(this.#cameraGovernorTimer);
    this.#cameraGovernorTimer = null;
  }
  #sendCameraFrame(
    data: Uint8Array,
    captureUs: number,
    keyframe: boolean,
  ): void {
    const lease = this.#camera;
    if (lease.status !== "active" || !lease.leaseId || !this.#sender) return;
    if (this.#cameraNeedsKeyframe && !keyframe) return;
    if (!data.length || data.length > CAMERA_FRAME_MAX) {
      this.#cameraFrameDropped();
      return;
    }
    if (lease.credit < data.length) {
      // Credit is conserved: it is returned as frames are consumed and never
      // grows past the window the lease opened with. So a frame larger than
      // the whole window can never be sent, and waiting for room to appear
      // is waiting forever — the next frame owed is a keyframe, and a
      // keyframe is precisely what does not fit. Ask for the window instead
      // and drop this one; the capture answers a drop with a fresh keyframe,
      // and the bitrate governor below has already been told to aim lower.
      this.#cameraRequiredCredit =
        this.#cameraCreditWindow > 0
          ? Math.min(data.length, this.#cameraCreditWindow)
          : data.length;
      if (data.length > this.#cameraCreditWindow) this.#cameraOverBudget();
      this.#cameraFrameDropped();
      return;
    }
    const fragmentSize = MEDIA_FRAGMENT_MAX;
    const count = Math.ceil(data.length / fragmentSize);
    if (!count || count > MEDIA_FRAGMENT_COUNT_MAX) {
      this.#cameraFrameDropped();
      return;
    }
    this.#cameraSequence = (this.#cameraSequence + 1) >>> 0;
    this.#camera = { ...lease, credit: lease.credit - data.length };
    for (let index = 0; index < count; index++) {
      this.#sender(
        buildMediaDataMessage({
          leaseId: lease.leaseId,
          sequence: this.#cameraSequence,
          captureUs,
          kind: "camera",
          codec: lease.codec,
          flags: (keyframe ? 1 : 0) | (this.#cameraDiscontinuity ? 2 : 0),
          fragmentIndex: index,
          fragmentCount: count,
          frameLength: data.length,
          data: data.subarray(
            index * fragmentSize,
            Math.min(data.length, (index + 1) * fragmentSize),
          ),
        }),
      );
    }
    if (keyframe) this.#cameraNeedsKeyframe = false;
    this.#cameraRequiredCredit = 1;
    this.#cameraDiscontinuity = false;
  }
  #cameraEnded(): void {
    const lease = this.#camera;
    if (lease.status === "active" && lease.leaseId && this.#sender) {
      this.#cameraSequence = (this.#cameraSequence + 1) >>> 0;
      this.#sender(
        buildMediaDataMessage({
          leaseId: lease.leaseId,
          sequence: this.#cameraSequence,
          captureUs: 0,
          kind: "camera",
          codec: lease.codec,
          flags: 4,
          fragmentIndex: 0,
          fragmentCount: 1,
          frameLength: 0,
          data: new Uint8Array(),
        }),
      );
    }
    this.#stopLocalCamera("camera device ended", false);
  }
  #stopLocalMicrophone(error: string | null = null, stopTrack = true): void {
    this.#microphoneEncoder?.stop();
    this.#microphoneEncoder = null;
    this.#microphoneCapture?.stop(stopTrack);
    this.#microphoneCapture = null;
    const changed = this.#microphone.status !== "inactive";
    this.#microphone = { ...emptyLease("microphone"), error };
    if (changed || error) this.#notifier.emit();
  }
  #stopLocalCamera(error: string | null = null, stopTrack = true): void {
    if (this.#cameraCapture) {
      this.#cameraCapture.stop(stopTrack);
    } else if (stopTrack && this.#cameraStartingTrack?.readyState === "live") {
      this.#cameraStartingTrack.stop();
    }
    this.#stopCameraGovernor();
    this.#cameraCapture = null;
    this.#cameraStartingTrack = null;
    this.#cameraNeedsKeyframe = false;
    this.#cameraDiscontinuity = false;
    this.#cameraRequiredCredit = 1;
    this.#cameraCreditWindow = 0;
    const changed = this.#camera.status !== "inactive";
    this.#camera = { ...emptyLease("camera"), error };
    if (changed || error) this.#notifier.emit();
  }
  #allocateNonce(): number {
    do this.#nextNonce = (this.#nextNonce + 1) >>> 0 || 1;
    while (this.#pendingStarts.has(this.#nextNonce));
    return this.#nextNonce;
  }
  #scheduleCapabilities(): void {
    if (!this.#sender || !this.#requestedCapabilities) return;
    if (this.#capabilitiesTimer) {
      clearTimeout(this.#capabilitiesTimer);
      this.#capabilitiesTimer = null;
    }
    const elapsed =
      this.#capabilitiesSentAt === null
        ? Number.POSITIVE_INFINITY
        : monotonicNow() - this.#capabilitiesSentAt;
    // The server accepts one update per second. Keep a small scheduling
    // margin so a capabilities announcement racing the first advertisement
    // cannot be lost to clock/timer granularity.
    const delay = Math.max(0, 1_050 - elapsed);
    if (delay === 0) {
      this.#sendCapabilities();
      return;
    }
    this.#capabilitiesTimer = setTimeout(() => {
      this.#capabilitiesTimer = null;
      this.#sendCapabilities();
    }, delay);
  }
  #sendCapabilities(): void {
    if (!this.#sender || !this.#requestedCapabilities) return;
    this.#sender(
      buildMediaCapabilitiesMessage({
        ...this.#requestedCapabilities,
        videoCodecs:
          this.#requestedCapabilities.videoCodecs & this.#serverVideoCodecs,
      }),
    );
    this.#capabilitiesSentAt = monotonicNow();
    this.#resolveCapabilityWaiters();
  }
  #waitForScheduledCapabilities(): Promise<void> {
    if (!this.#capabilitiesTimer) return Promise.resolve();
    return new Promise((resolve) => this.#capabilitiesWaiters.add(resolve));
  }
  #resolveCapabilityWaiters(): void {
    for (const resolve of this.#capabilitiesWaiters) resolve();
    this.#capabilitiesWaiters.clear();
  }
}

function emptyState(): DesktopMediaState {
  return {
    runtimeFlags: 0,
    activeFlags: 0,
    microphoneOwner: 0n,
    cameraOwner: 0n,
    screencasts: [],
  };
}

function emptyLease(kind: "microphone" | "camera"): MediaLeaseState {
  return {
    kind,
    status: "inactive",
    leaseId: 0,
    codec: 0,
    width: 0,
    height: 0,
    fps: 0,
    credit: 0,
    error: null,
  };
}

function monotonicNow(): number {
  return typeof performance !== "undefined" ? performance.now() : Date.now();
}

function clampInt(value: number, max: number): number {
  return Math.max(0, Math.min(max, Math.trunc(value)));
}

function requireUnsigned(
  value: number,
  max: number,
  name: string,
  nonzero = false,
): number {
  if (
    !Number.isSafeInteger(value) ||
    value < (nonzero ? 1 : 0) ||
    value > max
  ) {
    throw new RangeError(`invalid ${name}`);
  }
  return value;
}

function actionBigInt(
  value: number,
  name: string,
  scale = 1,
  round: (value: number) => number = Math.trunc,
): bigint {
  const encoded = round(value * scale);
  if (!Number.isSafeInteger(encoded)) {
    throw new RangeError(`invalid ${name}`);
  }
  return BigInt(encoded);
}

function pushU16(out: number[], value: number): void {
  out.push(value & 0xff, (value >>> 8) & 0xff);
}
function pushU32(out: number[], value: number): void {
  out.push(
    value & 0xff,
    (value >>> 8) & 0xff,
    (value >>> 16) & 0xff,
    (value >>> 24) & 0xff,
  );
}
function pushString16(out: number[], value: string, max = 0xffff): void {
  const encoded = new Uint8Array(Math.min(max, 0xffff));
  const { written } = encoder.encodeInto(value, encoded);
  pushU16(out, written);
  for (let index = 0; index < written; index++) out.push(encoded[index]!);
}
