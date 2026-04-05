mod agent;
mod interactive;
mod transport;

use clap::{Args, Parser, Subcommand};

// ---------------------------------------------------------------------------
// Destination URI parsing
// ---------------------------------------------------------------------------

/// A parsed destination: where to connect.
#[derive(Clone, Debug)]
pub enum Destination {
    /// Local blit-server (auto-start if needed).
    Local,
    /// SSH to a remote host.
    Ssh {
        user: Option<String>,
        host: String,
        socket: Option<String>,
    },
    /// Raw TCP connection.
    Tcp(String),
    /// Explicit Unix socket path.
    Socket(String),
}

/// A destination with a display name.
#[derive(Clone, Debug)]
pub struct NamedDestination {
    pub name: String,
    pub dest: Destination,
}

/// Parse a single destination URI string.
///
/// Grammar:
///   [name=]uri
///   uri = "ssh:" [user "@"] host [":" socket_path]
///       | "tcp:" host ":" port
///       | "socket:" path
///       | "local"
///
/// Auto-derived names:
///   ssh:alice@rabbit:/tmp/blit.sock -> "rabbit"
///   tcp:10.0.0.5:3264              -> "10.0.0.5"
///   socket:/tmp/blit.sock          -> "blit.sock"
///   local                          -> "local"
fn parse_destination(s: &str) -> Result<NamedDestination, String> {
    // Check for explicit name: "name=uri"
    let (explicit_name, uri) = if let Some(eq) = s.find('=') {
        let name = &s[..eq];
        let rest = &s[eq + 1..];
        if name.is_empty() {
            return Err(format!("empty destination name in '{s}'"));
        }
        (Some(name.to_string()), rest)
    } else {
        (None, s)
    };

    if uri == "local" {
        return Ok(NamedDestination {
            name: explicit_name.unwrap_or_else(|| "local".into()),
            dest: Destination::Local,
        });
    }

    if let Some(rest) = uri.strip_prefix("ssh:") {
        if rest.is_empty() {
            return Err("ssh: destination requires a host".into());
        }
        // Reject hosts starting with '-' to prevent SSH option injection.
        let host_check = rest.split('@').next_back().unwrap_or(rest);
        let host_check = host_check.split(':').next().unwrap_or(host_check);
        if host_check.starts_with('-') {
            return Err("invalid ssh host: must not start with '-'".to_string());
        }
        // Split off socket path at first ':' after user@ (same convention as scp)
        let colon_start = rest.find('@').map(|a| a + 1).unwrap_or(0);
        let (host_part, socket) = if let Some(rel) = rest[colon_start..].find(':') {
            let pos = colon_start + rel;
            let path = &rest[pos + 1..];
            if path.is_empty() {
                (rest, None)
            } else {
                (&rest[..pos], Some(path.to_string()))
            }
        } else {
            (rest, None)
        };
        // Split user@host
        let (user, host) = if let Some(at) = host_part.rfind('@') {
            (
                Some(host_part[..at].to_string()),
                host_part[at + 1..].to_string(),
            )
        } else {
            (None, host_part.to_string())
        };
        let auto_name = host.clone();
        return Ok(NamedDestination {
            name: explicit_name.unwrap_or(auto_name),
            dest: Destination::Ssh { user, host, socket },
        });
    }

    if let Some(rest) = uri.strip_prefix("tcp:") {
        if rest.is_empty() {
            return Err("tcp: destination requires host:port".into());
        }
        let auto_name = rest.split(':').next().unwrap_or(rest).to_string();
        return Ok(NamedDestination {
            name: explicit_name.unwrap_or(auto_name),
            dest: Destination::Tcp(rest.to_string()),
        });
    }

    if let Some(rest) = uri.strip_prefix("socket:") {
        if rest.is_empty() {
            return Err("socket: destination requires a path".into());
        }
        let auto_name = std::path::Path::new(rest)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("socket")
            .to_string();
        return Ok(NamedDestination {
            name: explicit_name.unwrap_or(auto_name),
            dest: Destination::Socket(rest.to_string()),
        });
    }

    Err(format!(
        "unknown destination URI '{uri}' (expected ssh:, tcp:, socket:, or local)"
    ))
}

/// Parse positional destination URIs into a Vec. Deduplicates names.
/// Returns local as default when no URIs are given.
fn parse_destinations(uris: &[String]) -> Vec<NamedDestination> {
    if uris.is_empty() {
        return vec![NamedDestination {
            name: "local".into(),
            dest: Destination::Local,
        }];
    }
    let mut seen_names = std::collections::HashSet::new();
    let mut result = Vec::new();
    for s in uris {
        match parse_destination(s) {
            Ok(mut nd) => {
                let base = nd.name.clone();
                let mut suffix = 2;
                while seen_names.contains(&nd.name) {
                    nd.name = format!("{base}-{suffix}");
                    suffix += 1;
                }
                seen_names.insert(nd.name.clone());
                result.push(nd);
            }
            Err(e) => {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
    }
    result
}

#[derive(Parser)]
#[command(
    name = "blit",
    version,
    about = "Terminal streaming for browsers and AI agents",
    long_about = "Terminal streaming for browsers and AI agents.\n\n\
        blit hosts PTYs and streams them to browsers over WebSocket, WebTransport, or WebRTC.\n\
        It also exposes every terminal operation as a CLI subcommand for scripts and LLM agents.\n\n\
        Quick start:\n  \
          blit open              Open the terminal UI in a browser\n  \
          blit share             Share a terminal session via WebRTC\n  \
          blit start htop        Start a PTY and print its session ID\n  \
          blit show 1            Dump current visible terminal text\n  \
          blit learn             Print the full CLI reference\n  \
          blit --help            Show this help",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(flatten)]
    connect: ConnectOpts,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct ConnectOpts {
    /// Connect to a specific Unix socket
    #[arg(long, short = 's', global = true, conflicts_with = "ssh")]
    socket: Option<String>,

    /// Connect via raw TCP (HOST:PORT)
    #[arg(long, global = true)]
    tcp: Option<String>,

    /// Connect via SSH to a remote host ([user@]host[:/path/to/socket])
    #[arg(long, global = true, conflicts_with = "socket")]
    ssh: Option<String>,

    /// Connect via WebRTC to a shared session (passphrase)
    #[arg(long, global = true)]
    passphrase: Option<String>,

    /// Signaling hub URL
    #[arg(long, global = true, env = "BLIT_HUB", default_value = blit_webrtc_forwarder::DEFAULT_HUB_URL)]
    hub: String,
}

#[derive(Subcommand)]
enum Command {
    /// Open the terminal UI in the browser
    ///
    /// With no arguments, connects to the local blit-server.
    /// Pass one or more destination URIs to open multiple connections:
    ///
    ///   blit open ssh:rabbit ssh:fox          # two SSH hosts
    ///   blit open ssh:alice@rabbit local      # SSH + local
    ///   blit open prod=ssh:prod.example.com   # explicit name
    ///
    /// URI formats:
    ///   ssh:[user@]host[:/socket/path]
    ///   tcp:host:port
    ///   socket:/path/to/sock
    ///   local
    Open {
        /// Bind browser UI to a specific port (default: random)
        #[arg(long)]
        port: Option<u16>,

        /// Destination URIs (e.g. ssh:host, tcp:host:port, socket:/path, local)
        destinations: Vec<String>,
    },

    /// Share a terminal session via WebRTC
    Share {
        /// Passphrase for the session (default: random)
        #[arg(long, env = "BLIT_PASSPHRASE")]
        passphrase: Option<String>,

        /// Don't print the sharing URL
        #[arg(long)]
        quiet: bool,

        /// Print detailed connection diagnostics to stderr
        #[arg(long)]
        verbose: bool,
    },

    /// Print the full CLI reference (usage guide for scripts and LLM agents)
    Learn,

    /// Start a new terminal session and print its ID
    Start {
        /// Command to run (defaults to $SHELL or /bin/sh)
        command: Vec<String>,

        /// Session tag / label
        #[arg(long, short = 't')]
        tag: Option<String>,

        /// Terminal rows
        #[arg(long, default_value = "24")]
        rows: u16,

        /// Terminal columns
        #[arg(long, default_value = "80")]
        cols: u16,

        /// Block until the process exits (requires --timeout)
        #[arg(long, requires = "timeout")]
        wait: bool,

        /// Maximum seconds to wait (only with --wait)
        #[arg(long)]
        timeout: Option<u64>,
    },

    /// Wait for a session to exit or match a pattern.
    ///
    /// Without --pattern, blocks until the PTY process exits and returns
    /// its exit code. With --pattern, subscribes to output and exits when
    /// the regex matches a line produced after the wait began.
    Wait {
        /// Session ID
        id: u16,

        /// Maximum seconds to wait before giving up (exit code 124)
        #[arg(long)]
        timeout: u64,

        /// Regex pattern to match against new output lines
        #[arg(long)]
        pattern: Option<String>,
    },

    /// Close a session
    Close {
        /// Session ID
        id: u16,
    },

    /// List all terminal sessions (TSV: ID, TAG, TITLE, STATUS)
    List,

    /// Print the current visible text of a session
    Show {
        /// Session ID
        id: u16,

        /// Include ANSI color/style escape sequences in output
        #[arg(long)]
        ansi: bool,

        /// Resize to this many rows before capturing
        #[arg(long)]
        rows: Option<u16>,

        /// Resize to this many columns before capturing
        #[arg(long)]
        cols: Option<u16>,
    },

    /// Print scrollback + viewport text.
    ///
    /// Without position flags, prints everything. Use --from-beginning or
    /// --from-end to set a starting offset, and --limit to cap the output.
    History {
        /// Session ID
        id: u16,

        /// Start N lines from the top (oldest = 0)
        #[arg(long, conflicts_with = "from_end")]
        from_start: Option<u32>,

        /// Start N lines from the bottom (newest = 0)
        #[arg(long, conflicts_with = "from_start")]
        from_end: Option<u32>,

        /// Maximum number of lines to return
        #[arg(long)]
        limit: Option<u32>,

        /// Include ANSI color/style escape sequences in output
        #[arg(long)]
        ansi: bool,

        /// Resize to this many rows before capturing
        #[arg(long)]
        rows: Option<u16>,

        /// Resize to this many columns before capturing
        #[arg(long)]
        cols: Option<u16>,
    },

    /// Send input to a session.
    ///
    /// Supports C-style escapes: \n \r \t \\ \0 \xHH.
    /// To control interactive programs like vim:
    ///   blit send 3 '\x1b:wq\n'
    ///   printf '\x1b:wq\n' | blit send 3 -
    Send {
        /// Session ID
        id: u16,

        /// Text to send (use - to read from stdin)
        text: String,
    },

    /// Restart an exited session (re-runs the original command)
    Restart {
        /// Session ID
        id: u16,
    },

    /// Send a signal to a session's leader process
    Kill {
        /// Session ID
        id: u16,

        /// Signal name or number (e.g. TERM, KILL, INT, 9)
        #[arg(default_value = "TERM")]
        signal: String,
    },

    /// Run the blit terminal multiplexer server
    Server {
        /// Shell flags (default: li, or set BLIT_SHELL_FLAGS)
        #[arg(long)]
        shell_flags: Option<String>,

        /// Scrollback buffer size in lines
        #[arg(long)]
        scrollback: Option<usize>,

        /// Accept clients via fd-passing on this file descriptor (Unix only)
        #[cfg(unix)]
        #[arg(long)]
        fd_channel: Option<i32>,
    },

    /// Install blit on a remote host via SSH
    Install {
        /// SSH target ([user@]host)
        host: String,
    },

    /// Upgrade blit to the latest version
    Upgrade,

    /// List all compositor surfaces (TSV: ID, TITLE, SIZE, APP_ID)
    Surfaces,

    /// Capture a screenshot of a surface
    Capture {
        /// Surface ID
        id: u16,

        /// Output file path (default: surface-<id>.png). Format is inferred
        /// from the extension (.png or .avif) unless --format is given.
        #[arg(short, long)]
        output: Option<String>,

        /// Image format: png or avif (default: inferred from --output, else png)
        #[arg(short, long)]
        format: Option<String>,

        /// Quality: 0 = lossless, 1–100 = lossy (applies to AVIF only)
        #[arg(short, long, default_value_t = 0)]
        quality: u8,

        /// Resize the surface to this width (pixels) before capturing
        #[arg(long)]
        width: Option<u16>,

        /// Resize the surface to this height (pixels) before capturing
        #[arg(long)]
        height: Option<u16>,
    },

    /// Click at coordinates on a surface
    Click {
        /// Surface ID
        id: u16,

        /// X coordinate (pixels)
        x: u16,

        /// Y coordinate (pixels)
        y: u16,

        /// Mouse button: left, right, or middle [default: left]
        #[arg(long, default_value = "left")]
        button: String,
    },

    /// Send a key press to a surface (e.g. Return, Escape, a, ctrl+a)
    Key {
        /// Surface ID
        id: u16,

        /// Key name (e.g. a, Return, Escape, F1, ctrl+a, shift+Tab)
        key: String,
    },

    /// Type text into a surface (xdotool-style: {Return}, {ctrl+a} for special keys)
    Type {
        /// Surface ID
        id: u16,

        /// Text to type
        text: String,
    },
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let cli = Cli::parse();

    match cli.command {
        Command::Server {
            shell_flags,
            scrollback,
            #[cfg(unix)]
            fd_channel,
        } => {
            let ipc_path = cli
                .connect
                .socket
                .unwrap_or_else(blit_server::default_ipc_path);

            #[cfg(unix)]
            let shell_default = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            #[cfg(windows)]
            let shell_default = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());

            #[cfg(unix)]
            let flags_default = "li";
            #[cfg(windows)]
            let flags_default = "";

            let config = blit_server::Config {
                shell: shell_default,
                shell_flags: shell_flags
                    .or_else(|| std::env::var("BLIT_SHELL_FLAGS").ok())
                    .unwrap_or_else(|| flags_default.into()),
                scrollback: scrollback
                    .or_else(|| {
                        std::env::var("BLIT_SCROLLBACK")
                            .ok()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(10_000),
                ipc_path,
                surface_encoders: blit_server::SurfaceEncoderPreference::defaults(),
                surface_quality: std::env::var("BLIT_SURFACE_QUALITY")
                    .ok()
                    .and_then(|v| blit_server::SurfaceQuality::parse(&v))
                    .unwrap_or_default(),
                vaapi_device: std::env::var("BLIT_VAAPI_DEVICE")
                    .unwrap_or_else(|_| "/dev/dri/renderD128".into()),
                #[cfg(unix)]
                fd_channel: fd_channel.or_else(|| {
                    std::env::var("BLIT_FD_CHANNEL")
                        .ok()
                        .and_then(|s| s.parse().ok())
                }),
                verbose: false,
                max_connections: 0,
                max_ptys: 0,
            };
            blit_server::run(config).await;
        }
        Command::Install { host } => {
            if let Err(e) = cmd_install(&host).await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Upgrade => {
            if let Err(e) = cmd_upgrade().await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Share {
            passphrase,
            quiet,
            verbose,
        } => {
            let signal_url = blit_webrtc_forwarder::normalize_hub(&cli.connect.hub);
            let passphrase = passphrase.unwrap_or_else(|| {
                use rand::RngExt as _;
                const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz234567";
                let mut rng = rand::rng();
                let bytes: [u8; 26] = rng.random();
                bytes
                    .iter()
                    .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
                    .collect()
            });

            let sock_path = cli
                .connect
                .socket
                .clone()
                .unwrap_or_else(blit_server::default_ipc_path);
            if let Err(e) = transport::ensure_local_server(&sock_path).await {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }

            blit_webrtc_forwarder::run(blit_webrtc_forwarder::Config {
                sock_path,
                signal_url,
                passphrase,
                message_override: None,
                quiet,
                verbose,
            })
            .await;
        }
        cmd @ (Command::List
        | Command::Start { .. }
        | Command::Show { .. }
        | Command::History { .. }
        | Command::Send { .. }
        | Command::Restart { .. }
        | Command::Kill { .. }
        | Command::Close { .. }
        | Command::Wait { .. }
        | Command::Surfaces
        | Command::Capture { .. }
        | Command::Click { .. }
        | Command::Key { .. }
        | Command::Type { .. }) => {
            let conn = &cli.connect;
            let transport = match transport::connect(
                &conn.socket,
                &conn.tcp,
                &conn.ssh,
                &conn.passphrase,
                &conn.hub,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("blit: {e}");
                    std::process::exit(1);
                }
            };
            let result = match cmd {
                Command::List => agent::cmd_list(transport).await,
                Command::Start {
                    command,
                    tag,
                    rows,
                    cols,
                    wait,
                    timeout,
                } => {
                    let start_result = agent::cmd_start(transport, tag, command, rows, cols).await;
                    if wait {
                        let pty_id = match start_result {
                            Ok(id) => id,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        };
                        let transport2 = match transport::connect(
                            &conn.socket,
                            &conn.tcp,
                            &conn.ssh,
                            &conn.passphrase,
                            &conn.hub,
                        )
                        .await
                        {
                            Ok(t) => t,
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        };
                        match agent::cmd_wait(transport2, pty_id, timeout.unwrap(), None).await {
                            Ok(code) => std::process::exit(code),
                            Err(e) => {
                                eprintln!("blit: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    start_result.map(|_| ())
                }
                Command::Show {
                    id,
                    ansi,
                    rows,
                    cols,
                } => agent::cmd_show(transport, id, ansi, rows, cols).await,
                Command::History {
                    id,
                    from_start,
                    from_end,
                    limit,
                    ansi,
                    rows,
                    cols,
                } => {
                    let size = agent::capture_size(rows, cols);
                    agent::cmd_history(transport, id, from_start, from_end, limit, ansi, size).await
                }
                Command::Send { id, text } => {
                    let text = if text == "-" {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf).unwrap_or(0);
                        buf
                    } else {
                        text
                    };
                    agent::cmd_send(transport, id, text).await
                }
                Command::Restart { id } => agent::cmd_restart(transport, id).await,
                Command::Kill { id, signal } => agent::cmd_kill(transport, id, &signal).await,
                Command::Close { id } => agent::cmd_close(transport, id).await,
                Command::Wait {
                    id,
                    timeout,
                    pattern,
                } => match agent::cmd_wait(transport, id, timeout, pattern).await {
                    Ok(code) => std::process::exit(code),
                    Err(e) => {
                        eprintln!("blit: {e}");
                        std::process::exit(1);
                    }
                },
                Command::Surfaces => agent::cmd_surfaces(transport).await,
                Command::Capture {
                    id,
                    output,
                    format,
                    quality,
                    width,
                    height,
                } => {
                    agent::cmd_capture(transport, id, output, format, quality, width, height).await
                }
                Command::Click { id, x, y, button } => {
                    agent::cmd_click(transport, id, x, y, &button).await
                }
                Command::Key { id, key } => agent::cmd_key(transport, id, &key).await,
                Command::Type { id, text } => agent::cmd_type(transport, id, &text).await,
                _ => unreachable!(),
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Open { port, destinations } => {
            let conn = &cli.connect;
            if conn.ssh.is_some() || conn.tcp.is_some() || conn.socket.is_some() {
                eprintln!(
                    "blit: --ssh/--tcp/--socket are not supported with open; use destination URIs instead\n\
                     \n  blit open ssh:host\n  blit open tcp:host:port\n  blit open socket:/path"
                );
                std::process::exit(1);
            }
            if let Some(passphrase) = &conn.passphrase {
                if !destinations.is_empty() {
                    eprintln!("blit: cannot combine --passphrase with destination URIs");
                    std::process::exit(1);
                }
                let hub = blit_webrtc_forwarder::normalize_hub(&conn.hub);
                interactive::run_browser_share(passphrase, &hub, port).await;
            } else {
                let dests = parse_destinations(&destinations);
                interactive::run_browser(dests, port).await;
            }
        }
        Command::Learn => {
            print!("{}", include_str!("learn.md"));
        }
    }
}

async fn cmd_install(host: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Reject hosts starting with '-' to prevent SSH option injection.
    let host_check = host.split('@').next_back().unwrap_or(host);
    if host_check.starts_with('-') {
        return Err(format!("invalid ssh host '{host}': must not start with '-'").into());
    }
    let ssh_base = |host: &str| {
        let mut cmd = std::process::Command::new("ssh");
        cmd.arg("-T")
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPath=/tmp/blit-ssh-%r@%h:%p")
            .arg("-o")
            .arg("ControlPersist=300")
            .arg(host);
        cmd
    };

    let detect = ssh_base(host)
        .arg("--")
        .arg("uname -s 2>/dev/null || echo WINDOWS")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()?;

    if !detect.status.success() {
        return Err("ssh failed to detect remote OS".into());
    }

    let os = String::from_utf8_lossy(&detect.stdout)
        .trim()
        .to_uppercase();

    let install_cmd = if os.contains("WINDOWS")
        || os.contains("MINGW")
        || os.contains("MSYS")
        || os.contains("CYGWIN")
    {
        r#"powershell -ExecutionPolicy Bypass -Command "irm https://install.blit.sh/install.ps1 | iex""#.to_string()
    } else {
        r#"sh -c 'if command -v curl >/dev/null 2>&1; then curl -sf https://install.blit.sh | sh; elif command -v wget >/dev/null 2>&1; then wget -qO- https://install.blit.sh | sh; else echo "error: neither curl nor wget found" >&2; exit 1; fi'"#.to_string()
    };

    eprintln!("installing blit on {host} ({os})...");

    let status = ssh_base(host)
        .arg("--")
        .arg(&install_cmd)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(format!("remote install exited with {status}").into());
    }

    Ok(())
}

async fn cmd_upgrade() -> Result<(), Box<dyn std::error::Error>> {
    let exe_path = std::env::current_exe()?;
    let install_dir = exe_path
        .parent()
        .ok_or("cannot determine binary directory")?;

    let install_url = if cfg!(windows) {
        "https://install.blit.sh/install.ps1"
    } else {
        "https://install.blit.sh"
    };
    let script = reqwest::get(install_url)
        .await?
        .error_for_status()?
        .text()
        .await?;

    let ext = if cfg!(windows) { "ps1" } else { "sh" };
    let tmp = std::env::temp_dir().join(format!("blit-install-{}.{}", std::process::id(), ext));
    std::fs::write(&tmp, &script)?;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("sh")
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", install_dir)
            .exec();
        Err(format!("exec failed: {err}").into())
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-File"])
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", install_dir)
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(not(any(unix, windows)))]
    {
        let status = std::process::Command::new("sh")
            .arg(&tmp)
            .env("BLIT_INSTALL_DIR", install_dir)
            .status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;

    #[test]
    fn parse_local() {
        let nd = parse_destination("local").unwrap();
        assert_eq!(nd.name, "local");
        assert!(matches!(nd.dest, Destination::Local));
    }

    #[test]
    fn parse_ssh_host_only() {
        let nd = parse_destination("ssh:rabbit").unwrap();
        assert_eq!(nd.name, "rabbit");
        match &nd.dest {
            Destination::Ssh { user, host, socket } => {
                assert_eq!(user, &None);
                assert_eq!(host, "rabbit");
                assert_eq!(socket, &None);
            }
            _ => panic!("expected Ssh"),
        }
    }

    #[test]
    fn parse_ssh_user_host() {
        let nd = parse_destination("ssh:alice@rabbit").unwrap();
        assert_eq!(nd.name, "rabbit");
        match &nd.dest {
            Destination::Ssh { user, host, socket } => {
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(host, "rabbit");
                assert_eq!(socket, &None);
            }
            _ => panic!("expected Ssh"),
        }
    }

    #[test]
    fn parse_ssh_host_socket() {
        let nd = parse_destination("ssh:rabbit:/tmp/blit.sock").unwrap();
        assert_eq!(nd.name, "rabbit");
        match &nd.dest {
            Destination::Ssh { user, host, socket } => {
                assert_eq!(user, &None);
                assert_eq!(host, "rabbit");
                assert_eq!(socket.as_deref(), Some("/tmp/blit.sock"));
            }
            _ => panic!("expected Ssh"),
        }
    }

    #[test]
    fn parse_ssh_user_host_socket() {
        let nd = parse_destination("ssh:alice@rabbit:/run/blit/alice.sock").unwrap();
        assert_eq!(nd.name, "rabbit");
        match &nd.dest {
            Destination::Ssh { user, host, socket } => {
                assert_eq!(user.as_deref(), Some("alice"));
                assert_eq!(host, "rabbit");
                assert_eq!(socket.as_deref(), Some("/run/blit/alice.sock"));
            }
            _ => panic!("expected Ssh"),
        }
    }

    #[test]
    fn parse_tcp() {
        let nd = parse_destination("tcp:10.0.0.5:3264").unwrap();
        assert_eq!(nd.name, "10.0.0.5");
        match &nd.dest {
            Destination::Tcp(addr) => assert_eq!(addr, "10.0.0.5:3264"),
            _ => panic!("expected Tcp"),
        }
    }

    #[test]
    fn parse_socket() {
        let nd = parse_destination("socket:/tmp/blit.sock").unwrap();
        assert_eq!(nd.name, "blit.sock");
        match &nd.dest {
            Destination::Socket(p) => assert_eq!(p, "/tmp/blit.sock"),
            _ => panic!("expected Socket"),
        }
    }

    #[test]
    fn parse_explicit_name() {
        let nd = parse_destination("prod=ssh:prod-server.example.com").unwrap();
        assert_eq!(nd.name, "prod");
        match &nd.dest {
            Destination::Ssh { host, .. } => assert_eq!(host, "prod-server.example.com"),
            _ => panic!("expected Ssh"),
        }
    }

    #[test]
    fn parse_explicit_name_local() {
        let nd = parse_destination("mybox=local").unwrap();
        assert_eq!(nd.name, "mybox");
        assert!(matches!(nd.dest, Destination::Local));
    }

    #[test]
    fn parse_unknown_scheme() {
        assert!(parse_destination("ftp:host").is_err());
    }

    #[test]
    fn parse_empty_ssh() {
        assert!(parse_destination("ssh:").is_err());
    }

    #[test]
    fn parse_destinations_default_local() {
        let dests = parse_destinations(&[]);
        assert_eq!(dests.len(), 1);
        assert_eq!(dests[0].name, "local");
        assert!(matches!(dests[0].dest, Destination::Local));
    }

    #[test]
    fn parse_destinations_multiple() {
        let dests = parse_destinations(&["ssh:rabbit".into(), "local".into()]);
        assert_eq!(dests.len(), 2);
        assert_eq!(dests[0].name, "rabbit");
        assert_eq!(dests[1].name, "local");
    }

    #[test]
    fn parse_destinations_deduplicates_names() {
        let dests = parse_destinations(&["ssh:rabbit".into(), "ssh:rabbit".into()]);
        assert_eq!(dests.len(), 2);
        assert_eq!(dests[0].name, "rabbit");
        assert_eq!(dests[1].name, "rabbit-2");
    }
}
