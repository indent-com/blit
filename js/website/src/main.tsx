import { createRoot } from "react-dom/client";
import { useState, useSyncExternalStore } from "react";
import { Landing } from "./Landing";
import { Terminal } from "./Terminal";

function useHash(): string {
  return useSyncExternalStore(
    (cb) => {
      window.addEventListener("hashchange", cb);
      return () => window.removeEventListener("hashchange", cb);
    },
    () => location.hash.slice(1),
  );
}

function App() {
  const hash = useHash();
  const passphrase = hash ? decodeURIComponent(hash) : null;

  if (passphrase) {
    return <Terminal passphrase={passphrase} />;
  }
  return <Landing />;
}

createRoot(document.getElementById("root")!).render(<App />);
