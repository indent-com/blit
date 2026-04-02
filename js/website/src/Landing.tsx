import { useEffect, useState, useRef, useCallback, type FormEvent } from "react";
import "./landing.css";

const INSTALL_CMD = "curl -f https://install.blit.sh | sh";
const DOCKER_CMD = "docker run --rm grab/blit-demo";

function JoinForm() {
  const [secret, setSecret] = useState("");
  const [visible, setVisible] = useState(false);

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
        className={`join-input${visible ? "" : " join-input--masked"}`}
        type="text"
        placeholder="session secret"
        value={secret}
        onChange={(e) => setSecret(e.target.value)}
        spellCheck={false}
        autoComplete="off"
        data-1p-ignore
        data-lpignore="true"
        data-form-type="other"
      />
      <button
        type="button"
        className="join-visibility"
        onClick={() => setVisible((v) => !v)}
        aria-label={visible ? "Hide secret" : "Show secret"}
        tabIndex={-1}
      >
        {visible ? (
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
            <path d="M1 1l14 14M6.5 6.5a2 2 0 0 0 3 3M2.5 5.2C1.6 6.1 1 7 1 8c0 2.2 3.1 5 7 5 .8 0 1.6-.1 2.3-.3M13.5 10.8c.9-.9 1.5-1.8 1.5-2.8 0-2.2-3.1-5-7-5-.8 0-1.6.1-2.3.3" />
          </svg>
        ) : (
          <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
            <path d="M1 8c0 2.2 3.1 5 7 5s7-2.8 7-5-3.1-5-7-5-7 2.8-7 5Z" />
            <circle cx="8" cy="8" r="2" />
          </svg>
        )}
      </button>
      <button className="join-btn" type="submit" disabled={!secret.trim()}>
        Join
      </button>
    </form>
  );
}

function Arrow({ x1, y1, x2, y2 }: { x1: number; y1: number; x2: number; y2: number }) {
  return <line x1={x1} y1={y1} x2={x2} y2={y2}
    stroke="var(--dim)" strokeWidth="1.5" markerEnd="url(#arr)" />;
}

function Box({ x, y, w, h, label, sub }: { x: number; y: number; w: number; h: number; label: string; sub?: string }) {
  return (
    <g>
      <rect x={x} y={y} width={w} height={h}
        rx="6" fill="var(--surface)" stroke="var(--border)" strokeWidth="1.5" />
      <text x={x + w / 2} y={y + (sub ? h / 2 - 2 : h / 2 + 5)}
        textAnchor="middle" fill="var(--bright)" fontSize="13" fontWeight="700"
        fontFamily="'Fira Code', monospace">{label}</text>
      {sub && <text x={x + w / 2} y={y + h / 2 + 14}
        textAnchor="middle" fill="var(--dim)" fontSize="10"
        fontFamily="'Fira Code', monospace">{sub}</text>}
    </g>
  );
}

function EdgeLabel({ x, y, label }: { x: number; y: number; label: string }) {
  return <text x={x} y={y} textAnchor="middle"
    fill="var(--accent)" fontSize="11" fontWeight="600"
    fontFamily="'Fira Code', monospace">{label}</text>;
}

function TransportDiagram() {
  return (
    <div className="transport-diagrams">
      <div className="transport-diagram-card">
        <div className="transport-diagram-label">Local / gateway</div>
        <svg viewBox="0 0 520 140" className="transport-diagram" role="img"
          aria-label="Browser connects to blit via gateway over WebSocket or WebTransport">
          <defs>
            <marker id="arr" viewBox="0 0 10 7" refX="9" refY="3.5"
              markerWidth="8" markerHeight="6" orient="auto-start-reverse">
              <path d="M0 0L10 3.5L0 7z" fill="var(--dim)" />
            </marker>
          </defs>
          <Box x={10} y={45} w={100} h={50} label="Browser" />
          <Box x={210} y={45} w={100} h={50} label="Gateway" sub="blit-gateway" />
          <Box x={410} y={45} w={100} h={50} label="blit" sub="server" />

          <Arrow x1={110} y1={60} x2={208} y2={60} />
          <EdgeLabel x={160} y={54} label="WebSocket" />

          <Arrow x1={110} y1={82} x2={208} y2={82} />
          <EdgeLabel x={160} y={100} label="WebTransport" />

          <Arrow x1={310} y1={70} x2={408} y2={70} />
          <EdgeLabel x={360} y={64} label="Unix sock" />
        </svg>
      </div>

      <div className="transport-diagram-card">
        <div className="transport-diagram-label">Share (peer-to-peer)</div>
        <svg viewBox="0 0 520 140" className="transport-diagram" role="img"
          aria-label="Browser connects to blit share via WebRTC through a signaling hub">
          <defs>
            <marker id="arr2" viewBox="0 0 10 7" refX="9" refY="3.5"
              markerWidth="8" markerHeight="6" orient="auto-start-reverse">
              <path d="M0 0L10 3.5L0 7z" fill="var(--dim)" />
            </marker>
          </defs>
          <Box x={10} y={45} w={100} h={50} label="Browser" />
          <Box x={210} y={45} w={100} h={50} label="Forwarder" sub="blit share" />
          <Box x={410} y={45} w={100} h={50} label="blit" sub="server" />

          <Arrow x1={110} y1={70} x2={208} y2={70} />
          <EdgeLabel x={160} y={64} label="WebRTC" />

          <Arrow x1={310} y1={70} x2={408} y2={70} />
          <EdgeLabel x={360} y={64} label="Unix sock" />
        </svg>
      </div>

      <div className="transport-diagram-card">
        <div className="transport-diagram-label">Remote (SSH)</div>
        <svg viewBox="0 0 520 140" className="transport-diagram" role="img"
          aria-label="CLI connects to remote blit server over SSH">
          <defs>
            <marker id="arr3" viewBox="0 0 10 7" refX="9" refY="3.5"
              markerWidth="8" markerHeight="6" orient="auto-start-reverse">
              <path d="M0 0L10 3.5L0 7z" fill="var(--dim)" />
            </marker>
          </defs>
          <Box x={10} y={45} w={100} h={50} label="CLI" sub="blit --ssh" />
          <Box x={210} y={45} w={100} h={50} label="SSH" sub="socket forward" />
          <Box x={410} y={45} w={100} h={50} label="blit" sub="server" />

          <Arrow x1={110} y1={70} x2={208} y2={70} />
          <EdgeLabel x={160} y={64} label="SSH" />

          <Arrow x1={310} y1={70} x2={408} y2={70} />
          <EdgeLabel x={360} y={64} label="Unix sock" />
        </svg>
      </div>
    </div>
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
          <svg viewBox="0 0 100 100" width="24" height="24">
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
            Stream terminals to any browser. Share with a link. Let AI agents drive them.
            <br />
            One binary, zero config, instant.
          </p>

          <div className="install-group">
            <div className="install-section">
              <div className="install-heading">Install</div>
              <div className="install-row">
                <span className="prompt-char">$</span>
                <span className="install-cmd">{INSTALL_CMD}</span>
                <CopyButton text={INSTALL_CMD} />
              </div>
            </div>

            <div className="install-section">
              <div className="install-heading">Try it without installing</div>
              <div className="install-row">
                <span className="prompt-char">$</span>
                <span className="install-cmd">{DOCKER_CMD}</span>
                <CopyButton text={DOCKER_CMD} />
              </div>
            </div>
          </div>

          <div className="join-section">
            <div className="join-label">or join an existing session</div>
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

        <section className="transport-section">
          <h2>Multiple paths, same protocol</h2>
          <TransportDiagram />
        </section>

        <section className="demo-section">
          <div className="demo-block">
            <div className="demo-label">Get started</div>
            <pre className="demo-pre"><code>{`$ curl -f https://install.blit.sh | sh
$ blit            # opens a browser
$ blit share      # share via WebRTC
$ blit --ssh host # remote terminal`}</code></pre>
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
          <pre className="demo-pre demo-pre--wide"><code className="code-hl">{
}<span className="hl-kw">import</span>{" { "}
<span className="hl-fn">BlitWorkspaceProvider</span>{", "}
<span className="hl-fn">BlitTerminal</span>{",\n         "}
<span className="hl-fn">useBlitFocusedSession</span>{" } "}
<span className="hl-kw">from</span> <span className="hl-str">'@blit-sh/react'</span>{";\n"}
<span className="hl-kw">import</span>{" { "}
<span className="hl-fn">BlitWorkspace</span>{" } "}
<span className="hl-kw">from</span> <span className="hl-str">'@blit-sh/core'</span>{";\n\n"}
<span className="hl-cm">{"// ● Create a workspace with a WebSocket connection"}</span>{"\n"}
<span className="hl-kw">const</span>{" workspace = "}
<span className="hl-kw">new</span> <span className="hl-fn">BlitWorkspace</span>{"({\n  wasm,\n  connections: [{\n    id: "}
<span className="hl-str">"ws"</span>{", type: "}
<span className="hl-str">"websocket"</span>{", url, passphrase,\n  }],\n});\n\n"}
<span className="hl-cm">{"// ● Render — that's it"}</span>{"\n"}
{"<"}
<span className="hl-fn">BlitWorkspaceProvider</span> <span className="hl-attr">workspace</span>{"={workspace}>\n  <"}
<span className="hl-fn">BlitTerminal</span> <span className="hl-attr">sessionId</span>{"={"}
<span className="hl-fn">useBlitFocusedSession</span>{"()?.id ?? "}
<span className="hl-kw">null</span>{"} />\n</"}
<span className="hl-fn">BlitWorkspaceProvider</span>{">"}
</code></pre>
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
