import { describe, it, expect, beforeEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useBlitSessions } from '../hooks/useBlitSessions';
import { MockTransport } from './mock-transport';
import { C2S_CREATE } from '../types';

describe('useBlitSessions — advanced scenarios', () => {
  let transport: MockTransport;

  beforeEach(() => {
    transport = new MockTransport();
  });

  // --- Rapid-fire events ---

  it('handles rapid create/close/create cycle', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([]);
      transport.pushCreated(1, 'a');
      transport.pushClosed(1);
      transport.pushCreated(2, 'b');
    });
    expect(result.current.sessions).toEqual([
      { ptyId: 1, tag: 'a', title: null, state: 'closed' },
      { ptyId: 2, tag: 'b', title: null, state: 'active' },
    ]);
  });

  it('handles duplicate CREATED for same ptyId', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([]);
      transport.pushCreated(1, 'first');
      transport.pushCreated(1, 'second');
    });
    const matches = result.current.sessions.filter((s) => s.ptyId === 1);
    expect(matches.length).toBe(1);
    expect(matches[0].state).toBe('active');
  });

  it('handles CLOSED then re-CREATED with same ptyId', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([]);
      transport.pushCreated(1, 'v1');
      transport.pushClosed(1);
      transport.pushCreated(1, 'v2');
    });
    const s = result.current.sessions.find((s) => s.ptyId === 1)!;
    expect(s.state).toBe('active');
    expect(s.tag).toBe('v2');
  });

  // --- Title updates ---

  it('title updates are independent per pty', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1, tag: '' }, { ptyId: 2, tag: '' }]);
      transport.pushTitle(1, 'vim');
      transport.pushTitle(2, 'bash');
    });
    expect(result.current.sessions[0].title).toBe('vim');
    expect(result.current.sessions[1].title).toBe('bash');
  });

  it('title can be updated multiple times', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1 }]);
      transport.pushTitle(1, 'a');
      transport.pushTitle(1, 'b');
      transport.pushTitle(1, 'c');
    });
    expect(result.current.sessions[0].title).toBe('c');
  });

  it('title can be set to empty string', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1 }]);
      transport.pushTitle(1, 'vim');
      transport.pushTitle(1, '');
    });
    expect(result.current.sessions[0].title).toBe('');
  });

  // --- LIST reconciliation edge cases ---

  it('LIST with same entries is idempotent', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }, { ptyId: 2, tag: 'b' }]));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }, { ptyId: 2, tag: 'b' }]));
    expect(result.current.sessions.map((s) => s.ptyId)).toEqual([1, 2]);
    expect(result.current.sessions.every((s) => s.state === 'active')).toBe(true);
  });

  it('LIST after all closed re-activates', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1 }]);
      transport.pushClosed(1);
    });
    expect(result.current.sessions[0].state).toBe('closed');
    act(() => transport.pushList([{ ptyId: 1 }]));
  });

  it('empty LIST marks everything closed', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1 }, { ptyId: 2 }, { ptyId: 3 }]);
    });
    act(() => transport.pushList([]));
    expect(result.current.sessions.every((s) => s.state === 'closed')).toBe(true);
  });

  // --- High pty IDs ---

  it('handles high pty IDs (u16 range)', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([]);
      transport.pushCreated(65535, 'max');
      transport.pushCreated(256, 'mid');
    });
    expect(result.current.sessions.find((s) => s.ptyId === 65535)?.tag).toBe('max');
    expect(result.current.sessions.find((s) => s.ptyId === 256)?.tag).toBe('mid');
  });

  // --- Many sessions ---

  it('handles 100 sessions', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    const entries = Array.from({ length: 100 }, (_, i) => ({
      ptyId: i,
      tag: `tag-${i}`,
    }));
    act(() => transport.pushList(entries));
    expect(result.current.sessions.length).toBe(100);
    expect(result.current.sessions[50].tag).toBe('tag-50');
    expect(result.current.sessions.every((s) => s.state === 'active')).toBe(true);
  });

  // --- Transport disconnect/reconnect ---

  it('sessions persist across transport disconnect', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }]));
    act(() => transport.setStatus('disconnected'));
    expect(result.current.sessions.length).toBe(1);
    expect(result.current.sessions[0].state).toBe('active');
  });

  it('sessions reconcile on reconnect LIST', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1, tag: 'a' }, { ptyId: 2, tag: 'b' }]);
      transport.pushTitle(1, 'vim');
    });
    act(() => transport.setStatus('disconnected'));
    act(() => transport.setStatus('connected'));
    act(() => transport.pushList([{ ptyId: 2, tag: 'b' }, { ptyId: 3, tag: 'c' }]));
    expect(result.current.sessions.find((s) => s.ptyId === 1)?.state).toBe('closed');
    expect(result.current.sessions.find((s) => s.ptyId === 2)?.state).toBe('active');
    expect(result.current.sessions.find((s) => s.ptyId === 3)?.state).toBe('active');
    expect(result.current.sessions.find((s) => s.ptyId === 1)?.title).toBe('vim');
  });

  // --- getInitialSize ---

  it('autoCreate uses getInitialSize', () => {
    renderHook(() =>
      useBlitSessions(transport, {
        autoCreateIfEmpty: true,
        getInitialSize: () => ({ rows: 40, cols: 160 }),
      }),
    );
    act(() => transport.pushList([]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(1);
    expect(creates[0][1] | (creates[0][2] << 8)).toBe(40);
    expect(creates[0][3] | (creates[0][4] << 8)).toBe(160);
  });

  // --- Unicode stress ---

  it('handles emoji tags and titles', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushList([{ ptyId: 1, tag: '🚀🔥' }]);
      transport.pushTitle(1, '💻 terminal — ñoño');
    });
    expect(result.current.sessions[0].tag).toBe('🚀🔥');
    expect(result.current.sessions[0].title).toBe('💻 terminal — ñoño');
  });

  it('handles CJK tags', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: '日本語ターミナル' }]));
    expect(result.current.sessions[0].tag).toBe('日本語ターミナル');
  });

  // --- Stability ---

  it('ready stays true after multiple LISTs', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([]));
    expect(result.current.ready).toBe(true);
    act(() => transport.pushList([{ ptyId: 1 }]));
    expect(result.current.ready).toBe(true);
    act(() => transport.pushList([]));
    expect(result.current.ready).toBe(true);
  });

  it('operations before LIST are safe', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => {
      transport.pushCreated(1, 'early');
      transport.pushTitle(1, 'title');
      transport.pushClosed(99);
    });
    expect(result.current.ready).toBe(false);
    expect(result.current.sessions.length).toBe(1);
    expect(result.current.sessions[0].title).toBe('title');
  });

  // --- focusedPtyId advanced ---

  it('focusedPtyId survives LIST reconciliation when pty still exists', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }, { ptyId: 2 }, { ptyId: 3 }]));
    act(() => result.current.focusPty(2));
    act(() => transport.pushList([{ ptyId: 2 }, { ptyId: 3 }]));
    expect(result.current.focusedPtyId).toBe(2);
  });

  it('focusedPtyId falls back when focused pty removed from LIST', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }, { ptyId: 2 }]));
    act(() => result.current.focusPty(1));
    act(() => transport.pushList([{ ptyId: 2 }]));
    expect(result.current.focusedPtyId).toBe(2);
  });
});
