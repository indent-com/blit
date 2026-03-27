import {
  useState,
  useCallback,
  useRef,
} from "react";
import { WebSocketTransport } from "blit-react";
import type { BlitWasmModule } from "blit-react";
import { PASS_KEY, readStorage, writeStorage, wsUrl } from "./storage";
import { themeFor } from "./theme";
import { Workspace } from "./Workspace";

export function App({ wasm }: { wasm: BlitWasmModule }) {
  const savedPass = readStorage(PASS_KEY);
  const [transport, setTransport] = useState<WebSocketTransport | null>(() =>
    savedPass ? new WebSocketTransport(wsUrl(), savedPass) : null,
  );
  const [authError, setAuthError] = useState<string | null>(null);
  const passRef = useRef<HTMLInputElement>(null);

  const connect = useCallback(
    (pass: string) => {
      setAuthError(null);
      transport?.close();
      const t = new WebSocketTransport(wsUrl(), pass, { reconnect: false });
      const onStatus = (status: string) => {
        if (status === "connected") {
          writeStorage(PASS_KEY, pass);
          t.removeEventListener("statuschange", onStatus);
          t.close();
          setTransport(new WebSocketTransport(wsUrl(), pass));
        } else if (status === "error") {
          setAuthError("Authentication failed");
          t.close();
          t.removeEventListener("statuschange", onStatus);
        }
      };
      t.addEventListener("statuschange", onStatus);
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
  passRef: React.RefObject<HTMLInputElement | null>;
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
