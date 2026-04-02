import { useState, useCallback, useRef, useEffect } from "react";
import { WebSocketTransport, WebTransportTransport } from "@blit-sh/core";
import type { BlitTransport } from "@blit-sh/core";
import type { BlitWasmModule } from "@blit-sh/core";
import {
  PASS_KEY,
  readStorage,
  writeStorage,
  wsUrl,
  wtUrl,
  wtCertHash,
} from "./storage";
import { themeFor } from "./theme";
import { t as i18n } from "./i18n";
import { Workspace } from "./Workspace";
import { createShareTransport } from "@blit-sh/core";

const DEFAULT_HUB = "wss://hub.blit.sh";

/** Read hub URL injected by CLI, or fall back to the default. */
function hubUrl(): string {
  return (window as unknown as { __blitHub?: string }).__blitHub ?? DEFAULT_HUB;
}

/**
 * Detect share mode from the URL hash. Layout state hashes start with
 * a known key (`l=`, `p=`, or `a=`). Anything else is a share passphrase.
 */
function getSharePassphrase(): string | null {
  const hash = location.hash.slice(1);
  if (!hash) return null;
  if (/^[lpa]=/.test(hash)) return null;
  return decodeURIComponent(hash);
}

function createGatewayTransport(pass: string): BlitTransport {
  const certHash = wtCertHash();
  if (typeof WebTransport !== "undefined" && certHash) {
    return new WebTransportTransport(wtUrl(), pass, {
      serverCertificateHash: certHash,
    });
  }
  return new WebSocketTransport(wsUrl(), pass);
}

export function App({ wasm }: { wasm: BlitWasmModule }) {
  const sharePassphrase = getSharePassphrase();

  // Share mode: bypass auth, connect via WebRTC
  if (sharePassphrase) {
    return <ShareApp wasm={wasm} passphrase={sharePassphrase} />;
  }

  // Gateway mode: auth screen + WebSocket/WebTransport
  return <GatewayApp wasm={wasm} />;
}

function ShareApp({
  wasm,
  passphrase,
}: {
  wasm: BlitWasmModule;
  passphrase: string;
}) {
  const [transport] = useState<BlitTransport>(() =>
    createShareTransport(hubUrl(), passphrase),
  );

  return (
    <Workspace
      transport={transport}
      wasm={wasm}
      onAuthError={() => {}}
    />
  );
}

function GatewayApp({ wasm }: { wasm: BlitWasmModule }) {
  const savedPass = readStorage(PASS_KEY);
  const [transport, setTransport] = useState<BlitTransport | null>(null);
  const [authError, setAuthError] = useState<string | null>(null);
  const passRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!savedPass || transport) return;
    setTransport(createGatewayTransport(savedPass));
  }, [savedPass, transport]);

  const connect = useCallback(
    (pass: string) => {
      setAuthError(null);
      const t = createGatewayTransport(pass);
      const onStatus = (status: string) => {
        if (status === "connected") {
          writeStorage(PASS_KEY, pass);
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
    [],
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
        writeStorage(PASS_KEY, "");
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
  passRef: React.RefObject<HTMLInputElement>;
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
