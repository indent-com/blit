import { useCallback, useRef, useSyncExternalStore } from "react";
import type { BlitTransport, BlitSession, ConnectionStatus } from "../types";
import { FEATURE_CREATE_NONCE, PROTOCOL_VERSION } from "../types";
import { useBlitConnection, type SearchResult } from "./useBlitConnection";
import { useBlitContext } from "../BlitContext";

export interface UseBlitSessionsOptions {
  /** Automatically create a PTY if the initial list is empty. Default: false. */
  autoCreateIfEmpty?: boolean;
  /** Tag to assign to auto-created PTYs (requires autoCreateIfEmpty). */
  autoCreateTag?: string;
  /** Command to run in auto-created PTYs (requires autoCreateIfEmpty). */
  autoCreateCommand?: string;
  /** Returns the initial terminal size for auto-created PTYs. */
  getInitialSize?: () => { rows: number; cols: number };
  /**
   * Optional accessor for the WASM Terminal instance backing a PTY.
   * When provided, the hook reads `terminal.title()` after each S2C_UPDATE
   * frame and updates `session.title` — matching the upstream web client
   * behavior for OSC 0/2 title sequences.
   */
  getTerminal?: (ptyId: number) => { title(): string } | null;
  /** Called when a new session appears (server-initiated or from createPty). */
  onSessionCreated?: (session: BlitSession) => void;
  /** Called when a session's subprocess exits but terminal state is retained. */
  onSessionExited?: (session: BlitSession) => void;
  /** Called when a session is closed (dismissed via closePty). */
  onSessionClosed?: (session: BlitSession) => void;
  /** Called when the transport disconnects. */
  onDisconnect?: () => void;
  /** Called when the transport reconnects (status becomes 'connected'). */
  onReconnect?: () => void;
  /** Called when server-side search results arrive. */
  onSearchResults?: (requestId: number, results: SearchResult[]) => void;
}

export interface UseBlitSessionsReturn {
  readonly ready: boolean;
  readonly sessions: readonly BlitSession[];
  readonly status: ConnectionStatus;
  readonly focusedPtyId: number | null;
  readonly createPty: (opts?: {
    rows?: number;
    cols?: number;
    command?: string;
    tag?: string;
    /** When set, the new PTY inherits the cwd of this PTY. */
    srcPtyId?: number;
  }) => Promise<number>;
  readonly focusPty: (ptyId: number) => void;
  readonly closePty: (ptyId: number) => Promise<void>;
  readonly sendSearch: (requestId: number, query: string) => void;
}

export type UseBlitSessionsFn = (
  transport?: BlitTransport,
  options?: UseBlitSessionsOptions,
) => UseBlitSessionsReturn;

export function useBlitSessions(
  transportArg?: BlitTransport,
  options?: UseBlitSessionsOptions,
): UseBlitSessionsReturn {
  const ctx = useBlitContext();
  const transport = transportArg ?? ctx.transport;
  if (!transport) {
    throw new Error(
      "useBlitSessions requires a transport argument or a BlitProvider ancestor",
    );
  }
  const autoCreate = options?.autoCreateIfEmpty ?? false;
  const autoTag = options?.autoCreateTag;
  const autoCommand = options?.autoCreateCommand;
  const getSize = options?.getInitialSize ?? (() => ({ rows: 24, cols: 80 }));
  const getTerminalRef = useRef(options?.getTerminal);
  getTerminalRef.current = options?.getTerminal;
  const lifecycleRef = useRef(options);
  lifecycleRef.current = options;

  const sessionsRef = useRef<readonly BlitSession[]>([]);
  const listenersRef = useRef(new Set<() => void>());
  const readyRef = useRef(false);
  const readyListenersRef = useRef(new Set<() => void>());
  const focusedPtyIdRef = useRef<number | null>(null);
  const focusedListenersRef = useRef(new Set<() => void>());
  const pendingCreatesRef = useRef<Map<number, (ptyId: number) => void>>(
    new Map(),
  );
  const nonceCounterRef = useRef(0);
  const serverFeaturesRef = useRef<number | null>(null);
  const pendingClosesRef = useRef<Map<number, Array<() => void>>>(new Map());
  const wasConnectedRef = useRef(transport.status === "connected");
  const hasConnectedRef = useRef(transport.status === "connected");

  const notify = useCallback(() => {
    for (const l of listenersRef.current) l();
  }, []);

  const notifyReady = useCallback(() => {
    for (const l of readyListenersRef.current) l();
  }, []);

  const notifyFocused = useCallback(() => {
    for (const l of focusedListenersRef.current) l();
  }, []);

  const setFocused = useCallback(
    (ptyId: number | null) => {
      if (focusedPtyIdRef.current !== ptyId) {
        focusedPtyIdRef.current = ptyId;
        notifyFocused();
      }
    },
    [notifyFocused],
  );

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
          { ptyId, tag, title: null, state: "active" as const, ...patch },
        ];
      }
      notify();
    },
    [notify],
  );

  // --- Connection callbacks ---

  const onCreated = useCallback(
    (ptyId: number, tag: string) => {
      upsert(ptyId, tag, { state: "active" });
      const hasNonce =
        serverFeaturesRef.current !== null &&
        (serverFeaturesRef.current & FEATURE_CREATE_NONCE) !== 0;
      if (!hasNonce && pendingCreatesRef.current.size > 0) {
        const first = pendingCreatesRef.current.entries().next().value!;
        pendingCreatesRef.current.delete(first[0]);
        first[1](ptyId);
      }
      if (focusedPtyIdRef.current === null) {
        setFocused(ptyId);
        sendFocusRef.current(ptyId);
      }
      const session = sessionsRef.current.find((s) => s.ptyId === ptyId);
      if (session) lifecycleRef.current?.onSessionCreated?.(session);
    },
    [upsert, setFocused],
  );

  const onCreatedN = useCallback(
    (nonce: number, ptyId: number, tag: string) => {
      upsert(ptyId, tag, { state: "active" });
      const resolve = pendingCreatesRef.current.get(nonce);
      if (resolve) {
        pendingCreatesRef.current.delete(nonce);
        resolve(ptyId);
      }
      // Auto-focus if nothing is focused (e.g. first PTY after connect)
      if (focusedPtyIdRef.current === null) {
        setFocused(ptyId);
        sendFocusRef.current(ptyId);
      }
      const session = sessionsRef.current.find((s) => s.ptyId === ptyId);
      if (session) lifecycleRef.current?.onSessionCreated?.(session);
    },
    [upsert, setFocused],
  );

  const onExited = useCallback(
    (exitedId: number) => {
      const prev = sessionsRef.current;
      const idx = prev.findIndex((s) => s.ptyId === exitedId);
      if (idx >= 0) {
        const next = [...prev];
        next[idx] = { ...next[idx], state: "exited" };
        sessionsRef.current = next;
        notify();
        lifecycleRef.current?.onSessionExited?.(next[idx]);
      }
    },
    [notify],
  );

  const onClosed = useCallback(
    (closedId: number) => {
      const prev = sessionsRef.current;
      const idx = prev.findIndex((s) => s.ptyId === closedId);
      if (idx >= 0) {
        const next = [...prev];
        next[idx] = { ...next[idx], state: "closed" };
        sessionsRef.current = next;
        notify();
        lifecycleRef.current?.onSessionClosed?.(next[idx]);
        const resolvers = pendingClosesRef.current.get(closedId);
        if (resolvers) {
          pendingClosesRef.current.delete(closedId);
          for (const r of resolvers) r();
        }
      }
      if (focusedPtyIdRef.current === closedId) {
        const nextActive = sessionsRef.current.find(
          (s) => s.state === "active" || s.state === "exited",
        );
        setFocused(nextActive?.ptyId ?? null);
      }
    },
    [notify, setFocused],
  );

  const onList = useCallback(
    (entries: { ptyId: number; tag: string }[]) => {
      const ids = new Set(entries.map((e) => e.ptyId));
      let next = sessionsRef.current.map((s) =>
        ids.has(s.ptyId) ? s : { ...s, state: "closed" as const },
      );
      for (const { ptyId, tag } of entries) {
        if (!next.some((s) => s.ptyId === ptyId)) {
          next = [
            ...next,
            { ptyId, tag, title: null, state: "active" as const },
          ];
        }
      }
      sessionsRef.current = next;

      if (!readyRef.current) {
        readyRef.current = true;
        notifyReady();
      }
      notify();

      if (
        focusedPtyIdRef.current !== null &&
        !ids.has(focusedPtyIdRef.current)
      ) {
        const nextAlive = next.find(
          (s) => s.state === "active" || s.state === "exited",
        );
        if (nextAlive) {
          setFocused(nextAlive.ptyId);
          sendFocusRef.current(nextAlive.ptyId);
        } else {
          setFocused(null);
        }
      } else if (focusedPtyIdRef.current === null && entries.length > 0) {
        setFocused(entries[0].ptyId);
        sendFocusRef.current(entries[0].ptyId);
      }

      if (autoCreate && entries.length === 0) {
        const { rows, cols } = getSize();
        const nonce = (nonceCounterRef.current =
          (nonceCounterRef.current + 1) & 0xffff);
        sendCreate2Ref.current(nonce, rows, cols, {
          tag: autoTag,
          command: autoCommand,
        });
      }
    },
    [
      notify,
      notifyReady,
      setFocused,
      autoCreate,
      autoTag,
      autoCommand,
      getSize,
    ],
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

  const onUpdate = useCallback(
    (ptyId: number, _payload: Uint8Array) => {
      const getTerminal = getTerminalRef.current;
      if (!getTerminal) return;
      queueMicrotask(() => {
        const terminal = getTerminal(ptyId);
        if (!terminal) return;
        const title = terminal.title();
        const prev = sessionsRef.current;
        const idx = prev.findIndex((s) => s.ptyId === ptyId);
        if (idx >= 0 && prev[idx].title !== title) {
          const next = [...prev];
          next[idx] = { ...next[idx], title };
          sessionsRef.current = next;
          notify();
        }
      });
    },
    [notify],
  );

  const onStatusChange = useCallback((newStatus: ConnectionStatus) => {
    if (newStatus === "disconnected" || newStatus === "error") {
      if (wasConnectedRef.current) {
        wasConnectedRef.current = false;
        lifecycleRef.current?.onDisconnect?.();
      }
      for (const resolve of pendingCreatesRef.current.values()) {
        resolve(-1);
      }
      pendingCreatesRef.current.clear();
      for (const resolvers of pendingClosesRef.current.values()) {
        for (const r of resolvers) r();
      }
      pendingClosesRef.current.clear();
    } else if (newStatus === "connected") {
      if (!wasConnectedRef.current) {
        wasConnectedRef.current = true;
        if (hasConnectedRef.current) {
          lifecycleRef.current?.onReconnect?.();
        }
        hasConnectedRef.current = true;
      }
    }
  }, []);

  const onHello = useCallback(
    (version: number, features: number) => {
      if (version > PROTOCOL_VERSION) {
        transport.close();
        return;
      }
      serverFeaturesRef.current = features;
    },
    [transport],
  );

  const sendCreate2Ref = useRef<
    (
      nonce: number,
      rows: number,
      cols: number,
      options?: { tag?: string; command?: string; srcPtyId?: number },
    ) => void
  >(() => {});
  const sendFocusRef = useRef<(ptyId: number) => void>(() => {});

  const onSearchResults = useCallback(
    (requestId: number, results: SearchResult[]) => {
      lifecycleRef.current?.onSearchResults?.(requestId, results);
    },
    [],
  );

  const { status, sendCreate2, sendFocus, sendClose, sendSearch } =
    useBlitConnection(transport, {
      onCreated,
      onCreatedN,
      onClosed,
      onExited,
      onList,
      onTitle,
      onUpdate,
      onHello,
      onStatusChange,
      onSearchResults,
    });
  sendCreate2Ref.current = sendCreate2;
  sendFocusRef.current = sendFocus;

  // --- Public API ---

  const sessions = useSyncExternalStore(
    useCallback((l: () => void) => {
      listenersRef.current.add(l);
      return () => listenersRef.current.delete(l);
    }, []),
    useCallback(() => sessionsRef.current, []),
    useCallback(() => sessionsRef.current, []),
  );

  const ready = useSyncExternalStore(
    useCallback((l: () => void) => {
      readyListenersRef.current.add(l);
      return () => readyListenersRef.current.delete(l);
    }, []),
    useCallback(() => readyRef.current, []),
    useCallback(() => readyRef.current, []),
  );

  const focusedPtyId = useSyncExternalStore(
    useCallback((l: () => void) => {
      focusedListenersRef.current.add(l);
      return () => focusedListenersRef.current.delete(l);
    }, []),
    useCallback(() => focusedPtyIdRef.current, []),
    useCallback(() => focusedPtyIdRef.current, []),
  );

  const createPty = useCallback(
    (opts?: {
      rows?: number;
      cols?: number;
      command?: string;
      tag?: string;
      srcPtyId?: number;
    }): Promise<number> => {
      const { rows, cols } = getSize();
      const r = opts?.rows ?? rows;
      const c = opts?.cols ?? cols;
      return new Promise<number>((resolve) => {
        const nonce = (nonceCounterRef.current =
          (nonceCounterRef.current + 1) & 0xffff);
        pendingCreatesRef.current.set(nonce, resolve);
        sendCreate2(nonce, r, c, {
          tag: opts?.tag,
          command: opts?.command,
          srcPtyId: opts?.srcPtyId,
        });
      });
    },
    [sendCreate2, getSize],
  );

  const focusPty = useCallback(
    (ptyId: number) => {
      setFocused(ptyId);
      sendFocus(ptyId);
    },
    [sendFocus, setFocused],
  );

  const closePty = useCallback(
    (ptyId: number): Promise<void> => {
      return new Promise<void>((resolve) => {
        const existing = pendingClosesRef.current.get(ptyId);
        if (existing) {
          existing.push(resolve);
        } else {
          pendingClosesRef.current.set(ptyId, [resolve]);
        }
        sendClose(ptyId);
      });
    },
    [sendClose],
  );

  return {
    ready,
    sessions,
    status,
    focusedPtyId,
    createPty,
    focusPty,
    closePty,
    sendSearch,
  } as const;
}
