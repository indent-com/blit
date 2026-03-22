import { useCallback, useRef, useSyncExternalStore } from 'react';
import type { BlitTransport, BlitSession } from '../types';
import {
  C2S_CREATE,
  C2S_FOCUS,
  C2S_CLOSE,
  S2C_CREATED,
  S2C_CLOSED,
  S2C_LIST,
  S2C_TITLE,
} from '../types';
import { useBlitConnection } from './useBlitConnection';

export interface UseBlitSessionsOptions {
  /** Automatically create a PTY if the initial list is empty. Default: false. */
  autoCreateIfEmpty?: boolean;
  /** Returns the initial terminal size for auto-created PTYs. */
  getInitialSize?: () => { rows: number; cols: number };
}

export function useBlitSessions(
  transport: BlitTransport,
  options?: UseBlitSessionsOptions,
) {
  const autoCreate = options?.autoCreateIfEmpty ?? false;
  const getSize = options?.getInitialSize ?? (() => ({ rows: 24, cols: 80 }));

  // Immutable snapshot for useSyncExternalStore.
  const sessionsRef = useRef<readonly BlitSession[]>([]);
  const listenersRef = useRef(new Set<() => void>());
  const readyRef = useRef(false);
  const readyListenersRef = useRef(new Set<() => void>());

  const notify = useCallback(() => {
    for (const l of listenersRef.current) l();
  }, []);

  const notifyReady = useCallback(() => {
    for (const l of readyListenersRef.current) l();
  }, []);

  const upsert = useCallback(
    (ptyId: number, tag: string, patch: Partial<BlitSession>) => {
      const prev = sessionsRef.current;
      const idx = prev.findIndex((s) => s.ptyId === ptyId);
      if (idx >= 0) {
        const updated = { ...prev[idx], tag, ...patch };
        const next = [...prev];
        next[idx] = updated;
        sessionsRef.current = next;
      } else {
        sessionsRef.current = [
          ...prev,
          { ptyId, tag, title: null, state: 'active' as const, ...patch },
        ];
      }
      notify();
    },
    [notify],
  );

  // --- Connection callbacks ---

  const onCreated = useCallback(
    (ptyId: number, tag: string) => {
      upsert(ptyId, tag, { state: 'active' });
    },
    [upsert],
  );

  const onClosed = useCallback(
    (closedId: number) => {
      const prev = sessionsRef.current;
      const idx = prev.findIndex((s) => s.ptyId === closedId);
      if (idx >= 0) {
        const next = [...prev];
        next[idx] = { ...next[idx], state: 'closed' };
        sessionsRef.current = next;
        notify();
      }
    },
    [notify],
  );

  const onList = useCallback(
    (entries: { ptyId: number; tag: string }[]) => {
      // Reconcile: mark missing as closed, add new as active.
      const ids = new Set(entries.map((e) => e.ptyId));
      let next = sessionsRef.current
        .map((s) => (ids.has(s.ptyId) ? s : { ...s, state: 'closed' as const }));
      for (const { ptyId, tag } of entries) {
        if (!next.some((s) => s.ptyId === ptyId)) {
          next = [...next, { ptyId, tag, title: null, state: 'active' as const }];
        }
      }
      sessionsRef.current = next;

      if (!readyRef.current) {
        readyRef.current = true;
        notifyReady();
      }
      notify();

      if (autoCreate && entries.length === 0) {
        const { rows, cols } = getSize();
        const encoder = new TextEncoder();
        const msg = new Uint8Array(7);
        msg[0] = C2S_CREATE;
        msg[1] = rows & 0xff;
        msg[2] = (rows >> 8) & 0xff;
        msg[3] = cols & 0xff;
        msg[4] = (cols >> 8) & 0xff;
        // tag_len = 0
        msg[5] = 0;
        msg[6] = 0;
        transport.send(msg);
      }
    },
    [notify, notifyReady, autoCreate, getSize, transport],
  );

  const onTitle = useCallback(
    (ptyId: number, title: string) => {
      const prev = sessionsRef.current;
      const idx = prev.findIndex((s) => s.ptyId === ptyId);
      if (idx >= 0) {
        const next = [...prev];
        next[idx] = { ...next[idx], title };
        sessionsRef.current = next;
        notify();
      }
    },
    [notify],
  );

  const { status, sendCreate, sendFocus, sendClose } = useBlitConnection(
    transport,
    { onCreated, onClosed, onList, onTitle },
  );

  // --- Public API ---

  const sessions = useSyncExternalStore(
    useCallback(
      (l: () => void) => {
        listenersRef.current.add(l);
        return () => listenersRef.current.delete(l);
      },
      [],
    ),
    useCallback(() => sessionsRef.current, []),
    useCallback(() => sessionsRef.current, []),
  );

  const ready = useSyncExternalStore(
    useCallback(
      (l: () => void) => {
        readyListenersRef.current.add(l);
        return () => readyListenersRef.current.delete(l);
      },
      [],
    ),
    useCallback(() => readyRef.current, []),
    useCallback(() => readyRef.current, []),
  );

  const createPty = useCallback(
    (opts?: { rows?: number; cols?: number; command?: string; tag?: string }) => {
      const { rows, cols } = getSize();
      sendCreate(opts?.rows ?? rows, opts?.cols ?? cols, {
        tag: opts?.tag,
        command: opts?.command,
      });
    },
    [sendCreate, getSize],
  );

  const focusPty = useCallback(
    (ptyId: number) => {
      sendFocus(ptyId);
    },
    [sendFocus],
  );

  const closePty = useCallback(
    (ptyId: number) => {
      sendClose(ptyId);
    },
    [sendClose],
  );

  return { ready, sessions, status, createPty, focusPty, closePty } as const;
}
