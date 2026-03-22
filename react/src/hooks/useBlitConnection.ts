import { useCallback, useEffect, useRef, useSyncExternalStore } from 'react';
import type { BlitTransport, ConnectionStatus } from '../types';
import {
  S2C_UPDATE,
  S2C_CREATED,
  S2C_CLOSED,
  S2C_LIST,
  S2C_TITLE,
  C2S_ACK,
  C2S_SUBSCRIBE,
  C2S_UNSUBSCRIBE,
  C2S_CREATE,
  C2S_CLOSE,
  C2S_FOCUS,
  C2S_RESIZE,
  C2S_INPUT,
  C2S_SCROLL,
} from '../types';

const textDecoder = new TextDecoder();

export interface ServerMessage {
  type: number;
  ptyId: number;
  /** Raw payload after the 3-byte header (only for S2C_UPDATE). */
  payload?: Uint8Array;
  /** Decoded title string (only for S2C_TITLE). */
  title?: string;
  /** List of PTY IDs (only for S2C_LIST). */
  ptyIds?: number[];
}

export interface PtyListEntry {
  ptyId: number;
  tag: string;
}

export interface BlitConnectionCallbacks {
  onUpdate?: (ptyId: number, payload: Uint8Array) => void;
  onCreated?: (ptyId: number, tag: string) => void;
  onClosed?: (ptyId: number) => void;
  onList?: (entries: PtyListEntry[]) => void;
  onTitle?: (ptyId: number, title: string) => void;
}

/**
 * Hook that manages a blit transport connection, parsing server messages
 * and providing helper functions for sending client messages.
 */
export function useBlitConnection(
  transport: BlitTransport,
  callbacks: BlitConnectionCallbacks,
) {
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;

  // Status tracking via useSyncExternalStore for tear-free reads.
  const statusRef = useRef<ConnectionStatus>(transport.status);
  const listenersRef = useRef(new Set<() => void>());

  const subscribe = useCallback((listener: () => void) => {
    listenersRef.current.add(listener);
    return () => {
      listenersRef.current.delete(listener);
    };
  }, []);

  const getSnapshot = useCallback(() => statusRef.current, []);

  const status = useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

  useEffect(() => {
    const prevOnMessage = transport.onmessage;
    const prevOnStatus = transport.onstatuschange;

    transport.onstatuschange = (newStatus: ConnectionStatus) => {
      statusRef.current = newStatus;
      for (const listener of listenersRef.current) listener();
    };
    // Sync in case status changed before we attached.
    if (statusRef.current !== transport.status) {
      statusRef.current = transport.status;
      for (const listener of listenersRef.current) listener();
    }

    transport.onmessage = (data: ArrayBuffer) => {
      const bytes = new Uint8Array(data);
      if (bytes.length === 0) return;

      const type = bytes[0];
      switch (type) {
        case S2C_UPDATE: {
          if (bytes.length < 3) break;
          const ptyId = bytes[1] | (bytes[2] << 8);
          const payload = bytes.subarray(3);
          callbacksRef.current.onUpdate?.(ptyId, payload);
          break;
        }
        case S2C_CREATED: {
          if (bytes.length < 3) break;
          const ptyId = bytes[1] | (bytes[2] << 8);
          const tag = textDecoder.decode(bytes.subarray(3));
          callbacksRef.current.onCreated?.(ptyId, tag);
          break;
        }
        case S2C_CLOSED: {
          if (bytes.length < 3) break;
          const ptyId = bytes[1] | (bytes[2] << 8);
          callbacksRef.current.onClosed?.(ptyId);
          break;
        }
        case S2C_LIST: {
          if (bytes.length < 3) break;
          const count = bytes[1] | (bytes[2] << 8);
          const entries: { ptyId: number; tag: string }[] = [];
          let off = 3;
          for (let i = 0; i < count; i++) {
            if (off + 4 > bytes.length) break;
            const ptyId = bytes[off] | (bytes[off + 1] << 8);
            const tagLen = bytes[off + 2] | (bytes[off + 3] << 8);
            off += 4;
            const tag = textDecoder.decode(bytes.subarray(off, off + tagLen));
            off += tagLen;
            entries.push({ ptyId, tag });
          }
          callbacksRef.current.onList?.(entries);
          break;
        }
        case S2C_TITLE: {
          if (bytes.length < 3) break;
          const ptyId = bytes[1] | (bytes[2] << 8);
          const title = textDecoder.decode(bytes.subarray(3));
          callbacksRef.current.onTitle?.(ptyId, title);
          break;
        }
      }
    };

    return () => {
      transport.onmessage = prevOnMessage;
      transport.onstatuschange = prevOnStatus;
    };
  }, [transport]);

  // --- Client message helpers ---

  const sendAck = useCallback(() => {
    transport.send(new Uint8Array([C2S_ACK]));
  }, [transport]);

  const sendInput = useCallback(
    (ptyId: number, data: Uint8Array) => {
      const msg = new Uint8Array(3 + data.length);
      msg[0] = C2S_INPUT;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      msg.set(data, 3);
      transport.send(msg);
    },
    [transport],
  );

  const sendResize = useCallback(
    (ptyId: number, rows: number, cols: number) => {
      const msg = new Uint8Array(7);
      msg[0] = C2S_RESIZE;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      msg[3] = rows & 0xff;
      msg[4] = (rows >> 8) & 0xff;
      msg[5] = cols & 0xff;
      msg[6] = (cols >> 8) & 0xff;
      transport.send(msg);
    },
    [transport],
  );

  const sendScroll = useCallback(
    (ptyId: number, offset: number) => {
      const msg = new Uint8Array(7);
      msg[0] = C2S_SCROLL;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      msg[3] = offset & 0xff;
      msg[4] = (offset >> 8) & 0xff;
      msg[5] = (offset >> 16) & 0xff;
      msg[6] = (offset >> 24) & 0xff;
      transport.send(msg);
    },
    [transport],
  );

  const sendCreate = useCallback(
    (rows: number, cols: number, options?: { tag?: string; command?: string }) => {
      const encoder = new TextEncoder();
      const tagBytes = options?.tag ? encoder.encode(options.tag) : new Uint8Array(0);
      const commandBytes = options?.command?.trim()
        ? encoder.encode(options.command.trim())
        : null;
      const msg = new Uint8Array(7 + tagBytes.length + (commandBytes ? commandBytes.length : 0));
      msg[0] = C2S_CREATE;
      msg[1] = rows & 0xff;
      msg[2] = (rows >> 8) & 0xff;
      msg[3] = cols & 0xff;
      msg[4] = (cols >> 8) & 0xff;
      msg[5] = tagBytes.length & 0xff;
      msg[6] = (tagBytes.length >> 8) & 0xff;
      if (tagBytes.length) msg.set(tagBytes, 7);
      if (commandBytes) msg.set(commandBytes, 7 + tagBytes.length);
      transport.send(msg);
    },
    [transport],
  );

  const sendFocus = useCallback(
    (ptyId: number) => {
      const msg = new Uint8Array(3);
      msg[0] = C2S_FOCUS;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      transport.send(msg);
    },
    [transport],
  );

  const sendClose = useCallback(
    (ptyId: number) => {
      const msg = new Uint8Array(3);
      msg[0] = C2S_CLOSE;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      transport.send(msg);
    },
    [transport],
  );

  const sendSubscribe = useCallback(
    (ptyId: number) => {
      const msg = new Uint8Array(3);
      msg[0] = C2S_SUBSCRIBE;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      transport.send(msg);
    },
    [transport],
  );

  const sendUnsubscribe = useCallback(
    (ptyId: number) => {
      const msg = new Uint8Array(3);
      msg[0] = C2S_UNSUBSCRIBE;
      msg[1] = ptyId & 0xff;
      msg[2] = (ptyId >> 8) & 0xff;
      transport.send(msg);
    },
    [transport],
  );

  return {
    status,
    sendAck,
    sendInput,
    sendResize,
    sendScroll,
    sendCreate,
    sendFocus,
    sendClose,
    sendSubscribe,
    sendUnsubscribe,
  };
}
