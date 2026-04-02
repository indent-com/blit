import { useEffect, useState, useRef, useCallback, type FormEvent } from "react";
import "./landing.css";

const INSTALL_CMD = "curl -fsS https://install.blit.sh | sh";

function JoinForm() {
  const [secret, setSecret] = useState("");

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    const trimmed = secret.trim();
    if (trimmed) {
      location.hash = encodeURIComponent(trimmed);
    }
  };

  return (
    <form className="join-form" onSubmit={handleSubmit}>
      <input
        className="join-input"
        type="text"
        placeholder="Enter a session secret"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        spellCheck={false}
        autoComplete="off"
      />
      <button className="join-btn" type="submit" disabled={!secret.trim()}>
        Join
      </button>
    </form>
  );
}
const THEME_KEY = "blit-theme";

function getInitialTheme(): "light" | "dark" {
  const stored = localStorage.getItem(THEME_KEY);
  if (stored === "light" || stored === "dark") return stored;
  return window.matchMedia("(prefers-color-scheme: dark)").matches
    ? "dark"
    : "light";
}

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const timeout = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  return (
    <button
      className="copy-btn"
      onClick={() => {
        navigator.clipboard.writeText(text);
        setCopied(true);
        clearTimeout(timeout.current);
        timeout.current = setTimeout(() => setCopied(false), 2000);
      }}
      aria-label="Copy to clipboard"
    >
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

function ThemeToggle({
  theme,
  onToggle,
}: {
  theme: "light" | "dark";
  onToggle: () => void;
}) {
  return (
    <button
      className="theme-toggle"
      onClick={onToggle}
      aria-label={`Switch to ${theme === "dark" ? "light" : "dark"} mode`}
      title={`Switch to ${theme === "dark" ? "light" : "dark"} mode`}
    >
      {theme === "dark" ? (
        <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
          <circle cx="8" cy="8" r="3.5" stroke="currentColor" strokeWidth="1.5" />
          <path d="M8 1v2M8 13v2M1 8h2M13 8h2M3.05 3.05l1.41 1.41M11.54 11.54l1.41 1.41M3.05 12.95l1.41-1.41M11.54 4.46l1.41-1.41" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
      ) : (
        <svg width="16" height="16" viewBox="0 0 16 16" fill="none">
          <path d="M14 9.2A6 6 0 0 1 6.8 2 6 6 0 1 0 14 9.2Z" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" />
        </svg>
      )}
    </button>
  );
}

export function Landing() {
  const [theme, setTheme] = useState<"light" | "dark">(getInitialTheme);

  const toggleTheme = useCallback(() => {
    setTheme((t) => {
      const next = t === "dark" ? "light" : "dark";
      localStorage.setItem(THEME_KEY, next);
      return next;
    });
  }, []);

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    document.documentElement.style.overflow = "auto";
    document.body.style.overflow = "auto";
    const root = document.getElementById("root");
    if (root) {
      root.style.height = "auto";
      root.style.overflow = "visible";
    }
    return () => {
      document.documentElement.style.overflow = "";
      document.body.style.overflow = "";
      if (root) {
        root.style.height = "";
        root.style.overflow = "";
      }
    };
  }, [theme]);

  return (
    <div className="landing">
      <header className="landing-header">
        <div className="landing-logo">
          <svg viewBox="0 0 100 100" width="28" height="28">
            <rect width="100" height="100" rx="16" fill="currentColor" className="logo-bg" />
            <text x="12" y="76" fontFamily="monospace" fontSize="72" fontWeight="bold" className="logo-text">b</text>
            <rect x="60" y="24" width="8" height="52" rx="2" className="logo-cursor" opacity="0.7" />
          </svg>
          <span className="landing-wordmark">blit</span>
        </div>
        <nav className="landing-nav">
          <a href="https://github.com/indent-com/blit" target="_blank" rel="noopener noreferrer">GitHub</a>
          <ThemeToggle theme={theme} onToggle={toggleTheme} />
        </nav>
      </header>

      <main className="landing-main">
        <section className="hero">
          <h1>Your terminal, everywhere.</h1>
          <p className="hero-sub">
            Open your terminal in any browser. Share it with a link.
            Let AI agents drive it. One binary, zero config, instant.
          </p>
          <div className="hero-install">
            <div className="install-block">
              <span className="install-prompt">$</span>
              <code>{INSTALL_CMD}</code>
              <CopyButton text={INSTALL_CMD} />
            </div>
          </div>
          <div className="hero-join">
            <span className="hero-join-label">or join a session in your browser</span>
            <JoinForm />
          </div>
        </section>

        <section className="why-section">
          <div className="why-card">
            <h3>Live state, not pixels</h3>
            <p>
              blit tracks the full parsed terminal grid and sends only what
              changed — LZ4-compressed binary diffs. The browser gets real
              structured data: select text, resize freely, type into it.
            </p>
          </div>
          <div className="why-card">
            <h3>Share with one command</h3>
            <p>
              <code>blit share</code> prints a URL. Open it anywhere.
              WebRTC punches through NATs — no SSH keys, no port forwarding,
              no tunnels. Full input for pair programming or AI agents.
            </p>
          </div>
          <div className="why-card">
            <h3>Terminals as an API</h3>
            <p>
              Non-interactive CLI for scripts and LLMs. <code>blit start</code>,{" "}
              <code>show</code>, <code>send</code>, <code>wait --pattern</code>.
              Plain text out, zero exit on success. No screen-scraping.
            </p>
          </div>
        </section>

        <section className="demo-section">
          <div className="demo-block">
            <div className="demo-label">Get started</div>
            <pre className="demo-pre"><code>{`$ curl -fsS https://install.blit.sh | sh
$ blit                    # opens a browser
$ blit share              # share via WebRTC
$ blit --ssh myhost       # remote host`}</code></pre>
          </div>
          <div className="demo-block">
            <div className="demo-label">Agent API</div>
            <pre className="demo-pre"><code>{`$ blit start -t build make -j8
1
$ blit wait 1 --pattern 'BUILD OK'
$ blit show 1
[100%] Built target app
BUILD OK`}</code></pre>
          </div>
        </section>

        <section className="embed-section">
          <h2>Drop into any React app</h2>
          <pre className="demo-pre demo-pre--wide"><code>{`import { BlitWorkspaceProvider, BlitTerminal,
         useBlitSessions, useBlitFocusedSession,
         useBlitWorkspace } from '@blit-sh/react';
import { BlitWorkspace, WebSocketTransport } from '@blit-sh/core';
import { useEffect, useMemo } from 'react';

function App({ wasm, url, passphrase }) {
  const transport = useMemo(
    () => new WebSocketTransport(url, passphrase), [url, passphrase]);
  const workspace = useMemo(
    () => new BlitWorkspace({
      wasm, connections: [{ id: "default", transport }],
    }), [wasm, transport]);
  useEffect(() => () => workspace.dispose(), [workspace]);

  return (
    <BlitWorkspaceProvider workspace={workspace}>
      <Terminal />
    </BlitWorkspaceProvider>
  );
}

function Terminal() {
  const workspace = useBlitWorkspace();
  const sessions = useBlitSessions();
  const focused = useBlitFocusedSession();

  useEffect(() => {
    if (!sessions.length)
      workspace.createSession({
        connectionId: "default", rows: 24, cols: 80,
      });
  }, [sessions.length, workspace]);

  return <BlitTerminal sessionId={focused?.id ?? null}
    style={{ width: "100%", height: "100vh" }} />;
}`}</code></pre>
        </section>
      </main>

      <footer className="landing-footer">
        <span>
          Built by{" "}
          <a href="https://indent.com" target="_blank" rel="noopener noreferrer">Indent</a>
        </span>
        <span className="footer-sep">&middot;</span>
        <a href="https://github.com/indent-com/blit" target="_blank" rel="noopener noreferrer">Source</a>
      </footer>
    </div>
  );
}
