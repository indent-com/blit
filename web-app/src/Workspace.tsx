import {
  useState,
  useCallback,
  useEffect,
  useRef,
} from "react";
import {
  BlitTerminal,
  BlitProvider,
  useBlitSessions,
  WebSocketTransport,
  TerminalStore,
  DEFAULT_FONT,
} from "blit-react";
import type {
  BlitTerminalHandle,
  BlitWasmModule,
  TerminalPalette,
  UseBlitSessionsReturn,
  SearchResult,
} from "blit-react";
import { useMetrics } from "./useMetrics";
import { PALETTE_KEY, FONT_KEY, writeStorage, preferredPalette, preferredFont, blitHost } from "./storage";
import { styles } from "./styles";
import { StatusBar } from "./StatusBar";
import { ExposeOverlay } from "./ExposeOverlay";
import { PaletteOverlay } from "./PaletteOverlay";
import { FontOverlay } from "./FontOverlay";
import { HelpOverlay } from "./HelpOverlay";

export type Overlay = "expose" | "palette" | "font" | "help" | null;

export function Workspace({ transport, wasm, onAuthError }: { transport: WebSocketTransport; wasm: BlitWasmModule; onAuthError: () => void }) {
  const [palette, setPalette] = useState<TerminalPalette>(preferredPalette);
  const [font, setFont] = useState(preferredFont);
  const [fontSize] = useState(13);
  const [overlay, setOverlay] = useState<Overlay>(null);
  const termRef = useRef<BlitTerminalHandle>(null);
  const overlayRef = useRef<Overlay>(null);
  overlayRef.current = overlay;
  const sessionsRef = useRef<UseBlitSessionsReturn | null>(null);
  const searchResultsCbRef = useRef<((reqId: number, results: SearchResult[]) => void) | null>(null);

  const storeRef = useRef<TerminalStore | null>(null);
  if (!storeRef.current) {
    storeRef.current = new TerminalStore(transport, wasm);
  }
  const store = storeRef.current;

  const onSearchResults = useCallback(
    (reqId: number, results: SearchResult[]) => {
      searchResultsCbRef.current?.(reqId, results);
    },
    [],
  );

  const sessions = useBlitSessions(transport, {
    autoCreateIfEmpty: true,
    getInitialSize: () => ({
      rows: termRef.current?.rows ?? 24,
      cols: termRef.current?.cols ?? 80,
    }),
    getTerminal: (ptyId) => store.getTerminal(ptyId),
    onSearchResults,
    onSessionClosed: (session) => store.freeTerminal(session.ptyId),
  });
  sessionsRef.current = sessions;
  const metrics = useMetrics(transport);

  const dark = palette.dark;

  useEffect(() => {
    store.setPalette(palette);
  }, [store, palette]);

  useEffect(() => {
    store.setFontFamily(font);
  }, [store, font]);

  useEffect(() => {
    store.setLead(sessions.focusedPtyId);
  }, [store, sessions.focusedPtyId]);

  useEffect(() => {
    const desired = new Set<number>();
    if (sessions.focusedPtyId !== null) desired.add(sessions.focusedPtyId);
    if (overlay === "expose") {
      for (const s of sessions.sessions) {
        if (s.state !== "closed") desired.add(s.ptyId);
      }
    }
    store.setDesiredSubscriptions(desired);
  }, [store, sessions.focusedPtyId, sessions.sessions, overlay]);

  useEffect(() => {
    return () => store.dispose();
  }, [store]);

  useEffect(() => {
    let wasConnected = false;
    const onStatus = (status: string) => {
      if (status === "connected") wasConnected = true;
      if (status === "error" && !wasConnected) onAuthError();
    };
    transport.addEventListener("statuschange", onStatus);
    return () => transport.removeEventListener("statuschange", onStatus);
  }, [transport, onAuthError]);

  const termCallbackRef = useCallback((handle: BlitTerminalHandle | null) => {
    (termRef as React.MutableRefObject<BlitTerminalHandle | null>).current = handle;
    if (handle && !overlayRef.current) {
      handle.focus();
    }
  }, []);

  useEffect(() => {
    document.documentElement.setAttribute(
      "data-theme",
      dark ? "dark" : "light",
    );
  }, [dark]);

  useEffect(() => {
    document.documentElement.style.fontFamily = "system-ui, sans-serif";
  }, []);

  useEffect(() => {
    const focused = sessions.sessions.find(
      (s) => s.ptyId === sessions.focusedPtyId,
    );
    const host = blitHost();
    const parts: string[] = [];
    if (focused?.title) parts.push(focused.title);
    if (host && host !== "localhost" && host !== "127.0.0.1") parts.push(host);
    parts.push("blit");
    document.title = parts.join(" — ");
  }, [sessions.focusedPtyId, sessions.sessions]);

  const focusTerminal = useCallback(() => {
    setTimeout(() => termRef.current?.focus(), 0);
  }, []);

  const closeOverlay = useCallback(() => {
    setOverlay(null);
    focusTerminal();
  }, [focusTerminal]);

  const toggleOverlay = useCallback((target: Overlay) => {
    setOverlay((cur) => {
      if (cur === target) {
        focusTerminal();
        return null;
      }
      return target;
    });
  }, [focusTerminal]);

  const changePalette = useCallback((p: TerminalPalette) => {
    setPalette(p);
    writeStorage(PALETTE_KEY, p.id);
    closeOverlay();
  }, [closeOverlay]);

  const changeFont = useCallback((f: string) => {
    const value = f.trim() || DEFAULT_FONT;
    setFont(value);
    writeStorage(FONT_KEY, value);
    closeOverlay();
  }, [closeOverlay]);

  const switchPty = useCallback(
    (ptyId: number) => {
      sessions.focusPty(ptyId);
      closeOverlay();
    },
    [sessions, closeOverlay],
  );

  const createAndFocus = useCallback(async (command?: string) => {
    const id = await sessions.createPty({
      ...(command ? { command } : {}),
      ...(!command && sessions.focusedPtyId != null ? { srcPtyId: sessions.focusedPtyId } : {}),
    });
    sessions.focusPty(id);
    closeOverlay();
  }, [sessions, closeOverlay]);

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const mod = e.metaKey || e.ctrlKey;

      if (mod && !e.shiftKey && e.key === "k") {
        e.preventDefault();
        toggleOverlay("expose");
        return;
      }
      if (mod && e.shiftKey && e.key === "P") {
        e.preventDefault();
        toggleOverlay("palette");
        return;
      }
      if (mod && e.shiftKey && e.key === "F") {
        e.preventDefault();
        toggleOverlay("font");
        return;
      }
      if (e.ctrlKey && e.shiftKey && (e.key === "?" || e.code === "Slash")) {
        e.preventDefault();
        toggleOverlay("help");
        return;
      }
      if (mod && e.shiftKey && e.key === "Enter") {
        e.preventDefault();
        createAndFocus();
        return;
      }
      if (mod && e.shiftKey && e.key === "W") {
        e.preventDefault();
        const s = sessionsRef.current;
        if (s && s.focusedPtyId != null) s.closePty(s.focusedPtyId);
        return;
      }
      if (mod && e.shiftKey && (e.key === "{" || e.key === "}")) {
        e.preventDefault();
        const s = sessionsRef.current;
        if (!s) return;
        const ids = s.sessions
          .filter((x) => x.state !== "closed")
          .map((x) => x.ptyId);
        if (ids.length < 2 || s.focusedPtyId == null) return;
        const idx = ids.indexOf(s.focusedPtyId);
        const next =
          e.key === "}"
            ? ids[(idx + 1) % ids.length]
            : ids[(idx - 1 + ids.length) % ids.length];
        s.focusPty(next);
        return;
      }
      if (e.key === "Escape" && overlayRef.current) {
        e.preventDefault();
        closeOverlay();
        return;
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [toggleOverlay, closeOverlay, createAndFocus]);

  const bg = `rgb(${palette.bg[0]},${palette.bg[1]},${palette.bg[2]})`;

  return (
    <BlitProvider transport={transport} store={store} palette={palette} fontFamily={font} fontSize={fontSize}>
      <main
        style={{
          ...styles.workspace,
          backgroundColor: bg,
          color: dark ? "#e0e0e0" : "#333",
        }}
      >
        <section style={styles.termContainer}>
          {sessions.focusedPtyId != null && (
            <BlitTerminal
              ref={termCallbackRef}
              ptyId={sessions.focusedPtyId}
              style={{ width: "100%", height: "100%" }}
            />
          )}
          {(sessions.status === "disconnected" || sessions.status === "error") && (
            <output style={styles.disconnected}>Disconnected</output>
          )}
        </section>
        {overlay === "expose" && (
          <ExposeOverlay
            sessions={sessions}
            onSelect={switchPty}
            onClose={closeOverlay}
            onCreate={createAndFocus}
            searchResultsCbRef={searchResultsCbRef}
          />
        )}
        {overlay === "palette" && (
          <PaletteOverlay
            current={palette}
            onSelect={changePalette}
            onPreview={setPalette}
            onClose={closeOverlay}
            dark={dark}
          />
        )}
        {overlay === "font" && (
          <FontOverlay
            current={font}
            onSelect={changeFont}
            onClose={closeOverlay}
            dark={dark}
          />
        )}
        {overlay === "help" && (
          <HelpOverlay onClose={closeOverlay} dark={dark} />
        )}
        <footer
          style={{
            ...styles.statusBar,
            borderTopColor: dark ? "rgba(255,255,255,0.1)" : "rgba(0,0,0,0.1)",
          }}
        >
          <StatusBar
            sessions={sessions}
            metrics={metrics}
            palette={palette}
            onExpose={() => toggleOverlay("expose")}
            onPalette={() => toggleOverlay("palette")}
            onFont={() => toggleOverlay("font")}
          />
        </footer>
      </main>
    </BlitProvider>
  );
}
