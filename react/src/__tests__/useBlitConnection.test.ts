import { describe, it, expect, beforeEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useBlitConnection } from '../hooks/useBlitConnection';
import { MockTransport } from './mock-transport';
import {
  C2S_INPUT,
  C2S_RESIZE,
  C2S_SCROLL,
  C2S_ACK,
  C2S_CREATE,
  C2S_FOCUS,
  C2S_CLOSE,
  C2S_SUBSCRIBE,
  C2S_UNSUBSCRIBE,
  S2C_UPDATE,
} from '../types';

describe('useBlitConnection', () => {
  let transport: MockTransport;

  beforeEach(() => {
    transport = new MockTransport();
  });

  it('starts with connected status', () => {
    const { result } = renderHook(() =>
      useBlitConnection(transport, {}),
    );
    expect(result.current.status).toBe('connected');
  });

  it('tracks status changes', () => {
    const { result } = renderHook(() =>
      useBlitConnection(transport, {}),
    );
    act(() => transport.setStatus('disconnected'));
    expect(result.current.status).toBe('disconnected');
  });

  // --- Callback dispatching ---

  it('dispatches onCreated with tag', () => {
    const onCreated = vi.fn();
    renderHook(() => useBlitConnection(transport, { onCreated }));
    act(() => transport.pushCreated(5, 'hello'));
    expect(onCreated).toHaveBeenCalledWith(5, 'hello');
  });

  it('dispatches onCreated with empty tag', () => {
    const onCreated = vi.fn();
    renderHook(() => useBlitConnection(transport, { onCreated }));
    act(() => transport.pushCreated(1));
    expect(onCreated).toHaveBeenCalledWith(1, '');
  });

  it('dispatches onClosed', () => {
    const onClosed = vi.fn();
    renderHook(() => useBlitConnection(transport, { onClosed }));
    act(() => transport.pushClosed(3));
    expect(onClosed).toHaveBeenCalledWith(3);
  });

  it('dispatches onList with tags', () => {
    const onList = vi.fn();
    renderHook(() => useBlitConnection(transport, { onList }));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }, { ptyId: 2, tag: 'b' }]));
    expect(onList).toHaveBeenCalledWith([
      { ptyId: 1, tag: 'a' },
      { ptyId: 2, tag: 'b' },
    ]);
  });

  it('dispatches onList with empty list', () => {
    const onList = vi.fn();
    renderHook(() => useBlitConnection(transport, { onList }));
    act(() => transport.pushList([]));
    expect(onList).toHaveBeenCalledWith([]);
  });

  it('dispatches onTitle', () => {
    const onTitle = vi.fn();
    renderHook(() => useBlitConnection(transport, { onTitle }));
    act(() => transport.pushTitle(7, 'vim'));
    expect(onTitle).toHaveBeenCalledWith(7, 'vim');
  });

  it('dispatches onUpdate', () => {
    const onUpdate = vi.fn();
    renderHook(() => useBlitConnection(transport, { onUpdate }));
    const payload = new Uint8Array([S2C_UPDATE, 0x02, 0x00, 0xAA, 0xBB]);
    act(() => transport.push(payload));
    expect(onUpdate).toHaveBeenCalledWith(2, expect.any(Uint8Array));
    const p = onUpdate.mock.calls[0][1] as Uint8Array;
    expect(p[0]).toBe(0xAA);
    expect(p[1]).toBe(0xBB);
  });

  it('ignores too-short messages', () => {
    const onCreated = vi.fn();
    renderHook(() => useBlitConnection(transport, { onCreated }));
    // S2C_CREATED with only 1 byte after type
    act(() => transport.push(new Uint8Array([0x01, 0x05])));
    expect(onCreated).not.toHaveBeenCalled();
  });

  it('ignores empty messages', () => {
    const onUpdate = vi.fn();
    renderHook(() => useBlitConnection(transport, { onUpdate }));
    act(() => transport.push(new Uint8Array([])));
    expect(onUpdate).not.toHaveBeenCalled();
  });

  // --- Send helpers ---

  it('sendAck sends ACK', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendAck());
    expect(transport.sent[0]).toEqual(new Uint8Array([C2S_ACK]));
  });

  it('sendInput sends INPUT with ptyId and data', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendInput(3, new Uint8Array([0x6c, 0x73])));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_INPUT);
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3]).toBe(0x6c);
    expect(msg[4]).toBe(0x73);
  });

  it('sendResize sends RESIZE', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendResize(1, 24, 80));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg[1] | (msg[2] << 8)).toBe(1);
    expect(msg[3] | (msg[4] << 8)).toBe(24);
    expect(msg[5] | (msg[6] << 8)).toBe(80);
  });

  it('sendScroll sends SCROLL', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendScroll(2, 100));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_SCROLL);
    expect(msg[1] | (msg[2] << 8)).toBe(2);
    const offset = msg[3] | (msg[4] << 8) | (msg[5] << 16) | (msg[6] << 24);
    expect(offset).toBe(100);
  });

  it('sendCreate sends CREATE with tag and command', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendCreate(24, 80, { tag: 'x', command: 'ls' }));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_CREATE);
    expect(msg[1] | (msg[2] << 8)).toBe(24);
    expect(msg[3] | (msg[4] << 8)).toBe(80);
    const tagLen = msg[5] | (msg[6] << 8);
    expect(tagLen).toBe(1);
    expect(new TextDecoder().decode(msg.slice(7, 8))).toBe('x');
    expect(new TextDecoder().decode(msg.slice(8))).toBe('ls');
  });

  it('sendCreate sends CREATE without tag/command', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendCreate(24, 80));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_CREATE);
    expect(msg[5] | (msg[6] << 8)).toBe(0); // tag_len = 0
    expect(msg.length).toBe(7); // no command
  });

  it('sendFocus sends FOCUS', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendFocus(9));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_FOCUS);
    expect(msg[1] | (msg[2] << 8)).toBe(9);
  });

  it('sendClose sends CLOSE', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendClose(4));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_CLOSE);
    expect(msg[1] | (msg[2] << 8)).toBe(4);
  });

  it('sendSubscribe sends SUBSCRIBE', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendSubscribe(6));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_SUBSCRIBE);
    expect(msg[1] | (msg[2] << 8)).toBe(6);
  });

  it('sendUnsubscribe sends UNSUBSCRIBE', () => {
    const { result } = renderHook(() => useBlitConnection(transport, {}));
    act(() => result.current.sendUnsubscribe(6));
    const msg = transport.sent[0];
    expect(msg[0]).toBe(C2S_UNSUBSCRIBE);
    expect(msg[1] | (msg[2] << 8)).toBe(6);
  });

  // --- Unicode tags and titles ---

  it('handles unicode tag in CREATED', () => {
    const onCreated = vi.fn();
    renderHook(() => useBlitConnection(transport, { onCreated }));
    act(() => transport.pushCreated(1, '日本語'));
    expect(onCreated).toHaveBeenCalledWith(1, '日本語');
  });

  it('handles unicode title in TITLE', () => {
    const onTitle = vi.fn();
    renderHook(() => useBlitConnection(transport, { onTitle }));
    act(() => transport.pushTitle(1, 'émacs'));
    expect(onTitle).toHaveBeenCalledWith(1, 'émacs');
  });

  it('handles LIST with unicode tags', () => {
    const onList = vi.fn();
    renderHook(() => useBlitConnection(transport, { onList }));
    act(() => transport.pushList([{ ptyId: 1, tag: '🚀' }]));
    expect(onList).toHaveBeenCalledWith([{ ptyId: 1, tag: '🚀' }]);
  });
});
