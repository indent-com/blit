import {
  createSignal,
  createEffect,
  createMemo,
  ErrorBoundary,
  onCleanup,
  Show,
} from "solid-js";
import { MuxTransport, createShareTransport } from "@blit-sh/core";
import type { BlitTransport, BlitWasmModule } from "@blit-sh/core";
import {
  useRemotes,
  useDefaultRemote,
  useWtCertHash,
  useWtAddr,
  configWsStatus,
  connectConfigWs,
  disconnectConfigWs,
  configWsUrl,
} from "./storage";
import { themeFor } from "./theme";
import { t as i18n } from "./i18n";
import { Workspace } from "./Workspace";
import { PASSPHRASE_KEY } from "./passphrase-storage";
import { muxWtUrl } from "./transportUrls";

function decodeHashValue(value: string): string {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
}

function readPassphrase(): string | null {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(PASSPHRASE_KEY);
  } catch {}

  const raw = location.hash.slice(1);
  if (!raw) return stored;
  const parts = raw.split("&");
  let decoded: string | null = null;
  let secretPartIndex = -1;

  // Canonical first-contact delivery: #psk=<url-encoded passphrase>.
  for (let i = 0; i < parts.length; i++) {
    const part = parts[i];
    const eq = part.indexOf("=");
    if (eq > 0 && decodeHashValue(part.slice(0, eq)) === "psk") {
      decoded = decodeHashValue(part.slice(eq + 1));
      secretPartIndex = i;
      break;
    }
  }

  if (secretPartIndex < 0) return stored;

  // First contact — secret is being delivered via the URL fragment. Move it
  // to localStorage and strip it from the URL so it does not end up in
  // browser history or get re-shared accidentally.
  const newHash = parts
    .filter((part, i) => i !== secretPartIndex && part)
    .join("&");
  const newUrl =
    location.pathname + location.search + (newHash ? `#${newHash}` : "");
  history.replaceState(null, "", newUrl);
  if (decoded) {
    try {
      localStorage.setItem(PASSPHRASE_KEY, decoded);
    } catch {}
    return decoded;
  }
  return stored;
}

readPassphrase();

export interface ConnectionSpec {
  id: string;
  label: string;
  transport: BlitTransport;
  /** The connection is read-only (an `.ro` share): the server refuses
   *  writes, so its terminals render without input affordances rather
   *  than swallowing keystrokes silently. */
  readOnly?: boolean;
}

const DEFAULT_HUB_URL = "wss://hub.blit.sh";

/**
 * Parse a share: URI into its passphrase and hub URL.
 * Accepts:
 *   share:PASSPHRASE
 *   share:PASSPHRASE?hub=wss://custom.hub
 */
function parseShareUri(uri: string): { passphrase: string; hubUrl: string } {
  const rest = uri.slice("share:".length);
  const qIdx = rest.indexOf("?");
  if (qIdx === -1) {
    return { passphrase: rest, hubUrl: DEFAULT_HUB_URL };
  }
  const passphrase = rest.slice(0, qIdx);
  const params = new URLSearchParams(rest.slice(qIdx + 1));
  const hubUrl = params.get("hub") ?? DEFAULT_HUB_URL;
  return { passphrase, hubUrl };
}

/** Returns true if the URI has ?proxiable=true, meaning the gateway handles it. */
function isProxiable(uri: string): boolean {
  const q = uri.indexOf("?");
  if (q === -1) return false;
  return new URLSearchParams(uri.slice(q + 1)).get("proxiable") === "true";
}

/** Build the WebSocket URL for the multiplexed endpoint. */
function muxWsUrl(): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const base = location.pathname.endsWith("/")
    ? location.pathname
    : location.pathname + "/";
  return proto + "//" + location.host + base + "mux";
}

export function App(props: { wasm: BlitWasmModule }) {
  const [passphrase, setPassphrase] = createSignal(readPassphrase());

  createEffect(() => {
    const onHashChange = () => {
      setPassphrase(readPassphrase());
      // Re-attempt config WS connection now that passphrase may be available.
      connectConfigWs();
    };
    window.addEventListener("hashchange", onHashChange);
    onCleanup(() => window.removeEventListener("hashchange", onHashChange));
  });

  function handleAuth(pass: string) {
    try {
      localStorage.setItem(PASSPHRASE_KEY, pass);
    } catch {}
    setPassphrase(pass);
    connectConfigWs();
  }

  function handleAuthError() {
    try {
      localStorage.removeItem(PASSPHRASE_KEY);
    } catch {}
    disconnectConfigWs();
    setPassphrase(null);
  }

  // Last-resort boundary. Individual tiles contain their own failures
  // (see BlitTile), but a throw in the shell — the dock, the status bar,
  // BSPContainer itself — has nothing above it and would leave a blank
  // page with the reason only in the console. Show it, and offer the one
  // action that reliably helps.
  return (
    <ErrorBoundary fallback={(err: unknown) => <AppCrash err={err} />}>
      <Show when={passphrase()} fallback={<AuthApp onAuth={handleAuth} />}>
        {(pass) => (
          <ConnectedApp
            wasm={props.wasm}
            passphrase={pass()}
            onAuthError={handleAuthError}
          />
        )}
      </Show>
    </ErrorBoundary>
  );
}

/** The shell failed. Deliberately dependency-free: whatever broke may well
 *  be the theme or the workspace this would otherwise read from. */
function AppCrash(props: { err: unknown }) {
  const message = () =>
    props.err instanceof Error
      ? `${props.err.name}: ${props.err.message}\n\n${props.err.stack ?? ""}`
      : String(props.err);
  return (
    <div
      style={{
        position: "fixed",
        inset: "0",
        display: "flex",
        "flex-direction": "column",
        gap: "12px",
        padding: "24px",
        overflow: "auto",
        background: "#1a1a1a",
        color: "#e0e0e0",
        "font-family": "ui-monospace, monospace",
        "font-size": "13px",
      }}
    >
      <b style={{ color: "#f66" }}>blit hit an unexpected error</b>
      <div>Reloading usually recovers; your terminals keep running.</div>
      <div>
        <button
          onClick={() => location.reload()}
          style={{
            padding: "4px 10px",
            background: "#2a2a2a",
            color: "#e0e0e0",
            border: "1px solid rgba(255,255,255,0.15)",
            "border-radius": "3px",
            cursor: "pointer",
            font: "inherit",
          }}
        >
          Reload
        </button>
      </div>
      <pre
        style={{
          "white-space": "pre-wrap",
          "word-break": "break-word",
          color: "rgba(255,255,255,0.5)",
          margin: "0",
        }}
      >
        {message()}
      </pre>
    </div>
  );
}

// ---------------------------------------------------------------------------
// HMR-preserved state: keep the mux transport and channel cache alive across
// hot-module reloads so remote connections are not torn down.
// ---------------------------------------------------------------------------

type HmrData = {
  version: number;
  mux: MuxTransport;
  channelCache: Map<string, { uri: string; transport: BlitTransport }>;
  passphrase: string;
};

// Bump when preserved transport instances are incompatible with hot code.
// Existing class instances keep their old prototype and receive callbacks,
// so reusing one would silently leave transport fixes inactive until reload.
const HMR_DATA_VERSION = 4;

function getHmrData(): HmrData | null {
  return (import.meta.hot?.data?.connectedApp as HmrData) ?? null;
}

function setHmrData(data: HmrData): void {
  if (import.meta.hot) {
    import.meta.hot.data.connectedApp = data;
  }
}

const muxDebug = {
  log: (m: string, ...a: unknown[]) => console.log(`[mux] ${m}`, ...a),
  warn: (m: string, ...a: unknown[]) => console.warn(`[mux] ${m}`, ...a),
  error: (m: string, ...a: unknown[]) => console.error(`[mux] ${m}`, ...a),
};

function ConnectedApp(props: {
  wasm: BlitWasmModule;
  passphrase: string;
  onAuthError: () => void;
}) {
  const remotes = useRemotes();
  const defaultRemote = useDefaultRemote();
  const certHash = useWtCertHash();
  const advertisedWtAddr = useWtAddr();

  // Reuse the mux and channel cache from a previous HMR cycle if the
  // passphrase hasn't changed; otherwise start fresh.
  const prev = getHmrData();
  const reusablePrev =
    prev?.version === HMR_DATA_VERSION && prev.passphrase === props.passphrase;
  if (prev && !reusablePrev) {
    prev.mux.close();
    for (const entry of prev.channelCache.values()) {
      entry.transport.close();
    }
  }

  const channelCache: Map<string, { uri: string; transport: BlitTransport }> =
    reusablePrev ? prev.channelCache : new Map();

  // The MuxTransport is created only once the config WS has resolved.  Before
  // that, mux() returns null and no connection is attempted.
  const [mux, setMux] = createSignal<MuxTransport | null>(
    reusablePrev ? prev.mux : null,
  );

  createEffect(() => {
    const status = configWsStatus();
    const hash = certHash();
    const wtUrl = muxWtUrl(location.href, advertisedWtAddr());
    if (status === "connecting") return;
    const existing = mux();
    if (existing) {
      // The gateway rotates its self-signed WebTransport cert every 13 days
      // and republishes the hash. A long-lived tab has to adopt it, or every
      // later QUIC attempt fails validation and the session silently stays on
      // WebSocket until someone reloads.
      if (hash) existing.updateWtCertHash(hash, wtUrl);
      return;
    }
    const m = new MuxTransport(muxWsUrl(), props.passphrase, {
      wtUrl: hash ? wtUrl : undefined,
      wtCertHash: hash,
      debug: muxDebug,
    });
    m.connect();
    setMux(m);
  });

  // Reconnect triggers the backoff cannot provide. Coming back from sleep or
  // regaining a network otherwise waits out whatever delay the backoff had
  // escalated to — up to 10s of a session that looks dead for no reason.
  // Deliberately in the UI layer: js/core stays free of window/document so it
  // can be imported by the preview service worker (docs/design/net.md).
  createEffect(() => {
    const wake = () => {
      if (document.visibilityState === "hidden") return;
      mux()?.connect();
    };
    window.addEventListener("online", wake);
    document.addEventListener("visibilitychange", wake);
    onCleanup(() => {
      window.removeEventListener("online", wake);
      document.removeEventListener("visibilitychange", wake);
    });
  });

  createEffect(() => {
    const m = mux();
    if (m)
      setHmrData({
        version: HMR_DATA_VERSION,
        mux: m,
        channelCache,
        passphrase: props.passphrase,
      });
  });

  // On real unmount (passphrase change / auth error) close all transports.
  // During HMR the data persists and the next mount will re-adopt them.
  onCleanup(() => {
    if (!import.meta.hot) {
      mux()?.close();
      for (const entry of channelCache.values()) {
        entry.transport.close();
      }
    }
  });

  const connections = createMemo<ConnectionSpec[]>(() => {
    const m = mux();
    // Disabled remotes are kept on disk for re-enabling later but must not
    // produce live transports — skip them here.
    const live = remotes().filter((r) => !r.disabled);
    const dflt = defaultRemote();
    if (!m) return [];
    const next: ConnectionSpec[] = [];
    const seen = new Set<string>();
    for (const { name, uri } of live) {
      seen.add(name);
      const cached = channelCache.get(name);
      if (cached && cached.uri === uri) {
        next.push({ id: name, label: name, transport: cached.transport });
      } else {
        // Close the old transport before replacing it (URI changed).
        if (cached) cached.transport.close();
        let transport: BlitTransport;
        if (uri.toLowerCase().startsWith("share:") && !isProxiable(uri)) {
          // Direct WebRTC share — not multiplexed.
          const { passphrase, hubUrl } = parseShareUri(uri);
          transport = createShareTransport(hubUrl, passphrase);
        } else {
          // Gateway-proxied destination — use a mux channel.
          transport = m.createChannel(name);
        }
        channelCache.set(name, { uri, transport });
        next.push({ id: name, label: name, transport });
      }
    }
    // Evict stale cache entries, closing their transports.
    for (const [key, entry] of channelCache) {
      if (!seen.has(key)) {
        entry.transport.close();
        channelCache.delete(key);
      }
    }
    // Move the default remote to the front so it is used for new terminals.
    if (dflt && dflt !== "local") {
      const idx = next.findIndex((c) => c.id === dflt);
      if (idx > 0) next.unshift(...next.splice(idx, 1));
    }
    return next;
  });

  return (
    <Workspace
      connections={connections}
      wasm={props.wasm}
      onAuthError={props.onAuthError}
    />
  );
}

function AuthApp(props: { onAuth: (pass: string) => void }) {
  const [authError, setAuthError] = createSignal<string | null>(null);

  function connect(pass: string) {
    setAuthError(null);
    const ws = new WebSocket(configWsUrl());
    let authed = false;
    let throttled = false;

    ws.onopen = () => {
      ws.send(pass);
    };

    ws.onmessage = (ev) => {
      const msg = String(ev.data);
      if (msg === "ok") {
        authed = true;
        ws.close();
        props.onAuth(pass);
      } else if (msg === "busy") {
        // Throttled before the passphrase was even checked. Saying
        // "authentication failed" here sends the user hunting for a wrong
        // credential when the only thing to do is wait.
        throttled = true;
        setAuthError(i18n("auth.busy"));
      }
    };

    ws.onerror = () => {};

    ws.onclose = () => {
      if (!authed && !throttled) {
        setAuthError(i18n("auth.failed"));
      }
    };
  }

  return <AuthScreen error={authError()} onSubmit={(pass) => connect(pass)} />;
}

function AuthScreen(props: {
  error: string | null;
  onSubmit: (pass: string) => void;
}) {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const theme = themeFor(dark);
  let inputRef!: HTMLInputElement;

  return (
    <main
      style={{
        display: "flex",
        "align-items": "center",
        "justify-content": "center",
        height: "100%",
        "background-color": theme.bg,
      }}
    >
      <form
        style={{
          display: "flex",
          "flex-direction": "column",
          gap: "0.5em",
        }}
        onSubmit={(e) => {
          e.preventDefault();
          const v = inputRef?.value;
          if (v) props.onSubmit(v);
        }}
      >
        <input
          ref={inputRef}
          name="blit-passphrase"
          type="password"
          placeholder={i18n("auth.placeholder")}
          autofocus
          style={{
            padding: "0.5em 0.75em",
            "font-size": "1em",
            border: "1px solid #444",
            outline: "none",
            width: "20em",
            "font-family": "inherit",
            "background-color": theme.solidInputBg,
            color: theme.fg,
          }}
        />
        <Show when={props.error}>
          {(err) => (
            <output style={{ color: theme.errorText, "font-size": "0.85em" }}>
              {err()}
            </output>
          )}
        </Show>
      </form>
    </main>
  );
}
