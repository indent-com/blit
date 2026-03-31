import { useState, useCallback, useRef, useEffect } from "react";
import { WebSocketTransport, WebTransportTransport } from "blit-react";
import type { BlitTransport } from "blit-react";
import type { BlitWasmModule } from "blit-react";
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

function createTransport(pass: string): BlitTransport {
  const certHash = wtCertHash();
  if (typeof WebTransport !== "undefined" && certHash) {
    return new WebTransportTransport(wtUrl(), pass, {
      serverCertificateHash: certHash,
    });
  }
  return new WebSocketTransport(wsUrl(), pass);
}

export function App({ wasm }: { wasm: BlitWasmModule }) {
  const savedPass = readStorage(PASS_KEY);
  const [transport, setTransport] = useState<BlitTransport | null>(null);
  const [authError, setAuthError] = useState<string | null>(null);
  const passRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!savedPass || transport) return;
    setTransport(createTransport(savedPass));
  }, [savedPass, transport]);

  const connect = useCallback(
    (pass: string) => {
      setAuthError(null);
      transport?.close();
      const t = createTransport(pass);
      const onStatus = (status: string) => {
        if (status === "connected") {
          writeStorage(PASS_KEY, pass);
          t.removeEventListener("statuschange", onStatus);
        } else if (status === "error") {
          setAuthError(t.lastError ?? i18n("auth.failed"));
          t.removeEventListener("statuschange", onStatus);
          t.close();
          setTransport(null);
        }
      };
      t.addEventListener("statuschange", onStatus);
      setTransport(t);
    },
    [transport],
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
        transport.close();
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
