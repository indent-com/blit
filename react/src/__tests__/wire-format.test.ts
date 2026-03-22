import { describe, it, expect, beforeEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useBlitConnection } from '../hooks/useBlitConnection';
import { MockTransport } from './mock-transport';
import {
  S2C_UPDATE,
  S2C_CREATED,
  S2C_CLOSED,
  S2C_LIST,
  S2C_TITLE,
  S2C_SEARCH_RESULTS,
} from '../types';

/**
 * Tests that verify the wire format parsing matches the blit protocol spec.
 * These test raw byte arrays, not the MockTransport helpers.
 */
describe('wire format parsing', () => {
  let transport: MockTransport;

  beforeEach(() => {
    transport = new MockTransport();
  });

  describe('S2C_UPDATE', () => {
    it('parses pty_id and payload', () => {
      const onUpdate = vi.fn();
      renderHook(() => useBlitConnection(transport, { onUpdate }));
      // pty_id=0x0103, payload=[0xDE, 0xAD]
      act(() => transport.push(new Uint8Array([S2C_UPDATE, 0x03, 0x01, 0xDE, 0xAD])));
      expect(onUpdate).toHaveBeenCalledWith(0x0103, expect.any(Uint8Array));
      expect(Array.from(onUpdate.mock.calls[0][1])).toEqual([0xDE, 0xAD]);
    });

    it('empty payload is valid', () => {
      const onUpdate = vi.fn();
      renderHook(() => useBlitConnection(transport, { onUpdate }));
      act(() => transport.push(new Uint8Array([S2C_UPDATE, 0x00, 0x00])));
      expect(onUpdate).toHaveBeenCalledWith(0, expect.any(Uint8Array));
      expect(onUpdate.mock.calls[0][1].length).toBe(0);
    });

    it('rejects 2-byte message', () => {
      const onUpdate = vi.fn();
      renderHook(() => useBlitConnection(transport, { onUpdate }));
      act(() => transport.push(new Uint8Array([S2C_UPDATE, 0x00])));
      expect(onUpdate).not.toHaveBeenCalled();
    });
  });

  describe('S2C_CREATED', () => {
    it('parses pty_id and tag', () => {
      const onCreated = vi.fn();
      renderHook(() => useBlitConnection(transport, { onCreated }));
      // pty_id=0x00FF, tag="hi"
      act(() => transport.push(new Uint8Array([S2C_CREATED, 0xFF, 0x00, 0x68, 0x69])));
      expect(onCreated).toHaveBeenCalledWith(0xFF, 'hi');
    });

    it('parses without tag (just pty_id)', () => {
      const onCreated = vi.fn();
      renderHook(() => useBlitConnection(transport, { onCreated }));
      act(() => transport.push(new Uint8Array([S2C_CREATED, 0x01, 0x00])));
      expect(onCreated).toHaveBeenCalledWith(1, '');
    });

    it('handles multi-byte UTF-8 tag', () => {
      const onCreated = vi.fn();
      renderHook(() => useBlitConnection(transport, { onCreated }));
      // "é" is 0xC3 0xA9 in UTF-8
      act(() => transport.push(new Uint8Array([S2C_CREATED, 0x01, 0x00, 0xC3, 0xA9])));
      expect(onCreated).toHaveBeenCalledWith(1, 'é');
    });
  });

  describe('S2C_CLOSED', () => {
    it('parses pty_id', () => {
      const onClosed = vi.fn();
      renderHook(() => useBlitConnection(transport, { onClosed }));
      act(() => transport.push(new Uint8Array([S2C_CLOSED, 0x07, 0x00])));
      expect(onClosed).toHaveBeenCalledWith(7);
    });

    it('handles high pty_id', () => {
      const onClosed = vi.fn();
      renderHook(() => useBlitConnection(transport, { onClosed }));
      act(() => transport.push(new Uint8Array([S2C_CLOSED, 0xFF, 0xFF])));
      expect(onClosed).toHaveBeenCalledWith(65535);
    });
  });

  describe('S2C_LIST', () => {
    it('parses multiple entries with tags', () => {
      const onList = vi.fn();
      renderHook(() => useBlitConnection(transport, { onList }));
      // count=2
      // entry 1: pty_id=1, tag_len=2, tag="ab"
      // entry 2: pty_id=2, tag_len=0
      act(() => transport.push(new Uint8Array([
        S2C_LIST, 0x02, 0x00,
        0x01, 0x00, 0x02, 0x00, 0x61, 0x62,
        0x02, 0x00, 0x00, 0x00,
      ])));
      expect(onList).toHaveBeenCalledWith([
        { ptyId: 1, tag: 'ab' },
        { ptyId: 2, tag: '' },
      ]);
    });

    it('parses empty list', () => {
      const onList = vi.fn();
      renderHook(() => useBlitConnection(transport, { onList }));
      act(() => transport.push(new Uint8Array([S2C_LIST, 0x00, 0x00])));
      expect(onList).toHaveBeenCalledWith([]);
    });

    it('handles truncated list gracefully', () => {
      const onList = vi.fn();
      renderHook(() => useBlitConnection(transport, { onList }));
      // count=2 but only 1 entry fits
      act(() => transport.push(new Uint8Array([
        S2C_LIST, 0x02, 0x00,
        0x01, 0x00, 0x00, 0x00,
        // second entry missing
      ])));
      expect(onList).toHaveBeenCalledWith([{ ptyId: 1, tag: '' }]);
    });

    it('handles long tags', () => {
      const onList = vi.fn();
      renderHook(() => useBlitConnection(transport, { onList }));
      const tag = 'x'.repeat(300);
      const tagBytes = new TextEncoder().encode(tag);
      const msg = new Uint8Array(3 + 4 + tagBytes.length);
      msg[0] = S2C_LIST;
      msg[1] = 1; msg[2] = 0; // count=1
      msg[3] = 0x05; msg[4] = 0x00; // pty_id=5
      msg[5] = tagBytes.length & 0xff;
      msg[6] = (tagBytes.length >> 8) & 0xff;
      msg.set(tagBytes, 7);
      act(() => transport.push(msg));
      expect(onList).toHaveBeenCalledWith([{ ptyId: 5, tag: tag }]);
    });
  });

  describe('S2C_TITLE', () => {
    it('parses pty_id and title', () => {
      const onTitle = vi.fn();
      renderHook(() => useBlitConnection(transport, { onTitle }));
      const titleBytes = new TextEncoder().encode('my-shell');
      const msg = new Uint8Array(3 + titleBytes.length);
      msg[0] = S2C_TITLE;
      msg[1] = 0x03; msg[2] = 0x00;
      msg.set(titleBytes, 3);
      act(() => transport.push(msg));
      expect(onTitle).toHaveBeenCalledWith(3, 'my-shell');
    });

    it('handles empty title', () => {
      const onTitle = vi.fn();
      renderHook(() => useBlitConnection(transport, { onTitle }));
      act(() => transport.push(new Uint8Array([S2C_TITLE, 0x01, 0x00])));
      expect(onTitle).toHaveBeenCalledWith(1, '');
    });
  });

  describe('unknown message types', () => {
    it('does not crash on unknown type', () => {
      const onUpdate = vi.fn();
      renderHook(() => useBlitConnection(transport, { onUpdate }));
      // Type 0xFF is unknown
      act(() => transport.push(new Uint8Array([0xFF, 0x01, 0x02, 0x03])));
      expect(onUpdate).not.toHaveBeenCalled();
    });
  });

  describe('message ordering', () => {
    it('processes multiple messages in order', () => {
      const calls: string[] = [];
      const onCreated = vi.fn(() => calls.push('created'));
      const onTitle = vi.fn(() => calls.push('title'));
      const onClosed = vi.fn(() => calls.push('closed'));
      renderHook(() => useBlitConnection(transport, { onCreated, onTitle, onClosed }));
      act(() => {
        transport.pushCreated(1, 'a');
        transport.pushTitle(1, 'vim');
        transport.pushClosed(1);
      });
      expect(calls).toEqual(['created', 'title', 'closed']);
    });
  });
});
