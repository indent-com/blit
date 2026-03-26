import { useCallback, useEffect, useRef, useSyncExternalStore } from "react";
import type { BlitTransport, ConnectionStatus } from "../types";
import {
  S2C_UPDATE,
  S2C_CREATED,
  S2C_CREATED_N,
  S2C_CLOSED,
  S2C_LIST,
  S2C_TITLE,
  S2C_SEARCH_RESULTS,
  S2C_HELLO,
  S2C_EXITED,
} from "../types";
import {
  buildAckMessage,
  buildInputMessage,
  buildResizeMessage,
  buildScrollMessage,
  buildCreate2Message,
  buildFocusMessage,
  buildCloseMessage,
  buildSubscribeMessage,
  buildUnsubscribeMessage,
  buildSearchMessage,
} from "../protocol";

const textDecoder = new TextDecoder();

export interface ServerMessage {
  type: number;
  ptyId: number;
  payload?: Uint8Array;
  title?: string;
  ptyIds?: number[];
}

export interface PtyListEntry {
  ptyId: number;
  tag: string;
}

export const SEARCH_SOURCE_TITLE = 0;
export const SEARCH_SOURCE_VISIBLE = 1;
export const SEARCH_SOURCE_SCROLLBACK = 2;
export const SEARCH_MATCH_TITLE = 1 << 0;
export const SEARCH_MATCH_VISIBLE = 1 << 1;
export const SEARCH_MATCH_SCROLLBACK = 1 << 2;

export interface SearchResult {
  ptyId: number;
  score: number;
  primarySource: number;
  matchedSources: number;
  scrollOffset: number | null;
  context: string;
}

export interface BlitConnectionCallbacks {
  onUpdate?: (ptyId: number, payload: Uint8Array) => void;
  onCreated?: (ptyId: number, tag: string) => void;
  onCreatedN?: (nonce: number, ptyId: number, tag: string) => void;
  onClosed?: (ptyId: number) => void;
  onExited?: (ptyId: number) => void;
  onList?: (entries: PtyListEntry[]) => void;
  onTitle?: (ptyId: number, title: string) => void;
  onSearchResults?: (requestId: number, results: SearchResult[]) => void;
  onHello?: (version: number, features: number) => void;
  onStatusChange?: (status: ConnectionStatus) => void;
}

export function useBlitConnection(
  transport: BlitTransport,
  callbacks: BlitConnectionCallbacks,
) {
  const callbacksRef = useRef(callbacks);
  callbacksRef.current = callbacks;

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
    const onMessage = (data: ArrayBuffer) => {
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
        case S2C_CREATED_N: {
          if (bytes.length < 5) break;
          const nonce = bytes[1] | (bytes[2] << 8);
          const ptyId = bytes[3] | (bytes[4] << 8);
          const tag = textDecoder.decode(bytes.subarray(5));
          callbacksRef.current.onCreatedN?.(nonce, ptyId, tag);
          break;
        }
        case S2C_CLOSED: {
          if (bytes.length < 3) break;
          const ptyId = bytes[1] | (bytes[2] << 8);
          callbacksRef.current.onClosed?.(ptyId);
          break;
        }
        case S2C_EXITED: {
          if (bytes.length < 3) break;
          const ptyId = bytes[1] | (bytes[2] << 8);
          callbacksRef.current.onExited?.(ptyId);
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
        case S2C_SEARCH_RESULTS: {
          if (bytes.length < 5) break;
          const requestId = bytes[1] | (bytes[2] << 8);
          const count = bytes[3] | (bytes[4] << 8);
          const results: SearchResult[] = [];
          let off = 5;
          for (let i = 0; i < count; i++) {
            if (off + 14 > bytes.length) break;
            const ptyId = bytes[off] | (bytes[off + 1] << 8);
            const score =
              bytes[off + 2] |
              (bytes[off + 3] << 8) |
              (bytes[off + 4] << 16) |
              ((bytes[off + 5] << 24) >>> 0);
            const primarySource = bytes[off + 6];
            const matchedSources = bytes[off + 7];
            const rawScroll =
              (bytes[off + 8] |
              (bytes[off + 9] << 8) |
              (bytes[off + 10] << 16) |
              (bytes[off + 11] << 24)) >>> 0;
            const scrollOffset = rawScroll === 0xffffffff ? null : rawScroll;
            const contextLen = bytes[off + 12] | (bytes[off + 13] << 8);
            off += 14;
            const context = textDecoder.decode(
              bytes.subarray(off, off + contextLen),
            );
            off += contextLen;
            results.push({
              ptyId,
              score,
              primarySource,
              matchedSources,
              scrollOffset,
              context,
            });
          }
          callbacksRef.current.onSearchResults?.(requestId, results);
          break;
        }
        case S2C_HELLO: {
          if (bytes.length < 7) break;
          const version = bytes[1] | (bytes[2] << 8);
          const features =
            bytes[3] | (bytes[4] << 8) | (bytes[5] << 16) | (bytes[6] << 24);
          callbacksRef.current.onHello?.(version, features);
          break;
        }
      }
    };

    const onStatus = (newStatus: ConnectionStatus) => {
      statusRef.current = newStatus;
      for (const listener of listenersRef.current) listener();
      callbacksRef.current.onStatusChange?.(newStatus);
    };

    transport.addEventListener("message", onMessage);
    transport.addEventListener("statuschange", onStatus);

    if (statusRef.current !== transport.status) {
      statusRef.current = transport.status;
      for (const listener of listenersRef.current) listener();
    }

    return () => {
      transport.removeEventListener("message", onMessage);
      transport.removeEventListener("statuschange", onStatus);
    };
  }, [transport]);

  // --- Client message helpers ---

  const sendAck = useCallback(() => {
    transport.send(buildAckMessage());
  }, [transport]);

  const sendInput = useCallback(
    (ptyId: number, data: Uint8Array) => {
      transport.send(buildInputMessage(ptyId, data));
    },
    [transport],
  );

  const sendResize = useCallback(
    (ptyId: number, rows: number, cols: number) => {
      transport.send(buildResizeMessage(ptyId, rows, cols));
    },
    [transport],
  );

  const sendScroll = useCallback(
    (ptyId: number, offset: number) => {
      transport.send(buildScrollMessage(ptyId, offset));
    },
    [transport],
  );

  const sendCreate2 = useCallback(
    (
      nonce: number,
      rows: number,
      cols: number,
      options?: { tag?: string; command?: string; srcPtyId?: number },
    ) => {
      transport.send(buildCreate2Message(nonce, rows, cols, options));
    },
    [transport],
  );

  const sendFocus = useCallback(
    (ptyId: number) => {
      transport.send(buildFocusMessage(ptyId));
    },
    [transport],
  );

  const sendClose = useCallback(
    (ptyId: number) => {
      transport.send(buildCloseMessage(ptyId));
    },
    [transport],
  );

  const sendSubscribe = useCallback(
    (ptyId: number) => {
      transport.send(buildSubscribeMessage(ptyId));
    },
    [transport],
  );

  const sendUnsubscribe = useCallback(
    (ptyId: number) => {
      transport.send(buildUnsubscribeMessage(ptyId));
    },
    [transport],
  );

  const sendSearch = useCallback(
    (requestId: number, query: string) => {
      transport.send(buildSearchMessage(requestId, query));
    },
    [transport],
  );

  return {
    status,
    sendAck,
    sendInput,
    sendResize,
    sendScroll,
    sendCreate2,
    sendFocus,
    sendClose,
    sendSubscribe,
    sendUnsubscribe,
    sendSearch,
  };
}
