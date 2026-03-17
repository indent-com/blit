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
pub struct Cli {
    #[command(flatten)]
    pub connect: ConnectOpts,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Clone)]
pub struct ConnectOpts {
    /// Remote to connect to: a URI (ssh:host, tcp:h:p, socket:/p, share:pass, local)
    /// or a named remote from blit.remotes. Overrides BLIT_TARGET and blit.conf `target`.
    #[arg(long, global = true)]
    pub on: Option<String>,

    /// Signaling hub URL
    #[arg(long, global = true, env = "BLIT_HUB", default_value = blit_webrtc_forwarder::DEFAULT_HUB_URL)]
    pub hub: String,
}

#[derive(Subcommand)]
pub enum RecordTarget {
    /// Record raw encoded video from a compositor surface
    ///
    /// Writes Annex B (H.264/H.265) or OBU (AV1) that ffplay can play directly.
    Surface {
        /// Surface ID
        id: u16,

        /// Output file path (default: surface-<id>.h264 / .h265)
        #[arg(short, long)]
        output: Option<String>,

        /// Maximum number of frames to record [default: 30]
        #[arg(short, long, default_value_t = 30)]
        frames: u32,
    },

    /// Record timestamped terminal output
    ///
    /// Writes a compact binary format (BLITREC) with microsecond timestamps.
    Pty {
        /// PTY session ID
        id: u16,

        /// Output file path (default: pty-<id>.blitrec)
        #[arg(short, long)]
        output: Option<String>,

        /// Maximum number of update frames to record [default: 30]
        #[arg(short, long, default_value_t = 30)]
        frames: u32,
    },
}

#[derive(Subcommand)]
pub enum Command {
    #[command(
        about = "Open the terminal UI in the browser",
        long_about = "Open the terminal UI in the browser\n\n\
            Opens the browser with all named remotes from ~/.config/blit/blit.remotes\n\
            plus the local blit-server. Manage remotes with `blit remote add/remove`\n\
            or through the Remotes dialog in the browser.\n\n\
            Examples:\n\
              blit open                        # local + all configured remotes\n\
              blit remote add rabbit ssh:rabbit\n\
              blit open                        # now includes rabbit"
    )]
    Open {
        /// Bind browser UI to a specific port (default: random)
        #[arg(long)]
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

    #[command(
        about = "Install blit on a remote host via SSH, or print install commands",
        long_about = "Install blit on a remote host via SSH, or print install commands.\n\n\
            With a host argument, connects via SSH and runs the installer remotely.\n\
            Without a host argument, prints the one-liner install commands for each\n\
            platform so you can copy and run them by hand."
    )]
    Install {
        /// SSH target ([user@]host). Omit to print install commands for each platform.
        host: Option<String>,
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

        /// Quality: 0 = lossless, 1-100 = lossy (applies to AVIF only)
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

    /// Record encoded video or terminal output to a file
    #[command(subcommand)]
    Record(RecordTarget),

    /// Manage named remotes in blit.remotes
    ///
    /// Named remotes let you refer to frequently-used destinations by a short
    /// name instead of a full URI.  They are stored in ~/.config/blit/blit.remotes
    /// (mode 0o600) and can also be set as the default target via `blit.conf`.
    ///
    /// Examples:
    ///   blit remote add rabbit ssh:rabbit
    ///   blit remote add prod ssh:alice@prod.example.com
    ///   blit remote add lab share:mysecret
    ///   blit remote list
    ///   blit remote remove rabbit
    ///   blit --on rabbit list
    ///   blit remote set-default rabbit
    #[command(subcommand)]
    Remote(RemoteCommand),

    /// Generate man pages and shell completions
    ///
    /// Writes man pages for all blit binaries and shell completions
    /// (fish, bash, zsh) for the blit CLI into the given directory.
    Generate {
        /// Output directory (e.g. /usr/share)
        output: String,
    },

    /// Run the connection-pool proxy daemon (internal; not for direct use)
    #[command(hide = true)]
    ProxyDaemon,
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// Add or update a named remote
    Add {
        /// Name for the remote
        name: String,
        /// URI to connect to (ssh:host, tcp:h:p, socket:/p, share:pass, local).
        /// Omit to be prompted interactively.
        uri: Option<String>,
    },

    /// Remove a named remote
    Remove {
        /// Name of the remote to remove
        name: String,
    },

    /// List all named remotes
    List {
        /// Show share passphrases in full instead of masking them
        #[arg(long)]
        reveal: bool,
    },

    /// Set the default remote in blit.conf
    ///
    /// After this, all agent subcommands (list, start, show, …) will connect
    /// to this remote by default, without needing --on.
    SetDefault {
        /// Name or URI to use as the default target.
        /// Pass an empty string or "local" to reset to local.
        target: String,
    },
}
