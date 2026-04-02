import { createRoot, hydrateRoot } from "react-dom/client";
import { lazy, Suspense, useState, useEffect } from "react";
import { useTheme } from "./theme";
import { Landing } from "./Landing";

const Terminal = lazy(() =>
  import("./Terminal").then((m) => ({ default: m.Terminal })),
);

function usePassphrase(): string | null {
  const [passphrase, setPassphrase] = useState<string | null>(() => {
    const hash = location.hash.slice(1);
    return hash ? decodeURIComponent(hash) : null;
  });

  useEffect(() => {
    if (passphrase) {
      history.replaceState(null, "", location.pathname + location.search);
    }
  }, [passphrase]);

  useEffect(() => {
    const onHash = () => {
      const hash = location.hash.slice(1);
      if (hash) {
        const decoded = decodeURIComponent(hash);
        setPassphrase(decoded);
        history.replaceState(null, "", location.pathname + location.search);
      }
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  return passphrase;
}

function App() {
  const passphrase = usePassphrase();
  const { theme, toggleTheme } = useTheme();
  const dark = theme === "dark";

  if (passphrase) {
    return (
      <Suspense
        fallback={
          <div
            style={{
              display: "flex",
              alignItems: "center",
              justifyContent: "center",
              height: "100%",
              background: dark ? "#0d1117" : "#ffffff",
              color: dark ? "#c9d1d9" : "#1f2328",
              fontFamily: "monospace",
            }}
          >
            Loading...
          </div>
        }
      >
        <Terminal
          passphrase={passphrase}
          dark={dark}
          onToggleTheme={toggleTheme}
        />
      </Suspense>
    );
  }
  return <Landing theme={theme} onToggleTheme={toggleTheme} />;
}

const root = document.getElementById("root")!;
const hasSSRContent = root.innerHTML.trim().length > 0;
const hash = location.hash.slice(1);

if (hasSSRContent && !hash) {
  hydrateRoot(root, <App />);
} else {
  createRoot(root).render(<App />);
}
