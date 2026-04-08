/**
 * Browser-side WebRTC transport for `blit share`.
 *
 * Connects to the signaling hub as a consumer, performs the SDP/ICE exchange
 * with the producer (blit-webrtc-forwarder), and wraps the resulting
 * RTCPeerConnection data channel in a BlitTransport via
 * createWebRtcDataChannelTransport.
 */

import nacl from "tweetnacl";
import { createWebRtcDataChannelTransport } from "./webrtc";
import type { BlitDebug, BlitTransport, ConnectionStatus } from "../types";

const PBKDF2_ROUNDS = 100_000;

async function pbkdf2Derive(
  input: Uint8Array,
  salt: Uint8Array,
): Promise<Uint8Array> {
  // Copy into plain ArrayBuffers to satisfy SubtleCrypto's BufferSource type.
  const inputAb = input.buffer.slice(
    input.byteOffset,
    input.byteOffset + input.byteLength,
  ) as ArrayBuffer;
  const saltAb = salt.buffer.slice(
    salt.byteOffset,
    salt.byteOffset + salt.byteLength,
  ) as ArrayBuffer;
  const key = await crypto.subtle.importKey("raw", inputAb, "PBKDF2", false, [
    "deriveBits",
  ]);
  const bits = await crypto.subtle.deriveBits(
    {
      name: "PBKDF2",
      salt: saltAb,
      iterations: PBKDF2_ROUNDS,
      hash: "SHA-256",
    },
    key,
    256,
  );
  return new Uint8Array(bits);
}

interface DerivedKeys {
  /** Ed25519 signing keypair — public key is the hub channel ID. */
  signing: nacl.SignKeyPair;
  /** Our X25519 secret key (RW or RO consumer). */
  ourX25519Secret: Uint8Array;
  /** Producer's X25519 public key (derived from Ed25519 seed). */
  producerX25519Public: Uint8Array;
  /** True when this is a read-only consumer key. */
  readOnly: boolean;
}

/** Derive all keys needed for a RW consumer from a passphrase. */
async function deriveKeys(passphrase: string): Promise<DerivedKeys> {
  const enc = new TextEncoder();
  const passphraseBytes = enc.encode(passphrase);

  // Level 1: Ed25519 seed from passphrase (channel ID / signing key).
  const ed25519Seed = await pbkdf2Derive(
    passphraseBytes,
    enc.encode("https://blit.sh"),
  );
  const signing = nacl.sign.keyPair.fromSeed(ed25519Seed);

  // Level 1: RW consumer X25519 secret key from passphrase.
  const ourX25519Secret = await pbkdf2Derive(
    passphraseBytes,
    enc.encode("blit-consumer-rw-x25519"),
  );

  // Level 2: producer X25519 secret key from Ed25519 seed bytes,
  // then derive its public key.
  const producerX25519Secret = await pbkdf2Derive(
    ed25519Seed,
    enc.encode("blit-producer-x25519"),
  );
  const producerX25519Public = nacl.scalarMult.base(producerX25519Secret);

  return { signing, ourX25519Secret, producerX25519Public, readOnly: false };
}

/** Base64url decode (no padding). */
function base64urlDecode(str: string): Uint8Array {
  const padded = str.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/**
 * Derive all keys needed for a RO consumer from a read-only token.
 *
 * The RO token is the base64url-encoded Ed25519 signing key (seed).
 * Mirrors Rust's `ConsumerKeys::derive_ro`.
 */
async function deriveKeysFromRoToken(token: string): Promise<DerivedKeys> {
  const ed25519Seed = base64urlDecode(token);
  if (ed25519Seed.length !== 32) {
    throw new Error(
      `Invalid RO token: expected 32 bytes, got ${ed25519Seed.length}`,
    );
  }
  const signing = nacl.sign.keyPair.fromSeed(ed25519Seed);

  // Level 2 (from Ed25519 seed): RO consumer X25519 sk.
  const ourX25519Secret = await pbkdf2Derive(
    ed25519Seed,
    new TextEncoder().encode("blit-consumer-ro-x25519"),
  );

  // Level 2 (from Ed25519 seed): producer X25519 sk → derive its public key.
  const producerX25519Secret = await pbkdf2Derive(
    ed25519Seed,
    new TextEncoder().encode("blit-producer-x25519"),
  );
  const producerX25519Public = nacl.scalarMult.base(producerX25519Secret);

  return { signing, ourX25519Secret, producerX25519Public, readOnly: true };
}

/**
 * Detect and parse a read-only token (ends with `.ro`) from the passphrase
 * field, then derive the appropriate keys.
 */
async function deriveConsumerKeys(passphrase: string): Promise<DerivedKeys> {
  if (passphrase.endsWith(".ro")) {
    const token = passphrase.slice(0, -3);
    return deriveKeysFromRoToken(token);
  }
  return deriveKeys(passphrase);
}

function hexEncode(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function signPayload(secretKey: Uint8Array, payload: Uint8Array): string {
  const signed = nacl.sign(payload, secretKey); // 64-byte sig + payload
  return btoa(String.fromCharCode(...signed));
}

/**
 * Encrypt `data` in a NaCl box and build a signed hub message.
 * Inner payload: `{"box":"<base64(nonce||ciphertext)>"}`.
 */
function buildSealedMessage(
  keys: DerivedKeys,
  signingSecretKey: Uint8Array,
  target: string,
  data: unknown,
): string {
  const plaintext = new TextEncoder().encode(JSON.stringify(data));
  const nonce = nacl.randomBytes(nacl.box.nonceLength);
  const ciphertext = nacl.box(
    plaintext,
    nonce,
    keys.producerX25519Public,
    keys.ourX25519Secret,
  );
  const sealed = new Uint8Array(nonce.length + ciphertext.length);
  sealed.set(nonce);
  sealed.set(ciphertext, nonce.length);
  const sealedB64 = btoa(String.fromCharCode(...sealed));
  const inner = JSON.stringify({ box: sealedB64 });
  const innerBytes = new TextEncoder().encode(inner);
  const signed = signPayload(signingSecretKey, innerBytes);
  return JSON.stringify({ signed, target });
}

/**
 * Try to open a NaCl box sealed payload from the producer.
 * Returns the decrypted JSON value, or `null` on failure.
 */
function openSealedData(
  data: Record<string, unknown>,
  keys: DerivedKeys,
): unknown | null {
  const sealedB64 = data["box"];
  if (typeof sealedB64 !== "string") return null;
  const sealed = Uint8Array.from(atob(sealedB64), (c) => c.charCodeAt(0));
  if (sealed.length < nacl.box.nonceLength) return null;
  const nonce = sealed.slice(0, nacl.box.nonceLength);
  const ciphertext = sealed.slice(nacl.box.nonceLength);
  const plaintext = nacl.box.open(
    ciphertext,
    nonce,
    keys.producerX25519Public,
    keys.ourX25519Secret,
  );
  if (!plaintext) return null;
  try {
    return JSON.parse(new TextDecoder().decode(plaintext)) as unknown;
  } catch {
    return null;
  }
}

interface ServerMessage {
  type: string;
  sessionId?: string;
  role?: string;
  from?: string;
  data?: Record<string, unknown>;
  message?: string;
}

/**
 * Fetch ICE server config from the hub's /ice endpoint.
 * Returns RTCIceServer[] for the browser's RTCPeerConnection.
 */
async function fetchIceServers(hubWsUrl: string): Promise<RTCIceServer[]> {
  const httpUrl = hubWsUrl
    .replace(/^wss:\/\//, "https://")
    .replace(/^ws:\/\//, "http://")
    .replace(/\/$/, "");
  try {
    const resp = await fetch(`${httpUrl}/ice`);
    const config = (await resp.json()) as {
      iceServers: Array<{
        urls: string | string[];
        username?: string;
        credential?: string;
      }>;
    };
    return config.iceServers.map((s) => ({
      urls: s.urls,
      ...(s.username && { username: s.username }),
      ...(s.credential && { credential: s.credential }),
    }));
  } catch {
    return [{ urls: "stun:stun.l.google.com:19302" }];
  }
}

/**
 * Create a BlitTransport that connects to a shared session via WebRTC.
 *
 * The transport handles all signaling internally: hub WebSocket connection,
 * Ed25519-signed message exchange, SDP offer/answer, and ICE candidate relay.
 *
 * Supports reconnection: calling `connect()` when the transport is in a
 * `"disconnected"` or `"error"` state tears down old resources and re-runs
 * the full signaling + WebRTC handshake.
 */
const noopDebug: BlitDebug = { log() {}, warn() {}, error() {} };

export function createShareTransport(
  hubWsUrl: string,
  passphrase: string,
  debug?: BlitDebug,
): BlitTransport {
  const dbg = debug ?? noopDebug;
  let _status: ConnectionStatus = "connecting";
  let _lastError: string | null = null;
  let inner: BlitTransport | null = null;
  let ws: WebSocket | null = null;
  let pc: RTCPeerConnection | null = null;
  let disposed = false;
  let started = false;
  let connectGeneration = 0;
  let cachedKeys: DerivedKeys | null = null;
  const earlyMessages: ArrayBuffer[] = [];
  const messageListeners = new Set<(data: ArrayBuffer) => void>();
  const statusListeners = new Set<(status: ConnectionStatus) => void>();

  function setStatus(s: ConnectionStatus) {
    if (_status === s) return;
    dbg.log("status %s → %s", _status, s);
    _status = s;
    for (const l of statusListeners) l(s);
  }

  function dispatch(data: ArrayBuffer) {
    if (!started) {
      earlyMessages.push(data);
    } else {
      for (const l of messageListeners) l(data);
    }
  }

  /** Tear down the current signaling WS, peer connection, and inner transport. */
  function teardown() {
    if (inner) {
      inner.close();
      inner = null;
    }
    if (pc) {
      try {
        pc.close();
      } catch {
        // Ignore.
      }
      pc = null;
    }
    if (ws) {
      try {
        ws.close();
      } catch {
        // Ignore.
      }
      ws = null;
    }
  }

  /** Run the full signaling + WebRTC setup. */
  async function doConnect(generation: number) {
    try {
      if (!cachedKeys) {
        dbg.log(
          passphrase.endsWith(".ro")
            ? "deriving keys from RO token"
            : "deriving keys from passphrase",
        );
        cachedKeys = await deriveConsumerKeys(passphrase);
      }
      const keys = cachedKeys;
      // Connect to the producer's channel (the passphrase-derived Ed25519
      // public key) and sign with the matching secret key.  The hub verifies
      // signatures against the channel ID as the Ed25519 public key, so the
      // signing key must correspond to the channel we connect to.
      // Multiple consumers can coexist in the same channel; the hub gives each
      // a unique sessionId (UUID).
      const pubHex = hexEncode(keys.signing.publicKey);
      dbg.log("channel id (passphrase-derived pubkey): %s", pubHex);
      const iceServers = await fetchIceServers(hubWsUrl);
      dbg.log("ICE servers: %o", iceServers);

      if (disposed || generation !== connectGeneration) {
        dbg.warn("stale connect attempt, aborting");
        return;
      }

      const wsUrl = `${hubWsUrl.replace(/\/$/, "")}/channel/${pubHex}/consumer`;
      dbg.log("connecting to signaling hub: %s", wsUrl);
      ws = new WebSocket(wsUrl);

      await new Promise<void>((resolve, reject) => {
        ws!.onopen = () => {
          dbg.log("signaling WS open");
          resolve();
        };
        ws!.onerror = () => {
          dbg.error("signaling WS error");
          reject(new Error("signaling connection failed"));
        };
        if (disposed) reject(new Error("disposed"));
      });

      if (disposed || generation !== connectGeneration) {
        dbg.warn("stale after WS open, aborting");
        ws?.close();
        return;
      }

      // Wait for registered + peer_joined (producer role only).
      dbg.log("waiting for registered + peer_joined");
      const producerSessionId = await new Promise<string>((resolve, reject) => {
        let registered = false;
        ws!.onmessage = (e) => {
          const m = JSON.parse(e.data as string) as ServerMessage;
          dbg.log("signaling ← %s %o", m.type, m);
          if (m.type === "registered") {
            registered = true;
            dbg.log("registered with hub");
          } else if (m.type === "peer_joined" && registered) {
            // Only connect to the producer (role === "producer"), not other consumers.
            if (m.role && m.role !== "producer") {
              dbg.log(
                "ignoring non-producer peer: %s (role=%s)",
                m.sessionId,
                m.role,
              );
              return;
            }
            dbg.log("producer joined: %s", m.sessionId);
            resolve(m.sessionId!);
          } else if (m.type === "error") {
            dbg.error("signaling error: %s", m.message);
            reject(new Error(m.message ?? "signaling error"));
          }
        };
        ws!.onclose = () => {
          dbg.warn("signaling WS closed before peer joined");
          reject(new Error("signaling closed before peer joined"));
        };
      });

      if (disposed || generation !== connectGeneration) {
        dbg.warn("stale after peer joined, aborting");
        ws?.close();
        return;
      }

      // Create RTCPeerConnection and data channel transport
      dbg.log(
        "creating RTCPeerConnection with %d ICE server(s)",
        iceServers.length,
      );
      pc = new RTCPeerConnection({ iceServers });

      pc.onconnectionstatechange = () =>
        dbg.log("pc.connectionState = %s", pc!.connectionState);
      pc.oniceconnectionstatechange = () =>
        dbg.log("pc.iceConnectionState = %s", pc!.iceConnectionState);
      pc.onicegatheringstatechange = () =>
        dbg.log("pc.iceGatheringState = %s", pc!.iceGatheringState);
      pc.onsignalingstatechange = () =>
        dbg.log("pc.signalingState = %s", pc!.signalingState);

      const dcTransport = createWebRtcDataChannelTransport(pc);
      inner = dcTransport;

      // Forward inner transport events
      dcTransport.addEventListener("message", (data: ArrayBuffer) =>
        dispatch(data),
      );
      dcTransport.addEventListener("statuschange", (s: ConnectionStatus) => {
        if (disposed || generation !== connectGeneration) return;
        setStatus(s);
      });

      // Create SDP offer (data channel was already created by createWebRtcDataChannelTransport)
      dbg.log("creating SDP offer");
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);
      dbg.log("SDP offer set as local description, type=%s", offer.type);

      // Send the offer encrypted to the producer.
      const sdpData = { sdp: { type: offer.type, sdp: offer.sdp } };
      ws!.send(
        buildSealedMessage(
          keys,
          keys.signing.secretKey,
          producerSessionId,
          sdpData,
        ),
      );
      dbg.log("sent encrypted SDP offer to producer %s", producerSessionId);

      // Buffer ICE candidates that arrive before we have the remote description
      const pendingCandidates: RTCIceCandidateInit[] = [];
      let remoteDescSet = false;

      // Send our ICE candidates to the producer, encrypted.
      pc.onicecandidate = (e) => {
        if (!e.candidate || disposed || generation !== connectGeneration)
          return;
        dbg.log("local ICE candidate: %s", e.candidate.candidate);
        const candidateData = { candidate: e.candidate.toJSON() };
        ws!.send(
          buildSealedMessage(
            keys,
            keys.signing.secretKey,
            producerSessionId,
            candidateData,
          ),
        );
      };

      // Receive answer + remote ICE candidates.
      // The producer sends replies encrypted with the consumer's X25519 public
      // key; try to decrypt, fall back to plaintext for legacy producers.
      ws!.onmessage = (e) => {
        const m = JSON.parse(e.data as string) as ServerMessage;
        dbg.log(
          "signaling ← %s %o",
          m.type,
          m.data ? Object.keys(m.data) : "no data",
        );
        if (m.type !== "signal" || !m.data) return;

        // Decrypt the payload if it's a crypto_box sealed message.
        const data: Record<string, unknown> =
          (openSealedData(m.data, keys) as Record<string, unknown>) ?? m.data;

        if (data.sdp) {
          dbg.log("received remote SDP answer");
          const sdp = data.sdp as { type?: string; sdp?: string };
          pc!
            .setRemoteDescription(
              new RTCSessionDescription({
                type: (sdp.type as RTCSdpType) ?? "answer",
                sdp: sdp.sdp as string,
              }),
            )
            .then(() => {
              remoteDescSet = true;
              dbg.log(
                "remote description set, flushing %d pending candidates",
                pendingCandidates.length,
              );
              for (const c of pendingCandidates) {
                pc!.addIceCandidate(new RTCIceCandidate(c)).catch(() => {});
              }
              pendingCandidates.length = 0;
            })
            .catch((err) => {
              dbg.error("setRemoteDescription failed: %o", err);
              if (disposed || generation !== connectGeneration) return;
              _lastError = err instanceof Error ? err.message : String(err);
              setStatus("error");
            });
        } else if (data.candidate) {
          const candidate = data.candidate as RTCIceCandidateInit;
          if (remoteDescSet) {
            dbg.log(
              "remote ICE candidate (applied): %s",
              (candidate as { candidate?: string }).candidate,
            );
            pc!.addIceCandidate(new RTCIceCandidate(candidate)).catch(() => {});
          } else {
            dbg.log(
              "remote ICE candidate (buffered): %s",
              (candidate as { candidate?: string }).candidate,
            );
            pendingCandidates.push(candidate);
          }
        }
      };

      ws!.onclose = () => {
        dbg.log("signaling WS closed (expected — WebRTC is peer-to-peer now)");
      };

      if (started) {
        dbg.log("calling inner transport.connect()");
        dcTransport.connect();
      } else {
        dbg.log("inner transport created but start() not yet called");
      }
    } catch (err) {
      dbg.error("share transport error: %o", err);
      if (disposed || generation !== connectGeneration) return;
      _lastError = err instanceof Error ? err.message : String(err);
      setStatus("disconnected");
    }
  }

  const transport: BlitTransport = {
    connect() {
      if (disposed) return;

      // First call: mark as started, kick off the initial connection, and
      // flush any early messages that arrived before connect() was called.
      if (!started) {
        started = true;
        doConnect(connectGeneration);
        for (const msg of earlyMessages) {
          for (const l of messageListeners) l(msg);
        }
        earlyMessages.length = 0;
        return;
      }

      // Subsequent calls: reconnect if currently disconnected or errored.
      if (_status === "disconnected" || _status === "error") {
        dbg.log(
          "reconnect requested (status=%s), tearing down and retrying",
          _status,
        );
        teardown();
        connectGeneration++;
        setStatus("connecting");
        doConnect(connectGeneration);
      }
    },

    get status() {
      return _status;
    },
    get authRejected() {
      return false;
    },
    get lastError() {
      return _lastError;
    },

    addEventListener(type: string, listener: (data: never) => void) {
      if (type === "message") {
        messageListeners.add(
          listener as unknown as (data: ArrayBuffer) => void,
        );
      } else if (type === "statuschange") {
        statusListeners.add(
          listener as unknown as (status: ConnectionStatus) => void,
        );
      }
    },

    removeEventListener(type: string, listener: (data: never) => void) {
      if (type === "message") {
        messageListeners.delete(
          listener as unknown as (data: ArrayBuffer) => void,
        );
      } else if (type === "statuschange") {
        statusListeners.delete(
          listener as unknown as (status: ConnectionStatus) => void,
        );
      }
    },

    send(data: Uint8Array) {
      inner?.send(data);
    },

    close() {
      disposed = true;
      teardown();
      setStatus("closed");
    },
  };

  return transport;
}
