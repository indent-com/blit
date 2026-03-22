import { describe, it, expect, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useBlitSessions } from '../hooks/useBlitSessions';
import { MockTransport } from './mock-transport';
import { C2S_CREATE, C2S_FOCUS, C2S_CLOSE } from '../types';

describe('useBlitSessions', () => {
  let transport: MockTransport;

  beforeEach(() => {
    transport = new MockTransport();
  });

  it('starts not ready with empty sessions', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    expect(result.current.ready).toBe(false);
    expect(result.current.sessions).toEqual([]);
  });

  it('becomes ready after LIST', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }]));
    expect(result.current.ready).toBe(true);
    expect(result.current.sessions).toEqual([
      { ptyId: 1, tag: 'a', title: null, state: 'active' },
    ]);
  });

  it('handles empty LIST', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([]));
    expect(result.current.ready).toBe(true);
    expect(result.current.sessions).toEqual([]);
  });

  it('autoCreateIfEmpty sends CREATE on empty LIST', () => {
    renderHook(() =>
      useBlitSessions(transport, { autoCreateIfEmpty: true }),
    );
    act(() => transport.pushList([]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(1);
    // Verify it's a valid CREATE message with tag_len=0
    expect(creates[0].length).toBe(7);
    expect(creates[0][5]).toBe(0); // tag_len lo
    expect(creates[0][6]).toBe(0); // tag_len hi
  });

  it('does not auto-create when LIST is non-empty', () => {
    renderHook(() =>
      useBlitSessions(transport, { autoCreateIfEmpty: true }),
    );
    act(() => transport.pushList([{ ptyId: 1 }]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(0);
  });

  it('tracks CREATED', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([]));
    act(() => transport.pushCreated(5, 'editor'));
    expect(result.current.sessions).toEqual([
      { ptyId: 5, tag: 'editor', title: null, state: 'active' },
    ]);
  });

  it('tracks CREATED with empty tag', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([]));
    act(() => transport.pushCreated(1));
    expect(result.current.sessions[0].tag).toBe('');
  });

  it('marks session closed on CLOSED', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: 'x' }]));
    act(() => transport.pushClosed(1));
    expect(result.current.sessions).toEqual([
      { ptyId: 1, tag: 'x', title: null, state: 'closed' },
    ]);
  });

  it('ignores CLOSED for unknown ptyId', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }]));
    act(() => transport.pushClosed(99));
    expect(result.current.sessions.length).toBe(1);
    expect(result.current.sessions[0].state).toBe('active');
  });

  it('updates title on TITLE', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: '' }]));
    act(() => transport.pushTitle(1, 'bash'));
    expect(result.current.sessions[0].title).toBe('bash');
  });

  it('ignores TITLE for unknown ptyId', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }]));
    act(() => transport.pushTitle(99, 'nope'));
    expect(result.current.sessions[0].title).toBeNull();
  });

  it('reconciles LIST — marks missing PTYs as closed, adds new', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }, { ptyId: 2, tag: 'b' }]));
    // Second LIST: pty 1 gone, pty 3 added
    act(() => transport.pushList([{ ptyId: 2, tag: 'b' }, { ptyId: 3, tag: 'c' }]));
    const s = result.current.sessions;
    expect(s.find((x) => x.ptyId === 1)?.state).toBe('closed');
    expect(s.find((x) => x.ptyId === 2)?.state).toBe('active');
    expect(s.find((x) => x.ptyId === 3)?.state).toBe('active');
  });

  it('preserves title across LIST reconciliation', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: '' }]));
    act(() => transport.pushTitle(1, 'vim'));
    act(() => transport.pushList([{ ptyId: 1, tag: '' }]));
    expect(result.current.sessions[0].title).toBe('vim');
  });

  it('multiple CREATED accumulate', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([]);
      transport.pushCreated(1, 'a');
      transport.pushCreated(2, 'b');
      transport.pushCreated(3, 'c');
    });
    expect(result.current.sessions.map((s) => s.ptyId)).toEqual([1, 2, 3]);
  });

  // --- Control functions ---

  it('createPty sends CREATE with default size', () => {
    const { result } = renderHook(() =>
      useBlitSessions(transport, {
        getInitialSize: () => ({ rows: 30, cols: 120 }),
      }),
    );
    act(() => result.current.createPty());
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE)!;
    expect(msg).toBeDefined();
    expect(msg[1] | (msg[2] << 8)).toBe(30);
    expect(msg[3] | (msg[4] << 8)).toBe(120);
  });

  it('createPty sends CREATE with tag and command', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => result.current.createPty({ tag: 'my-tag', command: 'vim' }));
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE)!;
    expect(msg).toBeDefined();
    const tagLen = msg[5] | (msg[6] << 8);
    expect(tagLen).toBe(6);
    const tag = new TextDecoder().decode(msg.slice(7, 7 + tagLen));
    expect(tag).toBe('my-tag');
    const cmd = new TextDecoder().decode(msg.slice(7 + tagLen));
    expect(cmd).toBe('vim');
  });

  it('createPty with custom rows/cols', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => result.current.createPty({ rows: 50, cols: 200 }));
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE)!;
    expect(msg[1] | (msg[2] << 8)).toBe(50);
    expect(msg[3] | (msg[4] << 8)).toBe(200);
  });

  it('focusPty sends FOCUS', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => result.current.focusPty(7));
    const msg = transport.sent.find((m) => m[0] === C2S_FOCUS)!;
    expect(msg).toBeDefined();
    expect(msg[1] | (msg[2] << 8)).toBe(7);
  });

  it('closePty sends CLOSE', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => result.current.closePty(3));
    const msg = transport.sent.find((m) => m[0] === C2S_CLOSE)!;
    expect(msg).toBeDefined();
    expect(msg[1] | (msg[2] << 8)).toBe(3);
  });

  // --- Status tracking ---

  it('reflects transport status', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    expect(result.current.status).toBe('connected');
    act(() => transport.setStatus('disconnected'));
    expect(result.current.status).toBe('disconnected');
  });
});
