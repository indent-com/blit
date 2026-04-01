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
import type { BlitTransport, ConnectionStatus } from "../types";

const PBKDF2_SALT = new TextEncoder().encode("https://blit.sh");
const PBKDF2_ROUNDS = 100_000;

/** Derive Ed25519 signing keypair from passphrase (PBKDF2-SHA256). */
async function deriveKeypair(passphrase: string): Promise<nacl.SignKeyPair> {
  const raw = new TextEncoder().encode(passphrase);
  const key = await crypto.subtle.importKey("raw", raw, "PBKDF2", false, [
    "deriveBits",
  ]);
  const bits = await crypto.subtle.deriveBits(
    {
      name: "PBKDF2",
      salt: PBKDF2_SALT,
      iterations: PBKDF2_ROUNDS,
      hash: "SHA-256",
    },
    key,
    256,
  );
  return nacl.sign.keyPair.fromSeed(new Uint8Array(bits));
}

function hexEncode(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

function signPayload(secretKey: Uint8Array, payload: Uint8Array): string {
  const signed = nacl.sign(payload, secretKey); // 64-byte sig + payload
  return btoa(String.fromCharCode(...signed));
}

function buildSignedMessage(
  secretKey: Uint8Array,
  target: string,
  data: unknown,
): string {
  const payload = new TextEncoder().encode(JSON.stringify(data));
  return JSON.stringify({ signed: signPayload(secretKey, payload), target });
}

interface ServerMessage {
  type: string;
  sessionId?: string;
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
 */
export function createShareTransport(
  hubWsUrl: string,
  passphrase: string,
): BlitTransport {
  let _status: ConnectionStatus = "connecting";
  let _lastError: string | null = null;
  let inner: BlitTransport | null = null;
  let ws: WebSocket | null = null;
  let pc: RTCPeerConnection | null = null;
  let disposed = false;
  let started = false;
  const earlyMessages: ArrayBuffer[] = [];
  const messageListeners = new Set<(data: ArrayBuffer) => void>();
  const statusListeners = new Set<(status: ConnectionStatus) => void>();

  function setStatus(s: ConnectionStatus) {
    if (_status === s) return;
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

  // Run the signaling + WebRTC setup asynchronously
  (async () => {
    try {
      const keypair = await deriveKeypair(passphrase);
      const pubHex = hexEncode(keypair.publicKey);
      const iceServers = await fetchIceServers(hubWsUrl);

      if (disposed) return;

      const wsUrl = `${hubWsUrl.replace(/\/$/, "")}/channel/${pubHex}/consumer`;
      ws = new WebSocket(wsUrl);

      await new Promise<void>((resolve, reject) => {
        ws!.onopen = () => resolve();
        ws!.onerror = () => reject(new Error("signaling connection failed"));
        if (disposed) reject(new Error("disposed"));
      });

      if (disposed) {
        ws.close();
        return;
      }

      // Wait for registered + peer_joined
      const producerSessionId = await new Promise<string>((resolve, reject) => {
        let registered = false;
        ws!.onmessage = (e) => {
          const m = JSON.parse(e.data as string) as ServerMessage;
          if (m.type === "registered") {
            registered = true;
          } else if (m.type === "peer_joined" && registered) {
            resolve(m.sessionId!);
          } else if (m.type === "error") {
            reject(new Error(m.message ?? "signaling error"));
          }
        };
        ws!.onclose = () =>
          reject(new Error("signaling closed before peer joined"));
      });

      if (disposed) {
        ws.close();
        return;
      }

      // Create RTCPeerConnection and data channel transport
      pc = new RTCPeerConnection({ iceServers });
      const transport = createWebRtcDataChannelTransport(pc);
      inner = transport;

      // Forward inner transport events
      transport.addEventListener("message", (data: ArrayBuffer) =>
        dispatch(data),
      );
      transport.addEventListener("statuschange", (s: ConnectionStatus) => {
        if (disposed) return;
        setStatus(s);
      });

      // Create SDP offer (data channel was already created by createWebRtcDataChannelTransport)
      const offer = await pc.createOffer();
      await pc.setLocalDescription(offer);

      // Send the offer via signaling
      const sdpData = { sdp: { type: offer.type, sdp: offer.sdp } };
      ws!.send(
        buildSignedMessage(keypair.secretKey, producerSessionId, sdpData),
      );

      // Buffer ICE candidates that arrive before we have the remote description
      const pendingCandidates: RTCIceCandidateInit[] = [];
      let remoteDescSet = false;

      // Send our ICE candidates to the producer
      pc.onicecandidate = (e) => {
        if (!e.candidate || disposed) return;
        const candidateData = { candidate: e.candidate.toJSON() };
        ws!.send(
          buildSignedMessage(
            keypair.secretKey,
            producerSessionId,
            candidateData,
          ),
        );
      };

      // Receive answer + remote ICE candidates
      ws!.onmessage = (e) => {
        const m = JSON.parse(e.data as string) as ServerMessage;
        if (m.type !== "signal" || !m.data) return;

        if (m.data.sdp) {
          const sdp = m.data.sdp as { type?: string; sdp?: string };
          pc!.setRemoteDescription(
            new RTCSessionDescription({
              type: (sdp.type as RTCSdpType) ?? "answer",
              sdp: sdp.sdp as string,
            }),
          ).then(() => {
            remoteDescSet = true;
            for (const c of pendingCandidates) {
              pc!.addIceCandidate(new RTCIceCandidate(c)).catch(() => {});
            }
            pendingCandidates.length = 0;
          }).catch((err) => {
            if (disposed) return;
            _lastError = err instanceof Error ? err.message : String(err);
            setStatus("error");
          });
        } else if (m.data.candidate) {
          const candidate = m.data.candidate as RTCIceCandidateInit;
          if (remoteDescSet) {
            pc!.addIceCandidate(new RTCIceCandidate(candidate)).catch(() => {});
          } else {
            pendingCandidates.push(candidate);
          }
        }
      };

      ws!.onclose = () => {
        // Signaling done — WebRTC connection continues peer-to-peer
      };

      if (started) transport.connect();
    } catch (err) {
      if (disposed) return;
      _lastError = err instanceof Error ? err.message : String(err);
      setStatus("error");
    }
  })();

  const transport: BlitTransport = {
    connect() {
      if (started) return;
      started = true;
      for (const msg of earlyMessages) {
        for (const l of messageListeners) l(msg);
      }
      earlyMessages.length = 0;
      inner?.connect();
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
      ws?.close();
      pc?.close();
      inner?.close();
      setStatus("disconnected");
    },
  };

  return transport;
}
