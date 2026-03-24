import { describe, it, expect, beforeEach, vi } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import { useBlitSessions } from '../hooks/useBlitSessions';
import { MockTransport } from './mock-transport';
import { C2S_CREATE, C2S_CREATE_N, C2S_FOCUS, C2S_CLOSE, FEATURE_CREATE_NONCE } from '../types';

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

  // --- autoCreate ---

  it('autoCreateIfEmpty sends CREATE on empty LIST with default tag', () => {
    renderHook(() =>
      useBlitSessions(transport, { autoCreateIfEmpty: true }),
    );
    act(() => transport.pushList([]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(1);
    expect(creates[0].length).toBe(7);
    expect(creates[0][5]).toBe(0);
    expect(creates[0][6]).toBe(0);
  });

  it('autoCreateIfEmpty sends CREATE with autoCreateTag', () => {
    renderHook(() =>
      useBlitSessions(transport, {
        autoCreateIfEmpty: true,
        autoCreateTag: 'interactive',
      }),
    );
    act(() => transport.pushList([]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(1);
    const tagLen = creates[0][5] | (creates[0][6] << 8);
    expect(tagLen).toBe(11);
    const tag = new TextDecoder().decode(creates[0].slice(7, 7 + tagLen));
    expect(tag).toBe('interactive');
  });

  it('autoCreateIfEmpty sends CREATE with autoCreateCommand', () => {
    renderHook(() =>
      useBlitSessions(transport, {
        autoCreateIfEmpty: true,
        autoCreateTag: 'bg',
        autoCreateCommand: 'make build',
      }),
    );
    act(() => transport.pushList([]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(1);
    const tagLen = creates[0][5] | (creates[0][6] << 8);
    const tag = new TextDecoder().decode(creates[0].slice(7, 7 + tagLen));
    expect(tag).toBe('bg');
    const cmd = new TextDecoder().decode(creates[0].slice(7 + tagLen));
    expect(cmd).toBe('make build');
  });

  it('does not auto-create when LIST is non-empty', () => {
    renderHook(() =>
      useBlitSessions(transport, { autoCreateIfEmpty: true }),
    );
    act(() => transport.pushList([{ ptyId: 1 }]));
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE);
    expect(creates.length).toBe(0);
  });

  // --- CREATED / CLOSED ---

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

  // --- Titles ---

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

  // --- LIST reconciliation ---

  it('reconciles LIST — marks missing PTYs as closed, adds new', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1, tag: 'a' }, { ptyId: 2, tag: 'b' }]));
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

  // --- createPty ---

  it('createPty sends C2S_CREATE_N with nonce and default size after hello', () => {
    const { result } = renderHook(() =>
      useBlitSessions(transport, {
        getInitialSize: () => ({ rows: 30, cols: 120 }),
      }),
    );
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    act(() => { result.current.createPty(); });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE_N)!;
    expect(msg).toBeDefined();
    expect(msg[3] | (msg[4] << 8)).toBe(30);
    expect(msg[5] | (msg[6] << 8)).toBe(120);
  });

  it('createPty sends C2S_CREATE_N with tag and command after hello', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    act(() => { result.current.createPty({ tag: 'my-tag', command: 'vim' }); });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE_N)!;
    expect(msg).toBeDefined();
    const tagLen = msg[7] | (msg[8] << 8);
    expect(tagLen).toBe(6);
    const tag = new TextDecoder().decode(msg.slice(9, 9 + tagLen));
    expect(tag).toBe('my-tag');
    const cmd = new TextDecoder().decode(msg.slice(9 + tagLen));
    expect(cmd).toBe('vim');
  });

  it('createPty with custom rows/cols after hello', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    act(() => { result.current.createPty({ rows: 50, cols: 200 }); });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE_N)!;
    expect(msg[3] | (msg[4] << 8)).toBe(50);
    expect(msg[5] | (msg[6] << 8)).toBe(200);
  });

  it('createPty sends C2S_CREATE without hello (old server)', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => { result.current.createPty({ tag: 'test' }); });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE)!;
    expect(msg).toBeDefined();
    expect(transport.sent.find((m) => m[0] === C2S_CREATE_N)).toBeUndefined();
  });

  it('createPty sends C2S_CREATE when hello has no FEATURE_CREATE_NONCE', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, 0));
    act(() => { result.current.createPty({ tag: 'test' }); });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE)!;
    expect(msg).toBeDefined();
    expect(transport.sent.find((m) => m[0] === C2S_CREATE_N)).toBeUndefined();
  });

  it('createPty returns a promise that resolves with ptyId via S2C_CREATED_N', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    let resolved: number | undefined;
    act(() => {
      result.current.createPty({ tag: 'test' }).then((id) => { resolved = id; });
    });
    expect(resolved).toBeUndefined();
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE_N)!;
    const nonce = msg[1] | (msg[2] << 8);
    await act(async () => { transport.pushCreatedN(nonce, 42, 'test'); });
    expect(resolved).toBe(42);
  });

  it('createPty promises resolve by nonce matching', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    const ids: number[] = [];
    act(() => {
      result.current.createPty({ tag: 'a' }).then((id) => ids.push(id));
      result.current.createPty({ tag: 'b' }).then((id) => ids.push(id));
    });
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE_N);
    const nonce1 = creates[0][1] | (creates[0][2] << 8);
    const nonce2 = creates[1][1] | (creates[1][2] << 8);
    await act(async () => {
      transport.pushCreatedN(nonce1, 10, 'a');
      transport.pushCreatedN(nonce2, 20, 'b');
    });
    expect(ids).toEqual([10, 20]);
  });

  it('S2C_CREATED_N resolves by nonce, ignoring unrelated S2C_CREATED', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    const ids: number[] = [];
    act(() => {
      result.current.createPty({ tag: 'a' }).then((id) => ids.push(id));
      result.current.createPty({ tag: 'b' }).then((id) => ids.push(id));
    });
    const creates = transport.sent.filter((m) => m[0] === C2S_CREATE_N);
    const nonce2 = creates[1][1] | (creates[1][2] << 8);
    await act(async () => {
      transport.pushCreatedN(nonce2, 20, 'b');
    });
    expect(ids).toEqual([20]);
    const nonce1 = creates[0][1] | (creates[0][2] << 8);
    await act(async () => {
      transport.pushCreatedN(nonce1, 10, 'a');
    });
    expect(ids).toEqual([20, 10]);
  });

  it('createPty promise resolves with -1 on disconnect', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    let resolved: number | undefined;
    act(() => {
      result.current.createPty({ tag: 'test' }).then((id) => { resolved = id; });
    });
    expect(resolved).toBeUndefined();
    await act(async () => { transport.setStatus('disconnected'); });
    expect(resolved).toBe(-1);
  });

  it('closePty promise resolves on disconnect', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 7, tag: '' }]));
    let resolved = false;
    act(() => {
      result.current.closePty(7).then(() => { resolved = true; });
    });
    expect(resolved).toBe(false);
    await act(async () => { transport.setStatus('disconnected'); });
    expect(resolved).toBe(true);
  });

  it('createPty falls back to FIFO via S2C_CREATED against old servers', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    let resolved: number | undefined;
    act(() => {
      result.current.createPty({ tag: 'test' }).then((id) => { resolved = id; });
    });
    expect(resolved).toBeUndefined();
    await act(async () => { transport.pushCreated(42, 'test'); });
    expect(resolved).toBe(42);
  });

  it('createPty FIFO fallback resolves in order against old servers', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    const ids: number[] = [];
    act(() => {
      result.current.createPty({ tag: 'a' }).then((id) => ids.push(id));
      result.current.createPty({ tag: 'b' }).then((id) => ids.push(id));
    });
    await act(async () => {
      transport.pushCreated(10, 'a');
      transport.pushCreated(20, 'b');
    });
    expect(ids).toEqual([10, 20]);
  });

  // --- closePty ---

  it('closePty sends CLOSE', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => { result.current.closePty(3); });
    const msg = transport.sent.find((m) => m[0] === C2S_CLOSE)!;
    expect(msg).toBeDefined();
    expect(msg[1] | (msg[2] << 8)).toBe(3);
  });

  it('closePty returns a promise that resolves on S2C_CLOSED', async () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 7, tag: '' }]));
    let resolved = false;
    act(() => {
      result.current.closePty(7).then(() => { resolved = true; });
    });
    expect(resolved).toBe(false);
    await act(async () => { transport.pushClosed(7); });
    expect(resolved).toBe(true);
  });

  // --- focusPty / focusedPtyId ---

  it('focusPty sends FOCUS and updates focusedPtyId', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }, { ptyId: 2 }]));
    act(() => result.current.focusPty(2));
    const msg = transport.sent.find((m) => m[0] === C2S_FOCUS)!;
    expect(msg).toBeDefined();
    expect(msg[1] | (msg[2] << 8)).toBe(2);
    expect(result.current.focusedPtyId).toBe(2);
  });

  it('focusedPtyId auto-selects first entry from LIST', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 5, tag: 'a' }, { ptyId: 6, tag: 'b' }]));
    expect(result.current.focusedPtyId).toBe(5);
  });

  it('focusedPtyId is null for empty LIST', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([]));
    expect(result.current.focusedPtyId).toBeNull();
  });

  it('focusedPtyId moves to next active on CLOSED', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }, { ptyId: 2 }]));
    act(() => result.current.focusPty(1));
    expect(result.current.focusedPtyId).toBe(1);
    act(() => transport.pushClosed(1));
    expect(result.current.focusedPtyId).toBe(2);
  });

  it('focusedPtyId becomes null when all sessions close', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushList([{ ptyId: 1 }]));
    act(() => transport.pushClosed(1));
    expect(result.current.focusedPtyId).toBeNull();
  });

  // --- Status tracking ---

  it('reflects transport status', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    expect(result.current.status).toBe('connected');
    act(() => transport.setStatus('disconnected'));
    expect(result.current.status).toBe('disconnected');
  });

  // --- Lifecycle callbacks ---

  it('calls onSessionCreated when a session is created', () => {
    const onSessionCreated = vi.fn();
    renderHook(() =>
      useBlitSessions(transport, { onSessionCreated }),
    );
    act(() => transport.pushList([]));
    act(() => transport.pushCreated(3, 'shell'));
    expect(onSessionCreated).toHaveBeenCalledWith(
      expect.objectContaining({ ptyId: 3, tag: 'shell', state: 'active' }),
    );
  });

  it('calls onSessionClosed when a session is closed', () => {
    const onSessionClosed = vi.fn();
    renderHook(() =>
      useBlitSessions(transport, { onSessionClosed }),
    );
    act(() => transport.pushList([{ ptyId: 1, tag: 'x' }]));
    act(() => transport.pushClosed(1));
    expect(onSessionClosed).toHaveBeenCalledWith(
      expect.objectContaining({ ptyId: 1, state: 'closed' }),
    );
  });

  it('calls onDisconnect when transport disconnects after being connected', () => {
    const onDisconnect = vi.fn();
    renderHook(() =>
      useBlitSessions(transport, { onDisconnect }),
    );
    act(() => transport.setStatus('disconnected'));
    expect(onDisconnect).toHaveBeenCalledTimes(1);
  });

  it('calls onReconnect when transport reconnects (not on initial connect)', () => {
    const onReconnect = vi.fn();
    const onDisconnect = vi.fn();
    renderHook(() =>
      useBlitSessions(transport, { onReconnect, onDisconnect }),
    );
    act(() => transport.setStatus('disconnected'));
    act(() => transport.setStatus('connected'));
    expect(onDisconnect).toHaveBeenCalledTimes(1);
    expect(onReconnect).toHaveBeenCalledTimes(1);
  });

  it('does not call onReconnect on initial connect for initially-disconnected transport', () => {
    const disconnectedTransport = new MockTransport('disconnected');
    const onReconnect = vi.fn();
    renderHook(() =>
      useBlitSessions(disconnectedTransport, { onReconnect }),
    );
    act(() => disconnectedTransport.setStatus('connected'));
    expect(onReconnect).not.toHaveBeenCalled();
  });

  it('calls onReconnect after initial connect + disconnect + reconnect', () => {
    const disconnectedTransport = new MockTransport('disconnected');
    const onReconnect = vi.fn();
    renderHook(() =>
      useBlitSessions(disconnectedTransport, { onReconnect }),
    );
    act(() => disconnectedTransport.setStatus('connected'));
    expect(onReconnect).not.toHaveBeenCalled();
    act(() => disconnectedTransport.setStatus('disconnected'));
    act(() => disconnectedTransport.setStatus('connected'));
    expect(onReconnect).toHaveBeenCalledTimes(1);
  });

  // --- S2C_HELLO version negotiation ---

  it('closes transport on hello with version > PROTOCOL_VERSION', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(2, FEATURE_CREATE_NONCE));
    expect(result.current.status).toBe('disconnected');
  });

  it('accepts hello with version 1', () => {
    const { result } = renderHook(() => useBlitSessions(transport));
    act(() => transport.pushHello(1, FEATURE_CREATE_NONCE));
    expect(result.current.status).toBe('connected');
  });

  // --- WASM title reading via getTerminal ---

  it('reads title from getTerminal on S2C_UPDATE', async () => {
    const terminal = { title: () => 'zsh: ~/project' };
    const { result } = renderHook(() =>
      useBlitSessions(transport, {
        getTerminal: (ptyId) => (ptyId === 1 ? terminal : null),
      }),
    );
    act(() => transport.pushList([{ ptyId: 1, tag: '' }]));
    await act(async () => transport.pushUpdate(1));
    expect(result.current.sessions[0].title).toBe('zsh: ~/project');
  });

  it('does not update title when getTerminal returns null', async () => {
    const { result } = renderHook(() =>
      useBlitSessions(transport, {
        getTerminal: () => null,
      }),
    );
    act(() => transport.pushList([{ ptyId: 1, tag: '' }]));
    act(() => transport.pushTitle(1, 'explicit'));
    await act(async () => transport.pushUpdate(1));
    expect(result.current.sessions[0].title).toBe('explicit');
  });

  it('deduplicates title updates from getTerminal', async () => {
    let renderCount = 0;
    const terminal = { title: () => 'same-title' };
    const { result } = renderHook(() => {
      renderCount++;
      return useBlitSessions(transport, {
        getTerminal: () => terminal,
      });
    });
    act(() => transport.pushList([{ ptyId: 1, tag: '' }]));
    await act(async () => transport.pushUpdate(1));
    const countAfterFirstUpdate = renderCount;
    await act(async () => transport.pushUpdate(1));
    expect(renderCount).toBe(countAfterFirstUpdate);
    expect(result.current.sessions[0].title).toBe('same-title');
  });
});
