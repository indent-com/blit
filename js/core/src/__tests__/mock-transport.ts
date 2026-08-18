import type {
  BlitTransport,
  BlitTransportEventMap,
  BlitTransportMessage,
  ConnectionStatus,
  BlitClientInfo,
  BlitClientOrigin,
} from "../types";
import {
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_CREATE_FAILED,
  S2C_CLOSED,
  S2C_EXITED,
  S2C_HELLO,
  S2C_KICKED,
  CLIENT_ORIGIN_EXTENSION,
  CLIENT_ORIGIN_NETWORK,
  S2C_CLIENT_LIST,
  S2C_CLIENT_LIST2,
  S2C_KICK_RESULT,
  S2C_LIST,
  S2C_QUIT,
  S2C_READY,
  S2C_TEXT,
  S2C_TITLE,
  S2C_UPDATE,
} from "../types";

/** A catalog entry to encode. Omitting `origin` asks for the older shape,
 *  which is the shape a server without `FEATURE_CLIENT_ORIGIN` sends. */
type ClientListFixture = Omit<BlitClientInfo, "origin"> & {
  origin?: BlitClientOrigin | null;
};

/** The `S2C_CLIENT_LIST2` origin block for one catalog entry. */
function encodeClientOrigin(origin: BlitClientOrigin): {
  kind: number;
  payload: Uint8Array;
} {
  if (origin.kind === "extension") {
    const name = new TextEncoder().encode(origin.name);
    const payload = new Uint8Array(28 + name.length);
    const view = new DataView(payload.buffer);
    view.setBigUint64(0, origin.extensionId, true);
    view.setBigUint64(8, origin.definitionRevision, true);
    view.setBigUint64(16, origin.attempt, true);
    view.setUint32(24, origin.taskId, true);
    payload.set(name, 28);
    return { kind: CLIENT_ORIGIN_EXTENSION, payload };
  }
  if (origin.kind === "unknown") {
    return { kind: origin.originKind, payload: new Uint8Array([1, 2, 3]) };
  }
  return { kind: CLIENT_ORIGIN_NETWORK, payload: new Uint8Array(0) };
}

export class MockTransport implements BlitTransport {
  private _status: ConnectionStatus;
  private messageListeners = new Set<(data: BlitTransportMessage) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  sent: Uint8Array[] = [];
  authRejected = false;
  lastError: string | null = null;
  reconnectCount = 0;
  suspendCount = 0;

  constructor(initialStatus: ConnectionStatus = "connected") {
    this._status = initialStatus;
  }

  get status() {
    return this._status;
  }

  connect() {}

  reconnect() {
    this.reconnectCount++;
    this.setStatus("disconnected");
    this.setStatus("connecting");
  }

  suspend() {
    this.suspendCount++;
    this.setStatus("disconnected");
  }

  send(data: Uint8Array) {
    this.sent.push(new Uint8Array(data));
  }

  close() {
    this.setStatus("closed");
  }

  addEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.add(
        listener as (data: BlitTransportMessage) => void,
      );
    } else if (type === "statuschange") {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.delete(
        listener as (data: BlitTransportMessage) => void,
      );
    } else if (type === "statuschange") {
      this.statusListeners.delete(
        listener as (status: ConnectionStatus) => void,
      );
    }
  }

  setStatus(s: ConnectionStatus) {
    this._status = s;
    for (const l of this.statusListeners) l(s);
  }

  push(data: Uint8Array) {
    const buf = data.buffer.slice(
      data.byteOffset,
      data.byteOffset + data.byteLength,
    ) as ArrayBuffer;
    for (const l of this.messageListeners) l(buf);
  }

  /** Deliver a borrowed view without copying, as BYOB transports do. */
  pushBorrowed(data: Uint8Array) {
    for (const l of this.messageListeners) l(data);
  }

  // --- Helpers to build wire-format server messages ---

  pushCreated(ptyId: number, tag = "") {
    const tagBytes = new TextEncoder().encode(tag);
    const msg = new Uint8Array(3 + tagBytes.length);
    msg[0] = S2C_CREATED;
    msg[1] = ptyId & 0xff;
    msg[2] = (ptyId >> 8) & 0xff;
    msg.set(tagBytes, 3);
    this.push(msg);
  }

  pushCreatedN(nonce: number, ptyId: number, tag = "") {
    const tagBytes = new TextEncoder().encode(tag);
    const msg = new Uint8Array(5 + tagBytes.length);
    msg[0] = S2C_CREATED_N;
    msg[1] = nonce & 0xff;
    msg[2] = (nonce >> 8) & 0xff;
    msg[3] = ptyId & 0xff;
    msg[4] = (ptyId >> 8) & 0xff;
    msg.set(tagBytes, 5);
    this.push(msg);
  }

  /** Wire: [0x10][nonce:2][status:1][detail:N]. */
  pushCreateFailed(nonce: number, status: number, detail = "") {
    const detailBytes = new TextEncoder().encode(detail);
    const msg = new Uint8Array(4 + detailBytes.length);
    msg[0] = S2C_CREATE_FAILED;
    msg[1] = nonce & 0xff;
    msg[2] = (nonce >> 8) & 0xff;
    msg[3] = status;
    msg.set(detailBytes, 4);
    this.push(msg);
  }

  pushClosed(ptyId: number) {
    this.push(new Uint8Array([S2C_CLOSED, ptyId & 0xff, (ptyId >> 8) & 0xff]));
  }

  /** Wire: [0x08][pty_id:2][exit_status:4] (i32 LE). */
  pushExited(ptyId: number, exitStatus: number) {
    const msg = new Uint8Array(7);
    const view = new DataView(msg.buffer);
    msg[0] = S2C_EXITED;
    msg[1] = ptyId & 0xff;
    msg[2] = (ptyId >> 8) & 0xff;
    view.setInt32(3, exitStatus, true);
    this.push(msg);
  }

  /** A legacy-style EXITED frame that omits the exit_status bytes. */
  pushExitedRaw(ptyId: number) {
    this.push(new Uint8Array([S2C_EXITED, ptyId & 0xff, (ptyId >> 8) & 0xff]));
  }

  pushList(entries: { ptyId: number; tag?: string; command?: string }[]) {
    const parts: number[] = [
      S2C_LIST,
      entries.length & 0xff,
      (entries.length >> 8) & 0xff,
    ];
    for (const { ptyId, tag = "", command = "" } of entries) {
      const tagBytes = new TextEncoder().encode(tag);
      const cmdBytes = new TextEncoder().encode(command);
      parts.push(ptyId & 0xff, (ptyId >> 8) & 0xff);
      parts.push(tagBytes.length & 0xff, (tagBytes.length >> 8) & 0xff);
      for (const b of tagBytes) parts.push(b);
      parts.push(cmdBytes.length & 0xff, (cmdBytes.length >> 8) & 0xff);
      for (const b of cmdBytes) parts.push(b);
    }
    this.push(new Uint8Array(parts));
  }

  pushTitle(ptyId: number, title: string) {
    const titleBytes = new TextEncoder().encode(title);
    const msg = new Uint8Array(3 + titleBytes.length);
    msg[0] = S2C_TITLE;
    msg[1] = ptyId & 0xff;
    msg[2] = (ptyId >> 8) & 0xff;
    msg.set(titleBytes, 3);
    this.push(msg);
  }

  pushText(nonce: number, ptyId: number, totalLines: number, text: string) {
    const textBytes = new TextEncoder().encode(text);
    const msg = new Uint8Array(13 + textBytes.length);
    const v = new DataView(msg.buffer);
    msg[0] = S2C_TEXT;
    v.setUint16(1, nonce, true);
    v.setUint16(3, ptyId, true);
    v.setUint32(5, totalLines, true);
    msg.set(textBytes, 13);
    this.push(msg);
  }

  pushHello(
    version: number,
    features: number,
    bootGeneration?: bigint,
    serverVersion?: string,
  ) {
    const verBytes =
      serverVersion === undefined
        ? null
        : new TextEncoder().encode(serverVersion);
    const len =
      bootGeneration === undefined
        ? 7
        : verBytes === null
          ? 15
          : 17 + verBytes.length;
    const msg = new Uint8Array(len);
    const view = new DataView(msg.buffer);
    msg[0] = S2C_HELLO;
    view.setUint16(1, version, true);
    view.setUint32(3, features, true);
    if (bootGeneration !== undefined) {
      view.setBigUint64(7, bootGeneration, true);
    }
    if (verBytes !== null) {
      view.setUint16(15, verBytes.length, true);
      msg.set(verBytes, 17);
    }
    this.push(msg);
  }

  pushQuit() {
    this.push(new Uint8Array([S2C_QUIT]));
  }

  pushKicked(reason = "") {
    const reasonBytes = new TextEncoder().encode(reason);
    const msg = new Uint8Array(1 + reasonBytes.length);
    msg[0] = S2C_KICKED;
    msg.set(reasonBytes, 1);
    this.push(msg);
  }

  /** `mangle` rewrites the encoded frame before delivery, for decoder
   *  robustness tests: truncation, trailing bytes, and other frames a healthy
   *  server never sends but a decoder still has to survive. */
  pushClientList(
    nonce: number,
    selfId: bigint,
    clients: readonly ClientListFixture[],
    mangle: (message: Uint8Array) => Uint8Array = (message) => message,
  ) {
    // An entry that carries an origin is only encodable in the wider shape, so
    // the caller picks the opcode by what it puts in the catalog — the same
    // way the server picks it by what the requester asked for.
    const origins = clients.map((client) =>
      client.origin ? encodeClientOrigin(client.origin) : null,
    );
    const withOrigin = origins.some((origin) => origin !== null);
    const length =
      15 +
      clients.reduce(
        (sum, client, index) =>
          sum +
          38 +
          client.terminals.length * 6 +
          client.surfaces.length * 8 +
          client.subscriptions.length * 3 +
          (withOrigin ? 3 + (origins[index]?.payload.length ?? 0) : 0),
        0,
      );
    const msg = new Uint8Array(length);
    const view = new DataView(msg.buffer);
    msg[0] = withOrigin ? S2C_CLIENT_LIST2 : S2C_CLIENT_LIST;
    view.setUint16(1, nonce, true);
    view.setBigUint64(3, selfId, true);
    view.setUint32(11, clients.length, true);
    let offset = 15;
    for (const [index, client] of clients.entries()) {
      view.setBigUint64(offset, client.id, true);
      view.setBigUint64(offset + 8, BigInt(client.ageSeconds), true);
      view.setBigUint64(
        offset + 16,
        BigInt(client.outboundBytesPerSecond),
        true,
      );
      view.setBigUint64(
        offset + 24,
        BigInt(client.inboundBytesPerSecond),
        true,
      );
      view.setUint16(offset + 32, client.terminals.length, true);
      view.setUint16(offset + 34, client.surfaces.length, true);
      view.setUint16(offset + 36, client.subscriptions.length, true);
      offset += 38;
      for (const terminal of client.terminals) {
        view.setUint16(offset, terminal.ptyId, true);
        view.setUint16(offset + 2, terminal.rows ?? 0, true);
        view.setUint16(offset + 4, terminal.cols ?? 0, true);
        offset += 6;
      }
      for (const surface of client.surfaces) {
        view.setUint16(offset, surface.surfaceId, true);
        view.setUint16(offset + 2, surface.width ?? 0, true);
        view.setUint16(offset + 4, surface.height ?? 0, true);
        view.setUint16(offset + 6, surface.scale120 ?? 0, true);
        offset += 8;
      }
      for (const subscription of client.subscriptions) {
        msg[offset] = subscription.kind;
        view.setUint16(offset + 1, subscription.id, true);
        offset += 3;
      }
      if (withOrigin) {
        const origin = origins[index] ?? {
          kind: CLIENT_ORIGIN_NETWORK,
          payload: new Uint8Array(0),
        };
        msg[offset] = origin.kind;
        view.setUint16(offset + 1, origin.payload.length, true);
        msg.set(origin.payload, offset + 3);
        offset += 3 + origin.payload.length;
      }
    }
    this.push(mangle(msg));
  }

  pushKickResult(nonce: number, status: number, detail = "") {
    const detailBytes = new TextEncoder().encode(detail);
    const msg = new Uint8Array(4 + detailBytes.length);
    msg[0] = S2C_KICK_RESULT;
    new DataView(msg.buffer).setUint16(1, nonce, true);
    msg[3] = status;
    msg.set(detailBytes, 4);
    this.push(msg);
  }

  pushReady() {
    this.push(new Uint8Array([S2C_READY]));
  }

  pushUpdate(ptyId: number, payload: Uint8Array = new Uint8Array(0)) {
    const msg = new Uint8Array(3 + payload.length);
    msg[0] = S2C_UPDATE;
    msg[1] = ptyId & 0xff;
    msg[2] = (ptyId >> 8) & 0xff;
    msg.set(payload, 3);
    this.push(msg);
  }
}
