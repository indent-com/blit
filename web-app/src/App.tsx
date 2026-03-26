import {
  useState,
  useCallback,
  useRef,
} from "react";
import { WebSocketTransport } from "blit-react";
import type { BlitWasmModule } from "blit-react";
import { PASS_KEY, readStorage, writeStorage, wsUrl } from "./storage";
import { styles } from "./styles";
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
  return (
    <main style={{
      ...styles.authContainer,
      backgroundColor: dark ? "#1a1a1a" : "#f5f5f5",
    }}>
      <form
        style={styles.authForm}
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
            ...styles.authInput,
            backgroundColor: dark ? "#2a2a2a" : "#fff",
            color: dark ? "#eee" : "#333",
          }}
        />
        {error && <output style={styles.authError}>{error}</output>}
      </form>
    </main>
  );
}
