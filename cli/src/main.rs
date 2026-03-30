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
        /// Command to run
        #[arg(required = true)]
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
    },

    /// Print the current visible text of a session
    Show {
        /// Session ID
        id: u16,

        /// Include ANSI color/style escape sequences in output
        #[arg(long)]
        ansi: bool,
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

    /// Close a session
    Close {
        /// Session ID
        id: u16,
    },

    /// Resize a session
    Resize {
        /// Session ID
        id: u16,

        /// Terminal rows
        rows: u16,

        /// Terminal columns
        cols: u16,
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
                } => agent::cmd_start(transport, tag, command, rows, cols).await,
                Command::Show { id, ansi } => agent::cmd_show(transport, id, ansi).await,
                Command::History {
                    id,
                    from_start,
                    from_end,
                    limit,
                    ansi,
                } => agent::cmd_history(transport, id, from_start, from_end, limit, ansi).await,
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
                Command::Close { id } => agent::cmd_close(transport, id).await,
                Command::Resize { id, rows, cols } => {
                    agent::cmd_resize(transport, id, rows, cols).await
                }
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
