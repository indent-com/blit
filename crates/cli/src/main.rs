mod agent;
mod interactive;
mod transport;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "blit", version, about = "Terminal streaming client")]
struct Cli {
    #[command(flatten)]
    connect: ConnectOpts,

    #[command(subcommand)]
    command: Option<Command>,

    /// Render to terminal instead of opening browser (legacy mode)
    #[arg(long)]
    console: bool,

    /// Bind browser UI to a specific port (default: random)
    #[arg(long)]
    port: Option<u16>,
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
}

#[derive(Subcommand)]
enum Command {
    /// List all terminal sessions (TSV: ID, TAG, TITLE, STATUS)
    List,

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

        /// Wait for the command to exit and use its exit code
        #[arg(long, short = 'w')]
        wait: bool,
    },

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

    /// Close a session
    Close {
        /// Session ID
        id: u16,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(cmd) => {
            let conn = &cli.connect;
            let transport = match transport::connect(&conn.socket, &conn.tcp, &conn.ssh).await {
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
                } => {
                    match agent::cmd_start(transport, tag, command, rows, cols, wait).await {
                        Ok(Some(code)) => std::process::exit(code),
                        Ok(None) => Ok(()),
                        Err(e) => Err(e),
                    }
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
                    agent::cmd_history(transport, id, from_start, from_end, limit, ansi, size)
                        .await
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
                Command::Close { id } => agent::cmd_close(transport, id).await,
            };
            if let Err(e) = result {
                eprintln!("blit: {e}");
                std::process::exit(1);
            }
        }
        None => {
            let conn = &cli.connect;
            if cli.console {
                interactive::run_console(&conn.socket, &conn.tcp, &conn.ssh).await;
            } else {
                interactive::run_browser(&conn.socket, &conn.tcp, &conn.ssh, cli.port).await;
            }
        }
    }
}
