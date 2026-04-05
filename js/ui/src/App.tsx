import { createSignal, createEffect, onCleanup, Show } from "solid-js";
import {
  WebSocketTransport,
  WebTransportTransport,
  createShareTransport,
} from "@blit-sh/core";
import type { BlitTransport, BlitWasmModule } from "@blit-sh/core";
import { wsUrl, wtUrl } from "./storage";
import { themeFor } from "./theme";
import { t as i18n } from "./i18n";
import { Workspace } from "./Workspace";
import {
  encryptPassphrase,
  isEncrypted,
  decryptPassphrase,
} from "./passphrase-crypto";

interface BlitConfigDestination {
  id: string;
  type: "gateway";
  label: string;
}

interface BlitConfig {
  gateway?: boolean;
  certHash?: string;
  hub?: string;
  host?: string;
  destinations?: BlitConfigDestination[];
}

function readPassphrase(): string | null {
  const raw = location.hash.slice(1);
  if (!raw) return null;
  const first = raw.split("&")[0];
  if (/^[lpa]=/.test(first)) return null;
  const decoded = decodeURIComponent(first);
  if (isEncrypted(decoded)) return decryptPassphrase(decoded);
  const encrypted = encryptPassphrase(decoded);
  const rest = raw.split("&").slice(1);
  const parts = [encrypted, ...rest].filter(Boolean);
  history.replaceState(null, "", `${location.pathname}#${parts.join("&")}`);
  return decoded;
}

readPassphrase();

async function fetchConfig(): Promise<BlitConfig> {
  const base = location.pathname.endsWith("/")
    ? location.pathname
    : location.pathname + "/";
  const resp = await fetch(base + "config");
  return resp.json();
}

export interface ConnectionSpec {
  id: string;
  label: string;
  transport: BlitTransport;
}

/** Build a WebSocket URL for a specific destination. */
function wsUrlForDest(destId: string): string {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const base = location.pathname.endsWith("/")
    ? location.pathname
    : location.pathname + "/";
  return (
    proto + "//" + location.host + base + "d/" + encodeURIComponent(destId)
  );
}

function createTransport(pass: string, config: BlitConfig): BlitTransport {
  if (config.hub) {
    return createShareTransport(config.hub, pass);
  }
  if (typeof WebTransport !== "undefined" && config.certHash) {
    return new WebTransportTransport(wtUrl(), pass, {
      serverCertificateHash: config.certHash,
    });
  }
  return new WebSocketTransport(wsUrl(), pass);
}

/** Create one transport per destination from the config. */
function createConnectionSpecs(
  pass: string,
  config: BlitConfig,
): ConnectionSpec[] {
  if (config.destinations && config.destinations.length > 0) {
    return config.destinations.map((dest) => ({
      id: dest.id,
      label: dest.label,
      transport: new WebSocketTransport(wsUrlForDest(dest.id), pass),
    }));
  }
  // Fallback: single connection (backward compat with old gateway).
  return [
    {
      id: "main",
      label: config.host ?? location.hostname,
      transport: createTransport(pass, config),
    },
  ];
}

export function App(props: { wasm: BlitWasmModule }) {
  const [passphrase, setPassphrase] = createSignal(readPassphrase());
  const [config, setConfig] = createSignal<BlitConfig | null>(null);

  createEffect(() => {
    const onHashChange = () => setPassphrase(readPassphrase());
    window.addEventListener("hashchange", onHashChange);
    onCleanup(() => window.removeEventListener("hashchange", onHashChange));
  });

  fetchConfig().then(setConfig, () => setConfig({ gateway: true }));

  return (
    <Show when={config()}>
      {(cfg) => (
        <Show
          when={passphrase()}
          fallback={<AuthApp wasm={props.wasm} config={cfg()} />}
        >
          {(pass) => (
            <ConnectedApp
              wasm={props.wasm}
              passphrase={pass()}
              config={cfg()}
            />
          )}
        </Show>
      )}
    </Show>
  );
}

function ConnectedApp(props: {
  wasm: BlitWasmModule;
  passphrase: string;
  config: BlitConfig;
}) {
  const connections = createConnectionSpecs(props.passphrase, props.config);

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

function AuthApp(props: { wasm: BlitWasmModule; config: BlitConfig }) {
  const [transport, setTransport] = createSignal<ConnectionSpec[] | null>(null);
  const [authError, setAuthError] = createSignal<string | null>(null);
  let passRef!: HTMLInputElement;

  function connect(pass: string) {
    setAuthError(null);
    const specs = createConnectionSpecs(pass, props.config);
    // Use the first transport to detect auth success/failure.
    const first = specs[0].transport;
    const onStatus = (status: string) => {
      if (status === "connected") {
        const encrypted = encryptPassphrase(pass);
        history.replaceState(null, "", `${location.pathname}#${encrypted}`);
        first.removeEventListener("statuschange", onStatus);
      } else if (status === "error") {
        setAuthError(first.lastError ?? i18n("auth.failed"));
        first.removeEventListener("statuschange", onStatus);
        setTransport(null);
      }
    };
    first.addEventListener("statuschange", onStatus);
    setTransport(specs);
  }

  return (
    <Show
      when={transport()}
      fallback={
        <AuthScreen
          error={authError()}
          passRef={passRef}
          onSubmit={(pass) => connect(pass)}
        />
      }
    >
      {(specs) => (
        <Workspace
          connections={specs()}
          wasm={props.wasm}
          onAuthError={() => {
            setTransport(null);
            setAuthError(i18n("auth.failed"));
          }}
        />
      )}
    </Show>
  );
}

function AuthScreen(props: {
  error: string | null;
  passRef: HTMLInputElement;
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
