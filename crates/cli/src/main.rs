mod agent;
mod interactive;
mod transport;

use clap::{Args, Parser, Subcommand};

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
    #[arg(long, short = 's', global = true)]
    socket: Option<String>,

    /// Connect via raw TCP (HOST:PORT)
    #[arg(long, global = true)]
    tcp: Option<String>,

    /// Connect via SSH to a remote host
    #[arg(long, global = true)]
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
    /// Open the terminal UI in the browser (default) or terminal
    Open {
        /// Render to terminal instead of opening browser (legacy mode)
        #[arg(long, conflicts_with = "port")]
        console: bool,

        /// Bind browser UI to a specific port (default: random)
        #[arg(long, conflicts_with = "console")]
        port: Option<u16>,
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
    Install,

    /// Upgrade blit to the latest version
    Upgrade,
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
                #[cfg(unix)]
                fd_channel: fd_channel.or_else(|| {
                    std::env::var("BLIT_FD_CHANNEL")
                        .ok()
                        .and_then(|s| s.parse().ok())
                }),
                verbose: false,
            };
            blit_server::run(config).await;
        }
        Command::Install => {
            if let Err(e) = cmd_install(&cli.connect).await {
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
        | Command::Wait { .. }) => {
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
                _ => unreachable!(),
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        Command::Open { console, port } => {
            let conn = &cli.connect;
            if console {
                interactive::run_console(
                    &conn.socket,
                    &conn.tcp,
                    &conn.ssh,
                    &conn.passphrase,
                    &conn.hub,
                )
                .await;
            } else if let Some(passphrase) = &conn.passphrase {
                let hub = blit_webrtc_forwarder::normalize_hub(&conn.hub);
                interactive::run_browser_share(passphrase, &hub, port).await;
            } else {
                interactive::run_browser(&conn.socket, &conn.tcp, &conn.ssh, port).await;
            }
        }
        Command::Learn => {
            print!("{}", include_str!("learn.md"));
        }
    }
}

async fn cmd_install(opts: &ConnectOpts) -> Result<(), Box<dyn std::error::Error>> {
    let host = match &opts.ssh {
        Some(h) => h,
        None => {
            if opts.tcp.is_some() {
                return Err("install requires --ssh (got --tcp)".into());
            }
            if opts.passphrase.is_some() {
                return Err("install requires --ssh (got --passphrase)".into());
            }
            if opts.socket.is_some() {
                return Err("install requires --ssh".into());
            }
            return Err("install requires --ssh <HOST>".into());
        }
    };

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

    let script = reqwest::get("https://install.blit.sh")
        .await?
        .error_for_status()?
        .text()
        .await?;

    let tmp = std::env::temp_dir().join(format!("blit-install-{}.sh", std::process::id()));
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
