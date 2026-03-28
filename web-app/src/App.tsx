import {
  useState,
  useCallback,
  useRef,
  useEffect,
} from "react";
import {
  WebSocketTransport,
  WebTransportTransport,
} from "blit-react";
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
import { Workspace } from "./Workspace";

function createBestTransport(
  pass: string,
  onFallbackToWebSocket: () => void,
): BlitTransport {
  const certHash = wtCertHash();
  const canTryWebTransport = typeof WebTransport !== "undefined";
  if (!canTryWebTransport || !certHash) {
    return new WebSocketTransport(wsUrl(), pass);
  }
  const wt = new WebTransportTransport(wtUrl(), pass, {
    serverCertificateHash: certHash,
  });
  let connectedOnce = false;
  const onStatus = (status: string) => {
    if (status === "connected") {
      connectedOnce = true;
      wt.removeEventListener("statuschange", onStatus);
      return;
    }
    if (!connectedOnce && (status === "error" || status === "disconnected")) {
      wt.removeEventListener("statuschange", onStatus);
      wt.close();
      console.warn("WebTransport failed (cert may have changed), falling back to WebSocket");
      onFallbackToWebSocket();
    }
  };
  wt.addEventListener("statuschange", onStatus);
  return wt;
}

export function App({ wasm }: { wasm: BlitWasmModule }) {
  const savedPass = readStorage(PASS_KEY);
  const [transport, setTransport] = useState<BlitTransport | null>(null);
  const [authError, setAuthError] = useState<string | null>(null);
  const passRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (!savedPass || transport) return;
    setTransport(
      createBestTransport(savedPass, () =>
        setTransport(new WebSocketTransport(wsUrl(), savedPass)),
      ),
    );
  }, [savedPass, transport]);

  const connect = useCallback(
    (pass: string) => {
      setAuthError(null);
      transport?.close();
      // Auth check uses plain WebSocket (no QUIC dependency)
      const t = new WebSocketTransport(wsUrl(), pass, { reconnect: false });
      const onStatus = (status: string) => {
        if (status === "connected") {
          writeStorage(PASS_KEY, pass);
          t.removeEventListener("statuschange", onStatus);
          t.close();
          setTransport(
            createBestTransport(pass, () => {
              const ws = new WebSocketTransport(wsUrl(), pass);
              setTransport(ws);
              // connect() will be called when the workspace attaches the connection
            }),
          );
        } else if (status === "error") {
          setAuthError("Authentication failed");
          t.close();
          t.removeEventListener("statuschange", onStatus);
        }
      };
      t.addEventListener("statuschange", onStatus);
      t.connect();
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

  return <Workspace transport={transport} wasm={wasm} onAuthError={() => {
    transport.close();
    writeStorage(PASS_KEY, "");
    setTransport(null);
    setAuthError("Authentication failed");
  }} />;
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
    <main style={{
      display: "flex",
      alignItems: "center",
      justifyContent: "center",
      height: "100%",
      backgroundColor: theme.bg,
    }}>
      <form
        style={{
          display: "flex",
          flexDirection: "column",
          gap: 8,
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
          placeholder="passphrase"
          autoFocus
          style={{
            padding: "8px 12px",
            fontSize: 16,
            border: "1px solid #444",
            outline: "none",
            width: 260,
            fontFamily: "inherit",
            backgroundColor: theme.solidInputBg,
            color: theme.fg,
          }}
        />
        {error && <output style={{ color: theme.errorText, fontSize: 13 }}>{error}</output>}
      </form>
    </main>
  );
}
