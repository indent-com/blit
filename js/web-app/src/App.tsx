import { useState, useEffect, useCallback, useRef } from "react";
import {
  WebSocketTransport,
  WebTransportTransport,
  createShareTransport,
} from "@blit-sh/core";
import type { BlitTransport } from "@blit-sh/core";
import type { BlitWasmModule } from "@blit-sh/core";
import { wsUrl, wtUrl } from "./storage";
import { themeFor } from "./theme";
import { t as i18n } from "./i18n";
import { Workspace } from "./Workspace";
import {
  encryptPassphrase,
  isEncrypted,
  decryptPassphrase,
} from "./passphrase-crypto";

interface BlitConfig {
  gateway?: boolean;
  certHash?: string;
  hub?: string;
  host?: string;
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
  const parts = [encodeURIComponent(encrypted), ...rest].filter(Boolean);
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

export function App({ wasm }: { wasm: BlitWasmModule }) {
  const [passphrase, setPassphrase] = useState(readPassphrase);
  const [config, setConfig] = useState<BlitConfig | null>(null);

  useEffect(() => {
    const onHashChange = () => setPassphrase(readPassphrase());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    fetchConfig().then(setConfig, () => setConfig({ gateway: true }));
  }, []);

  if (!config) return null;

  if (passphrase) {
    return <ConnectedApp wasm={wasm} passphrase={passphrase} config={config} />;
  }

  return <AuthApp wasm={wasm} config={config} />;
}

function ConnectedApp({
  wasm,
  passphrase,
  config,
}: {
  wasm: BlitWasmModule;
  passphrase: string;
  config: BlitConfig;
}) {
  const [transport] = useState<BlitTransport>(() =>
    createTransport(passphrase, config),
  );

  return (
    <Workspace
      transport={transport}
      wasm={wasm}
      onAuthError={() => {
        history.replaceState(null, "", location.pathname);
        window.location.reload();
      }}
    />
  );
}

function AuthApp({
  wasm,
  config,
}: {
  wasm: BlitWasmModule;
  config: BlitConfig;
}) {
  const [transport, setTransport] = useState<BlitTransport | null>(null);
  const [authError, setAuthError] = useState<string | null>(null);
  const passRef = useRef<HTMLInputElement | null>(null);

  const connect = useCallback(
    (pass: string) => {
      setAuthError(null);
      const t = createTransport(pass, config);
      const onStatus = (status: string) => {
        if (status === "connected") {
          const encrypted = encryptPassphrase(pass);
          history.replaceState(
            null,
            "",
            `${location.pathname}#${encodeURIComponent(encrypted)}`,
          );
          t.removeEventListener("statuschange", onStatus);
        } else if (status === "error") {
          setAuthError(t.lastError ?? i18n("auth.failed"));
          t.removeEventListener("statuschange", onStatus);
          setTransport(null);
        }
      };
      t.addEventListener("statuschange", onStatus);
      setTransport(t);
    },
    [config],
  );

  if (!transport) {
    return (
      <AuthScreen
        error={authError}
        passRef={passRef}
        onSubmit={(pass) => connect(pass)}
      />
    );
  }

  return (
    <Workspace
      transport={transport}
      wasm={wasm}
      onAuthError={() => {
        setTransport(null);
        setAuthError(i18n("auth.failed"));
      }}
    />
  );
}

function AuthScreen({
  error,
  passRef,
  onSubmit,
}: {
  error: string | null;
  passRef: React.RefObject<HTMLInputElement | null>;
  onSubmit: (pass: string) => void;
}) {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const theme = themeFor(dark);
  return (
    <main
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        height: "100%",
        backgroundColor: theme.bg,
      }}
    >
      <form
        style={{
          display: "flex",
          flexDirection: "column",
          gap: "0.5em",
        }}
        onSubmit={(e) => {
          e.preventDefault();
          const v = passRef.current?.value;
          if (v) onSubmit(v);
        }}
      >
        <input
          ref={passRef}
          type="password"
          placeholder={i18n("auth.placeholder")}
          autoFocus
          style={{
            padding: "0.5em 0.75em",
            fontSize: "1em",
            border: "1px solid #444",
            outline: "none",
            width: "20em",
            fontFamily: "inherit",
            backgroundColor: theme.solidInputBg,
            color: theme.fg,
          }}
        />
        {error && (
          <output style={{ color: theme.errorText, fontSize: "0.85em" }}>
            {error}
          </output>
        )}
      </form>
    </main>
  );
}
