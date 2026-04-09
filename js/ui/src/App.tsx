import {
  createSignal,
  createEffect,
  createMemo,
  onCleanup,
  Show,
} from "solid-js";
import { MuxTransport, createShareTransport } from "@blit-sh/core";
import type { BlitTransport, BlitWasmModule } from "@blit-sh/core";
import {
  useRemotes,
  useDefaultRemote,
  connectConfigWs,
  configWsUrl,
} from "./storage";
import { themeFor } from "./theme";
import { t as i18n } from "./i18n";
import { Workspace } from "./Workspace";
import {
  encryptPassphrase,
  isEncrypted,
  decryptPassphrase,
} from "./passphrase-crypto";

function readPassphrase(): string | null {
  const raw = location.hash.slice(1);
  if (!raw) return null;
  const first = raw.split("&")[0];
  if (/^[lpa]=/.test(first)) return null;
  const decoded = decodeURIComponent(first);
  if (isEncrypted(decoded)) {
    // Already encrypted — decrypt and return. If decryption fails (wrong
    // browser key) return null without touching the hash, so the layout
    // params (l=, a=, p=, t=) are preserved for after the user authenticates.
    return decryptPassphrase(decoded);
  }
  // Plain-text passphrase — encrypt it in-place so it isn't visible in
  // browser history, preserving all other hash params.
  const encrypted = encryptPassphrase(decoded);
  const rest = raw.split("&").slice(1);
  const parts = [encrypted, ...rest].filter(Boolean);
  history.replaceState(null, "", `${location.pathname}#${parts.join("&")}`);
  return decoded;
}

readPassphrase();

export interface ConnectionSpec {
  id: string;
  label: string;
  transport: BlitTransport;
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
    const encrypted = encryptPassphrase(pass);
    // Keep layout/assignment params (l=, a=, p=, t=, s=) from the existing
    // hash so that layout state is restored after authentication.
    const otherParams = location.hash
      .slice(1)
      .split("&")
      .filter((s) => /^[lpast]=/.test(s));
    const newHash = [encrypted, ...otherParams].join("&");
    history.replaceState(null, "", `${location.pathname}#${newHash}`);
    setPassphrase(pass);
    connectConfigWs();
  }

  return (
    <Show when={passphrase()} fallback={<AuthApp onAuth={handleAuth} />}>
      {(pass) => <ConnectedApp wasm={props.wasm} passphrase={pass()} />}
    </Show>
  );
}

function ConnectedApp(props: { wasm: BlitWasmModule; passphrase: string }) {
  const remotes = useRemotes();
  const defaultRemote = useDefaultRemote();

  // Single multiplexed WebSocket for all gateway-proxied destinations.
  const mux = new MuxTransport(muxWsUrl(), props.passphrase);
  mux.connect();

  onCleanup(() => mux.close());

  // Channel cache: maps destination name → MuxChannel (or share transport).
  // Channels are reused across reactive updates; only created/closed when the
  // remote list changes.
  const channelCache = new Map<
    string,
    { uri: string; transport: BlitTransport }
  >();

  const connections = createMemo<ConnectionSpec[]>(() => {
    const live = remotes();
    const dflt = defaultRemote();
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
          transport = mux.createChannel(name);
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
      onAuthError={() => {
        history.replaceState(null, "", location.pathname);
        window.location.reload();
      }}
    />
  );
}

function AuthApp(props: { onAuth: (pass: string) => void }) {
  const [authError, setAuthError] = createSignal<string | null>(null);

  function connect(pass: string) {
    setAuthError(null);
    const ws = new WebSocket(configWsUrl());
    let authed = false;

    ws.onopen = () => {
      ws.send(pass);
    };

    ws.onmessage = (ev) => {
      const msg = String(ev.data);
      if (msg === "ok") {
        authed = true;
        ws.close();
        props.onAuth(pass);
      }
    };

    ws.onerror = () => {};

    ws.onclose = () => {
      if (!authed) {
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
