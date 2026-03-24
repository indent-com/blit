import type { BlitTransport, BlitTransportEventMap, ConnectionStatus } from '../types';
import {
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_CLOSED,
  S2C_HELLO,
  S2C_LIST,
  S2C_TITLE,
  S2C_UPDATE,
} from '../types';

export class MockTransport implements BlitTransport {
  private _status: ConnectionStatus;
  private messageListeners = new Set<(data: ArrayBuffer) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  sent: Uint8Array[] = [];

  constructor(initialStatus: ConnectionStatus = 'connected') {
    this._status = initialStatus;
  }

  get status() {
    return this._status;
  }

  send(data: Uint8Array) {
    this.sent.push(new Uint8Array(data));
  }

  close() {
    this.setStatus('disconnected');
  }

  addEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void {
    if (type === 'message') {
      this.messageListeners.add(listener as (data: ArrayBuffer) => void);
    } else if (type === 'statuschange') {
      this.statusListeners.add(listener as (status: ConnectionStatus) => void);
    }
  }

  removeEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void {
    if (type === 'message') {
      this.messageListeners.delete(listener as (data: ArrayBuffer) => void);
    } else if (type === 'statuschange') {
      this.statusListeners.delete(listener as (status: ConnectionStatus) => void);
    }
  }

  setStatus(s: ConnectionStatus) {
    this._status = s;
    for (const l of this.statusListeners) l(s);
  }

  push(data: Uint8Array) {
    const buf = data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer;
    for (const l of this.messageListeners) l(buf);
  }

  // --- Helpers to build wire-format server messages ---

  pushCreated(ptyId: number, tag = '') {
    const tagBytes = new TextEncoder().encode(tag);
    const msg = new Uint8Array(3 + tagBytes.length);
    msg[0] = S2C_CREATED;
    msg[1] = ptyId & 0xff;
    msg[2] = (ptyId >> 8) & 0xff;
    msg.set(tagBytes, 3);
    this.push(msg);
  }

  pushCreatedN(nonce: number, ptyId: number, tag = '') {
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

  pushClosed(ptyId: number) {
    this.push(new Uint8Array([S2C_CLOSED, ptyId & 0xff, (ptyId >> 8) & 0xff]));
  }

  pushList(entries: { ptyId: number; tag?: string }[]) {
    const parts: number[] = [S2C_LIST, entries.length & 0xff, (entries.length >> 8) & 0xff];
    for (const { ptyId, tag = '' } of entries) {
      const tagBytes = new TextEncoder().encode(tag);
      parts.push(ptyId & 0xff, (ptyId >> 8) & 0xff);
      parts.push(tagBytes.length & 0xff, (tagBytes.length >> 8) & 0xff);
      for (const b of tagBytes) parts.push(b);
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

  pushHello(version: number, features: number) {
    const msg = new Uint8Array(7);
    msg[0] = S2C_HELLO;
    msg[1] = version & 0xff;
    msg[2] = (version >> 8) & 0xff;
    msg[3] = features & 0xff;
    msg[4] = (features >> 8) & 0xff;
    msg[5] = (features >> 16) & 0xff;
    msg[6] = (features >> 24) & 0xff;
    this.push(msg);
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
