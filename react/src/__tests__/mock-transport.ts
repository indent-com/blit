import type { BlitTransport, ConnectionStatus } from '../types';
import {
  S2C_CREATED,
  S2C_CLOSED,
  S2C_LIST,
  S2C_TITLE,
  S2C_UPDATE,
} from '../types';

/**
 * A mock BlitTransport for testing. Starts as 'connected'.
 * - `push(data)` delivers a server message to the consumer.
 * - `sent` captures all client messages.
 */
export class MockTransport implements BlitTransport {
  private _status: ConnectionStatus = 'connected';
  onmessage: ((data: ArrayBuffer) => void) | null = null;
  onstatuschange: ((status: ConnectionStatus) => void) | null = null;
  sent: Uint8Array[] = [];

  get status() {
    return this._status;
  }

  send(data: Uint8Array) {
    this.sent.push(new Uint8Array(data));
  }

  close() {
    this.setStatus('disconnected');
  }

  setStatus(s: ConnectionStatus) {
    this._status = s;
    this.onstatuschange?.(s);
  }

  /** Deliver a raw server message to the consumer. */
  push(data: Uint8Array) {
    this.onmessage?.(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength) as ArrayBuffer);
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
}
