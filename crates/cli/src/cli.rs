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
          blit open                 Open the terminal UI in a browser\n  \
          blit share                Share a terminal via WebRTC\n  \
          blit terminal start htop  Start a PTY and print its terminal ID\n  \
          blit terminal show 1      Dump current visible terminal text\n  \
          blit learn                Print the full CLI reference\n  \
          blit --help               Show this help",
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
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Manage terminals (PTYs)
    #[command(alias = "t")]
    Terminal {
        #[command(subcommand)]
        command: Option<TerminalCommand>,
    },

    /// Manage compositor surfaces
    #[command(alias = "s")]
    Surface {
        #[command(subcommand)]
        command: Option<SurfaceCommand>,
    },

    /// Manage the clipboard
    #[command(alias = "c")]
    Clipboard {
        #[command(subcommand)]
        command: Option<ClipboardCommand>,
    },

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
    ///   blit --on rabbit terminal list
    ///   blit remote set-default rabbit
    #[command(alias = "r")]
    Remote {
        #[command(subcommand)]
        command: Option<RemoteCommand>,
    },

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

    /// Share a terminal via WebRTC
    ///
    /// Set BLIT_PASSPHRASE to use a deterministic passphrase (default: random).
    Share {
        /// Don't print the sharing URL
        #[arg(long)]
        quiet: bool,

        /// Print detailed connection diagnostics to stderr
        #[arg(long)]
        verbose: bool,
    },

    /// Print the full CLI reference (usage guide for scripts and LLM agents)
    Learn,

    /// Run the blit terminal multiplexer server
    Server {
        /// IPC socket/pipe path (or set BLIT_SOCK)
        #[arg(long)]
        socket: Option<String>,

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

        /// Enable verbose logging
        #[arg(long, short)]
        verbose: bool,
    },

    /// Shut down the blit server
    Quit,

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

    /// Run the WebSocket/WebTransport gateway
    ///
    /// All configuration is via environment variables:
    ///
    ///   BLIT_PASSPHRASE   Browser passphrase (required)
    ///
    ///   BLIT_ADDR         Listen address (default: 0.0.0.0:3264)
    ///
    ///   BLIT_REMOTES      Path to remotes file
    ///
    ///   BLIT_QUIC         Set to 1 for WebTransport
    ///
    ///   BLIT_PROXY        Set to 0 to disable blit-proxy
    Gateway,

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

// ── Terminal subcommands ─────────────────────────────────────────────────

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum TerminalCommand {
    /// List all terminals (TSV: ID, TAG, TITLE, STATUS)
    #[command(alias = "ls")]
    List,

    /// Start a new terminal and print its ID
    Start {
        /// Command to run (defaults to $SHELL or /bin/sh)
        command: Vec<String>,

        /// Terminal tag / label
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

    /// Print the current visible text of a terminal
    Show {
        /// Terminal ID
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
        /// Terminal ID
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

    /// Ripgrep-compatible search over terminals' backlog + viewport.
    ///
    /// Each terminal is treated as a "file". Trailing IDs pick specific terminals
    /// (same numbers `blit terminal list` prints); with no IDs and no filters,
    /// every terminal is searched. Logical lines that soft-wrap across multiple
    /// physical rows are stitched back into one line before matching — a regex
    /// like 'Error: .* refused' matches even if the message wrapped at column 80.
    ///
    /// Target selection:
    ///   blit terminal grep PATTERN            # all terminals
    ///   blit terminal grep PATTERN 3 5        # just PTYs 3 and 5
    ///   blit terminal grep PATTERN --tag build
    ///   blit terminal grep PATTERN --title vim --running
    ///   blit terminal grep PATTERN --all
    ///
    /// Uses the Rust `regex` crate (RE2-style — same default engine as ripgrep).
    /// Lookaround and backreferences are not supported; pipe through external
    /// ripgrep if you need them: `blit terminal history 3 | rg -P '(?<=...)'`.
    #[command(alias = "rg")]
    Grep {
        /// Regex pattern (or literal string with -F). May be omitted if -e/-f is used.
        pattern: Option<String>,

        /// Terminal IDs to search (empty = all terminals, subject to filters)
        ids: Vec<u16>,

        // ── Patterns ─────────────────────────────────────────────────────
        /// Additional regex pattern (may be given multiple times)
        #[arg(short = 'e', long = "regexp", action = clap::ArgAction::Append)]
        regexps: Vec<String>,

        /// Read one pattern per line from FILE (may be given multiple times)
        #[arg(short = 'f', long = "file", action = clap::ArgAction::Append)]
        pattern_files: Vec<String>,

        /// Treat pattern as a literal string, not a regex
        #[arg(short = 'F', long)]
        fixed_strings: bool,

        /// Only match whole words (wrap pattern in \b…\b)
        #[arg(short = 'w', long)]
        word_regexp: bool,

        /// Only match whole lines (anchor pattern with \A…\z)
        #[arg(short = 'x', long)]
        line_regexp: bool,

        // ── Case ─────────────────────────────────────────────────────────
        /// Case-insensitive match
        #[arg(short = 'i', long, conflicts_with_all = ["case_sensitive", "smart_case"])]
        ignore_case: bool,

        /// Force case-sensitive match (overrides -i, -S)
        #[arg(short = 's', long, conflicts_with_all = ["ignore_case", "smart_case"])]
        case_sensitive: bool,

        /// Case-insensitive if pattern is all-lowercase, else sensitive
        #[arg(short = 'S', long, conflicts_with_all = ["ignore_case", "case_sensitive"])]
        smart_case: bool,

        /// Invert: print lines that do NOT match
        #[arg(short = 'v', long)]
        invert_match: bool,

        // ── Multiline ────────────────────────────────────────────────────
        /// Allow patterns to span multiple lines
        #[arg(short = 'U', long)]
        multiline: bool,

        /// In multiline mode, let `.` match newline as well
        #[arg(long, requires = "multiline")]
        multiline_dotall: bool,

        // ── Context ──────────────────────────────────────────────────────
        /// Show N lines of context after each match
        #[arg(short = 'A', long, default_value_t = 0)]
        after_context: usize,

        /// Show N lines of context before each match
        #[arg(short = 'B', long, default_value_t = 0)]
        before_context: usize,

        /// Show N lines of context before and after each match
        #[arg(short = 'C', long)]
        context: Option<usize>,

        /// Separator printed between non-contiguous context groups
        #[arg(long, default_value = "--")]
        context_separator: String,

        /// Suppress the context separator line
        #[arg(long)]
        no_context_separator: bool,

        // ── Output shaping ───────────────────────────────────────────────
        /// Show 1-based line numbers (default on)
        #[arg(short = 'n', long, conflicts_with = "no_line_number")]
        line_number: bool,

        /// Suppress line numbers
        #[arg(short = 'N', long)]
        no_line_number: bool,

        /// Always print the terminal "filename" (pty:N) with each match
        #[arg(short = 'H', long, conflicts_with = "no_filename")]
        with_filename: bool,

        /// Never print the terminal "filename"
        #[arg(short = 'I', long)]
        no_filename: bool,

        /// Group matches per terminal under a heading (default on TTY, multi-PTY)
        #[arg(long, conflicts_with = "no_heading")]
        heading: bool,

        /// Do not group matches under a per-terminal heading
        #[arg(long)]
        no_heading: bool,

        /// Show 1-based column of the first match on each line
        #[arg(long)]
        column: bool,

        /// Print only "pty:N:<count>" per terminal (no match lines)
        #[arg(short = 'c', long)]
        count: bool,

        /// Like -c but count every match, not every matching line
        #[arg(long, conflicts_with = "count")]
        count_matches: bool,

        /// Print only the IDs of terminals with at least one match
        #[arg(short = 'l', long)]
        files_with_matches: bool,

        /// Print only the IDs of terminals with no matches
        #[arg(long, conflicts_with = "files_with_matches")]
        files_without_match: bool,

        /// Print only the matched text, one per line
        #[arg(short = 'o', long)]
        only_matching: bool,

        /// Stop after N matches per terminal
        #[arg(short = 'm', long)]
        max_count: Option<u64>,

        /// Print every line; matching lines use the match separator
        #[arg(long)]
        passthru: bool,

        /// Emit one line per match as pty:N:line:col:text
        #[arg(long)]
        vimgrep: bool,

        /// Emit ripgrep's JSON event stream (begin/match/context/end/summary)
        #[arg(long)]
        json: bool,

        /// Alias for --color=always --heading -n
        #[arg(short = 'p', long)]
        pretty: bool,

        /// Separate filename from the rest with a NUL byte
        #[arg(short = '0', long)]
        null: bool,

        /// When to colorize output: auto, always, never, ansi
        #[arg(long, default_value = "auto", value_parser = ["auto", "always", "never", "ansi"])]
        color: String,

        /// String between filename and line number for context lines
        #[arg(long, default_value = "-")]
        field_context_separator: String,

        /// String between filename and line number for match lines
        #[arg(long, default_value = ":")]
        field_match_separator: String,

        // ── Limiters & meta ──────────────────────────────────────────────
        /// Do not print anything; exit 0 on any match, 1 otherwise
        #[arg(short = 'q', long)]
        quiet: bool,

        /// Suppress warnings about unreadable files / missing IDs
        #[arg(long)]
        no_messages: bool,

        /// Print match-count statistics after searching
        #[arg(long)]
        stats: bool,

        /// In a terminal, stop searching after the first non-matching line
        /// that follows a match (useful for tailing recent events)
        #[arg(long)]
        stop_on_nonmatch: bool,

        // ── Sorting ──────────────────────────────────────────────────────
        /// Sort results: "path" (by numeric terminal ID) or "none"
        #[arg(long, value_parser = ["path", "none"], conflicts_with = "sortr")]
        sort: Option<String>,

        /// Like --sort but reversed
        #[arg(long, value_parser = ["path", "none"])]
        sortr: Option<String>,

        // ── Target selection (blit extensions) ───────────────────────────
        /// Keep terminals whose tag contains this substring
        #[arg(long)]
        tag: Option<String>,

        /// Keep terminals whose title contains this substring
        #[arg(long)]
        title: Option<String>,

        /// Keep only running terminals
        #[arg(long, conflicts_with = "exited")]
        running: bool,

        /// Keep only exited terminals
        #[arg(long)]
        exited: bool,

        /// Explicitly opt in to "no filter, no positional IDs"
        #[arg(long, conflicts_with_all = [
            "tag", "title", "running", "exited"
        ])]
        all: bool,
    },

    /// Send input to a terminal.
    ///
    /// Supports C-style escapes: \n \r \t \\ \0 \xHH.
    /// \n sends CR (Enter), matching real terminal behavior. Use \x0a for literal LF.
    /// To control interactive programs like vim:
    ///   blit terminal send 3 '\x1b:wq\n'
    ///   printf '\x1b:wq\n' | blit terminal send 3 -
    Send {
        /// Terminal ID
        id: u16,

        /// Text to send (use - to read from stdin)
        text: String,
    },

    /// Wait for a terminal to exit or match a pattern.
    ///
    /// Without --pattern, blocks until the PTY process exits and returns
    /// its exit code. With --pattern, subscribes to output and exits when
    /// the regex matches a line produced after the wait began.
    Wait {
        /// Terminal ID
        id: u16,

        /// Maximum seconds to wait before giving up (exit code 124)
        #[arg(long)]
        timeout: u64,

        /// Regex pattern to match against new output lines
        #[arg(long)]
        pattern: Option<String>,
    },

    /// Restart an exited terminal (re-runs the original command)
    Restart {
        /// Terminal ID
        id: u16,
    },

    /// Send a signal to a terminal's leader process
    Kill {
        /// Terminal ID
        id: u16,

        /// Signal name or number (e.g. TERM, KILL, INT, 9)
        #[arg(default_value = "TERM")]
        signal: String,
    },

    /// Close a terminal
    Close {
        /// Terminal ID
        id: u16,
    },

    /// Record timestamped terminal output
    ///
    /// Writes a compact binary format (BLITREC) with microsecond timestamps.
    /// Records until --frames or --duration is reached, or Ctrl+C.
    Record {
        /// PTY terminal ID
        id: u16,

        /// Output file path (default: pty-<id>.blitrec)
        #[arg(short, long)]
        output: Option<String>,

        /// Maximum number of frames to record (0 = unlimited)
        #[arg(short, long, default_value_t = 0)]
        frames: u32,

        /// Maximum duration in seconds (0 = unlimited)
        #[arg(short, long, default_value_t = 0.0)]
        duration: f64,
    },
}

// ── Surface subcommands ──────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum SurfaceCommand {
    /// List all compositor surfaces (TSV: ID, TITLE, SIZE, APP_ID)
    #[command(alias = "ls")]
    List,

    /// Close a compositor surface (sends xdg_toplevel close event)
    Close {
        /// Surface ID
        id: u16,
    },

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

        /// Render scale in 120ths (wp_fractional_scale_v1 units).
        /// 120 = 1x, 240 = 2x, 180 = 1.5x, etc.
        /// Default (0) uses the compositor's current output scale.
        #[arg(long, default_value_t = 0)]
        scale: u16,
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

    /// Record raw encoded video from a compositor surface
    ///
    /// Writes Annex B (H.264) or OBU (AV1) that ffplay can play directly.
    /// Records until --frames or --duration is reached, or Ctrl+C.
    Record {
        /// Surface ID
        id: u16,

        /// Output file path (default: surface-<id>.<codec>)
        #[arg(short, long)]
        output: Option<String>,

        /// Maximum number of frames to record (0 = unlimited)
        #[arg(short, long, default_value_t = 0)]
        frames: u32,

        /// Maximum duration in seconds (0 = unlimited)
        #[arg(short, long, default_value_t = 0.0)]
        duration: f64,

        /// Codec(s) to announce as supported (comma-separated or repeated).
        /// Accepted values: h264, av1.
        /// Default: all codecs.
        #[arg(short, long, value_delimiter = ',')]
        codec: Vec<String>,
    },
}

// ── Clipboard subcommands ────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum ClipboardCommand {
    /// List available MIME types on the clipboard
    #[command(alias = "ls")]
    List,

    /// Read clipboard content
    Get {
        /// MIME type to retrieve (default: text/plain)
        #[arg(long, default_value = "text/plain")]
        mime: String,
    },

    /// Set clipboard content
    Set {
        /// MIME type (default: text/plain;charset=utf-8)
        #[arg(long, default_value = "text/plain;charset=utf-8")]
        mime: String,

        /// Text to set (if omitted, reads from stdin)
        text: Option<String>,
    },
}

// ── Remote subcommands ───────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// List all named remotes
    #[command(alias = "ls")]
    List {
        /// Show share passphrases in full instead of masking them
        #[arg(long)]
        reveal: bool,
    },

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
