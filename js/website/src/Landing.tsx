import { useEffect, useState, useRef } from "react";
import "./landing.css";

const INSTALL_CMD = "curl -fsSL https://install.blit.sh | sh";

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

export function Landing() {
  const [dark, setDark] = useState(
    window.matchMedia("(prefers-color-scheme: dark)").matches,
  );

  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handler = (e: MediaQueryListEvent) => setDark(e.matches);
    mq.addEventListener("change", handler);
    return () => mq.removeEventListener("change", handler);
  }, []);

  useEffect(() => {
    document.documentElement.setAttribute(
      "data-theme",
      dark ? "dark" : "light",
    );
    document.body.style.overflow = "auto";
    return () => {
      document.body.style.overflow = "";
    };
  }, [dark]);

  return (
    <div className="landing">
      <header className="landing-header">
        <div className="landing-logo">
          <svg viewBox="0 0 100 100" width="32" height="32">
            <rect width="100" height="100" rx="16" fill="currentColor" className="logo-bg" />
            <text x="12" y="76" fontFamily="monospace" fontSize="72" fontWeight="bold" className="logo-text">b</text>
            <rect x="60" y="24" width="8" height="52" rx="2" className="logo-cursor" opacity="0.7" />
          </svg>
          <span className="landing-wordmark">blit</span>
        </div>
        <nav className="landing-nav">
          <a href="https://github.com/nichochar/blit" target="_blank" rel="noopener noreferrer">GitHub</a>
        </nav>
      </header>

      <main className="landing-main">
        <section className="hero">
          <h1>Terminal streaming for&nbsp;browsers and&nbsp;AI&nbsp;agents</h1>
          <p className="hero-sub">
            A single binary that hosts PTYs, computes binary diffs with LZ4
            compression, and streams parsed terminal state to browser clients
            over WebSocket, WebTransport, or WebRTC.
          </p>

          <div className="install-block">
            <code>{INSTALL_CMD}</code>
            <CopyButton text={INSTALL_CMD} />
          </div>
        </section>

        <section className="features">
          <div className="feature">
            <h3>WebGL rendering</h3>
            <p>
              GPU-accelerated terminal rendering with a glyph atlas and
              background rect shaders. Smooth at any font size, any DPR.
            </p>
          </div>
          <div className="feature">
            <h3>Binary diffs</h3>
            <p>
              Only changed cells are sent. LZ4-compressed frames with
              copy-rect and patch-cells ops minimize bandwidth.
            </p>
          </div>
          <div className="feature">
            <h3>Per-client backpressure</h3>
            <p>
              Each client reports its render rate. The server paces updates
              so fast output never overwhelms slow connections.
            </p>
          </div>
          <div className="feature">
            <h3>Multiple transports</h3>
            <p>
              Unix socket for local, WebSocket for tunnels, WebTransport
              (QUIC) for low-latency, WebRTC for NAT traversal.
            </p>
          </div>
          <div className="feature">
            <h3>Agent CLI</h3>
            <p>
              Non-interactive subcommands (<code>list</code>, <code>start</code>,{" "}
              <code>show</code>, <code>send</code>, <code>wait</code>) for
              scripts and LLM agents.
            </p>
          </div>
          <div className="feature">
            <h3>React embedding</h3>
            <p>
              Drop <code>@blit-sh/react</code> into any React app.
              Workspace provider, hooks, and a terminal component.
            </p>
          </div>
        </section>

        <section className="quickstart">
          <h2>Quick start</h2>
          <div className="code-block">
            <pre><code>{`# Install
curl -fsSL https://install.blit.sh | sh

# Start a server
blit server

# Open in browser
open http://localhost:7681

# Share a session over WebRTC
blit share

# Use from an AI agent
blit start bash
blit send 0 "echo hello"
blit show 0`}</code></pre>
          </div>
        </section>
      </main>

      <footer className="landing-footer">
        <p>
          Built by{" "}
          <a href="https://indent.com" target="_blank" rel="noopener noreferrer">
            Indent
          </a>
        </p>
      </footer>
    </div>
  );
}
