use clap::{Args, Parser, Subcommand};

use crate::extension::ExtensionCommand;
use crate::process::RunArgs;

/// Text for `blit --license`.  Mentions exactly the third-party components
/// whose licenses affect distribution of *this* binary — nothing more.
pub fn license_text() -> String {
    let mut text = String::new();
    text.push_str(
        "blit — MIT License, copyright (c) 2026 Indent Team\n\
         Full text: https://github.com/indent-com/blit/blob/main/LICENSE\n",
    );
    #[cfg(all(target_os = "linux", feature = "x264"))]
    text.push_str(
        "\nThis binary includes libx264, copyright (c) the x264 project,\n\
         licensed under the GNU General Public License, version 2 or later.\n\
         As a combined work, this binary as a whole is distributed under the\n\
         terms of the GPL-2.0-or-later.  Complete corresponding source:\n\
         https://github.com/indent-com/blit (x264: https://code.videolan.org/videolan/x264)\n",
    );
    #[cfg(all(target_os = "linux", feature = "openh264"))]
    text.push_str(
        "\nThis binary includes OpenH264, copyright (c) Cisco Systems,\n\
         licensed under the BSD-2-Clause license.\n",
    );
    text
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
          blit open                 Open the terminal UI in a browser\n  \
          blit share                Share via WebRTC\n  \
          blit terminal start htop  Start a PTY and print its terminal ID\n  \
          blit terminal show 1      Dump current visible terminal text\n  \
          blit learn                Print the full CLI reference\n  \
          blit --help               Show this help\n  \
          blit --license            Show license terms for this binary",
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    #[command(flatten)]
    pub connect: ConnectOpts,

    /// Emit NDJSON for an extension-provided `@name` command
    ///
    /// This root option must precede `@name`. A `--json` after the namespace
    /// is passed verbatim to the extension command.
    #[arg(long = "json")]
    pub advertised_command_json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Clone)]
pub struct ConnectOpts {
    /// Remote to connect to: a URI (ssh:host, tcp:h:p, socket:/p, share:pass, local[:name])
    /// or a named remote from blit.remotes. Overrides BLIT_TARGET and blit.conf `target`.
    #[arg(long, global = true)]
    pub on: Option<String>,

    /// Signaling hub URL
    #[arg(long, global = true, env = "BLIT_HUB", default_value = blit_webrtc_forwarder::DEFAULT_HUB_URL)]
    pub hub: String,
}

/// Startup-only extension and native-channel deployment policy.
#[derive(Args, Clone, Debug, Default)]
pub struct ServerDeploymentOpts {
    /// Disable Wasmi extensions (equivalent to BLIT_EXT=0)
    #[arg(long)]
    no_extensions: bool,

    /// Disable native channels (equivalent to BLIT_CHANNEL=0)
    #[arg(long)]
    no_channels: bool,

    /// Maximum concurrent running extension attempts
    #[arg(long, value_name = "N")]
    ext_max_running: Option<u64>,

    /// Maximum persistent extension definitions
    #[arg(long, value_name = "N")]
    ext_max_persistent: Option<u64>,

    /// Maximum active transient extension supervisors
    #[arg(long, value_name = "N")]
    ext_max_transient: Option<u64>,

    /// Maximum followed extensions per client
    #[arg(long, value_name = "N")]
    ext_follow_max_per_client: Option<u64>,

    /// Maximum extension follower cursors server-wide
    #[arg(long, value_name = "N")]
    ext_follow_max: Option<u64>,

    /// Maximum retained extension argument bytes
    #[arg(long, value_name = "BYTES")]
    ext_argument_store_max: Option<u64>,

    /// Maximum raw Wasm module bytes (hard ceiling: 64 MiB)
    #[arg(long, value_name = "BYTES")]
    ext_module_max: Option<u64>,

    /// Maximum extension object-cache bytes
    #[arg(long, value_name = "BYTES")]
    ext_object_cache_max: Option<u64>,

    /// Maximum extension object-cache entries
    #[arg(long, value_name = "N")]
    ext_object_cache_max_entries: Option<u64>,

    /// Maximum active uploads per client
    #[arg(long, value_name = "N")]
    ext_upload_max_per_client: Option<u64>,

    /// Maximum active extension uploads server-wide
    #[arg(long, value_name = "N")]
    ext_upload_max_active: Option<u64>,

    /// Active-upload idle timeout in seconds
    #[arg(long, value_name = "SECONDS")]
    ext_upload_timeout: Option<u64>,

    /// Pending extension-creation timeout in seconds
    #[arg(long, value_name = "SECONDS")]
    ext_pending_timeout: Option<u64>,

    /// Maximum concurrent module validations
    #[arg(long, value_name = "N")]
    ext_max_validating: Option<u64>,

    /// Maximum Wasm linear-memory bytes per attempt
    #[arg(long, value_name = "BYTES")]
    ext_memory_max: Option<u64>,

    /// Queued extension egress byte ceiling
    #[arg(long, value_name = "BYTES")]
    ext_outbox_max: Option<u64>,

    /// Maximum queued messages per extension endpoint
    #[arg(long, value_name = "N")]
    ext_outbox_messages_max: Option<u64>,

    /// Full extension-output no-progress timeout in seconds
    #[arg(long, value_name = "SECONDS")]
    ext_outbox_timeout: Option<u64>,

    /// Maximum active tracked jobs per extension endpoint
    #[arg(long, value_name = "N")]
    ext_job_max_per_client: Option<u64>,

    /// Maximum active tracked extension jobs server-wide
    #[arg(long, value_name = "N")]
    ext_job_max: Option<u64>,

    /// Maximum pending tracked jobs per extension endpoint
    #[arg(long, value_name = "N")]
    ext_job_pending_max_per_client: Option<u64>,

    /// Maximum pending tracked extension jobs server-wide
    #[arg(long, value_name = "N")]
    ext_job_pending_max: Option<u64>,

    /// Maximum tracked-job request bytes per extension endpoint
    #[arg(long, value_name = "BYTES")]
    ext_job_bytes_max_per_client: Option<u64>,

    /// Maximum tracked-job request bytes server-wide
    #[arg(long, value_name = "BYTES")]
    ext_job_bytes_max: Option<u64>,

    /// Maximum retained extension output bytes server-wide
    #[arg(long, value_name = "BYTES")]
    ext_output_retain_max: Option<u64>,

    /// Terminal replay lease for transient extensions, in seconds
    #[arg(long, value_name = "SECONDS")]
    ext_terminal_retain: Option<u64>,

    /// Maximum command-record and discovery-snapshot bytes
    #[arg(long, value_name = "BYTES")]
    ext_command_store_max: Option<u64>,

    /// Maximum active command-discovery snapshots
    #[arg(long, value_name = "N")]
    ext_command_snapshot_max: Option<u64>,

    /// Maximum aggregate Wasm table elements per attempt
    #[arg(long, value_name = "N")]
    ext_table_elements_max: Option<u64>,

    /// Maximum Wasmi value-stack bytes per attempt
    #[arg(long, value_name = "BYTES")]
    ext_value_stack_max: Option<u64>,

    /// Maximum Wasmi call depth per attempt
    #[arg(long, value_name = "N")]
    ext_call_depth_max: Option<u64>,

    /// Native stack bytes per extension thread
    #[arg(long, value_name = "BYTES")]
    ext_stack_size: Option<u64>,

    /// Fuel replenished per extension driver slice
    #[arg(long, value_name = "N")]
    ext_fuel_slice: Option<u64>,

    /// Maximum channel listeners per client
    #[arg(long, value_name = "N")]
    channel_max_listen_per_client: Option<u64>,

    /// Maximum channel listeners server-wide
    #[arg(long, value_name = "N")]
    channel_max_listeners: Option<u64>,

    /// Maximum connected channel handles per client
    #[arg(long, value_name = "N")]
    channel_max_per_client: Option<u64>,

    /// Maximum connected channel pairs server-wide
    #[arg(long, value_name = "N")]
    channel_max_connected: Option<u64>,

    /// Maximum reserved channel-window bytes server-wide
    #[arg(long, value_name = "BYTES")]
    channel_buffer_max: Option<u64>,
}

impl ServerDeploymentOpts {
    pub fn into_overrides(self) -> Result<blit_server::DeploymentOverrides, String> {
        let mut overrides = blit_server::DeploymentOverrides::default();
        if self.no_extensions {
            overrides.disable_extensions();
        }
        if self.no_channels {
            overrides.disable_channels();
        }
        macro_rules! setting {
            ($field:ident, $name:literal) => {
                if let Some(value) = self.$field {
                    overrides.set($name, value)?;
                }
            };
        }
        setting!(ext_max_running, "BLIT_EXT_MAX_RUNNING");
        setting!(ext_max_persistent, "BLIT_EXT_MAX_PERSISTENT");
        setting!(ext_max_transient, "BLIT_EXT_MAX_TRANSIENT");
        setting!(ext_follow_max_per_client, "BLIT_EXT_FOLLOW_MAX_PER_CLIENT");
        setting!(ext_follow_max, "BLIT_EXT_FOLLOW_MAX");
        setting!(ext_argument_store_max, "BLIT_EXT_ARGUMENT_STORE_MAX");
        setting!(ext_module_max, "BLIT_EXT_MODULE_MAX");
        setting!(ext_object_cache_max, "BLIT_EXT_OBJECT_CACHE_MAX");
        setting!(
            ext_object_cache_max_entries,
            "BLIT_EXT_OBJECT_CACHE_MAX_ENTRIES"
        );
        setting!(ext_upload_max_per_client, "BLIT_EXT_UPLOAD_MAX_PER_CLIENT");
        setting!(ext_upload_max_active, "BLIT_EXT_UPLOAD_MAX_ACTIVE");
        setting!(ext_upload_timeout, "BLIT_EXT_UPLOAD_TIMEOUT");
        setting!(ext_pending_timeout, "BLIT_EXT_PENDING_TIMEOUT");
        setting!(ext_max_validating, "BLIT_EXT_MAX_VALIDATING");
        setting!(ext_memory_max, "BLIT_EXT_MEMORY_MAX");
        setting!(ext_outbox_max, "BLIT_EXT_OUTBOX_MAX");
        setting!(ext_outbox_messages_max, "BLIT_EXT_OUTBOX_MESSAGES_MAX");
        setting!(ext_outbox_timeout, "BLIT_EXT_OUTBOX_TIMEOUT");
        setting!(ext_job_max_per_client, "BLIT_EXT_JOB_MAX_PER_CLIENT");
        setting!(ext_job_max, "BLIT_EXT_JOB_MAX");
        setting!(
            ext_job_pending_max_per_client,
            "BLIT_EXT_JOB_PENDING_MAX_PER_CLIENT"
        );
        setting!(ext_job_pending_max, "BLIT_EXT_JOB_PENDING_MAX");
        setting!(
            ext_job_bytes_max_per_client,
            "BLIT_EXT_JOB_BYTES_MAX_PER_CLIENT"
        );
        setting!(ext_job_bytes_max, "BLIT_EXT_JOB_BYTES_MAX");
        setting!(ext_output_retain_max, "BLIT_EXT_OUTPUT_RETAIN_MAX");
        setting!(ext_terminal_retain, "BLIT_EXT_TERMINAL_RETAIN");
        setting!(ext_command_store_max, "BLIT_EXT_COMMAND_STORE_MAX");
        setting!(ext_command_snapshot_max, "BLIT_EXT_COMMAND_SNAPSHOT_MAX");
        setting!(ext_table_elements_max, "BLIT_EXT_TABLE_ELEMENTS_MAX");
        setting!(ext_value_stack_max, "BLIT_EXT_VALUE_STACK_MAX");
        setting!(ext_call_depth_max, "BLIT_EXT_CALL_DEPTH_MAX");
        setting!(ext_stack_size, "BLIT_EXT_STACK_SIZE");
        setting!(ext_fuel_slice, "BLIT_EXT_FUEL_SLICE");
        setting!(
            channel_max_listen_per_client,
            "BLIT_CHANNEL_MAX_LISTEN_PER_CLIENT"
        );
        setting!(channel_max_listeners, "BLIT_CHANNEL_MAX_LISTENERS");
        setting!(channel_max_per_client, "BLIT_CHANNEL_MAX_PER_CLIENT");
        setting!(channel_max_connected, "BLIT_CHANNEL_MAX_CONNECTED");
        setting!(channel_buffer_max, "BLIT_CHANNEL_BUFFER_MAX");
        Ok(overrides)
    }
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

    /// List and disconnect clients connected to the server
    Client {
        #[command(subcommand)]
        command: Option<ClientCommand>,
    },

    /// Inspect and configure the server's binary event journal
    Events {
        #[command(subcommand)]
        command: EventsCommand,
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

    /// Mirror server filesystem state (docs/fs-watch.md)
    Fs {
        #[command(subcommand)]
        command: FsCommand,
    },

    /// Inspect git repositories on the server (docs/git.md)
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },

    /// Read and write the server's key/value store (docs/design/kv.md)
    ///
    /// A prefix-watchable store the server already keeps for the web app's
    /// settings; it doubles as host-local scratch space for scripts.
    Kv {
        #[command(subcommand)]
        command: KvCommand,
    },

    /// Query language servers on the server (docs/design/lsp.md)
    ///
    /// Language servers are discovered by project markers (Cargo.toml,
    /// go.mod, tsconfig.json, …), spawned lazily, and stay warm across
    /// invocations. Positions are 1-based PATH:LINE:COL. First calls in
    /// a fresh workspace may report "warming up" — retry, or run
    /// `blit lsp wait`.
    Lsp {
        #[command(subcommand)]
        command: LspCommand,
    },

    /// Run and manage Wasmi extensions
    #[command(name = "ext", alias = "extension")]
    Extension {
        #[command(subcommand)]
        command: ExtensionCommand,
    },

    /// Execute a process and connect its standard streams
    Run(RunArgs),

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
    ///   blit remote add sandbox uplink:<jwt>
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
            plus the local blit server. Manage remotes with `blit remote add/remove`\n\
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

    /// Share via WebRTC
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

    /// Expose the local blit server through a relay
    ///
    /// Requires BLIT_UPLINK_TOKEN for the control endpoint.
    Uplink {
        /// Control endpoint URL (e.g. https://blit.indent.com)
        url: String,
    },

    /// Forward local ports to the server's network (TCP and UDP)
    ///
    /// Specs are `[kind/][bind:]port:host:hostport`, where kind is tcp
    /// (default) or udp, bind defaults to 127.0.0.1, and a local port of 0
    /// picks a free one. Every spec rides one connection; all listeners bind
    /// before any serves, so a bind failure leaves nothing running.
    ///
    /// Forwards end with the process — the listening socket is local, so
    /// there is nothing to reattach to.
    ///
    /// Examples:
    ///   blit forward 8080:localhost:3000
    ///   blit forward 8080:localhost:3000 5432:db.internal:5432
    ///   blit forward udp/5353:resolver.internal:53
    ///   blit forward tls/8443:api.internal:443   # server terminates TLS
    ///   blit forward --all                    # every enabled blit.forwards entry
    ///   blit forward add web 8080:localhost:3000
    ///   blit forward list
    #[command(alias = "f")]
    Forward {
        /// Forward specs, or a named entry's management subcommand
        #[arg(value_name = "SPEC")]
        specs: Vec<String>,

        /// Start every enabled entry in ~/.config/blit/blit.forwards
        #[arg(long)]
        all: bool,

        /// ALPN protocols to offer on tls/ forwards, in preference order
        /// (e.g. --alpn h2,http/1.1). Omitted offers no ALPN, which is not
        /// the same as offering http/1.1.
        #[arg(long, value_delimiter = ',')]
        alpn: Vec<String>,

        /// Skip certificate verification on tls/ forwards. The server must
        /// also permit it (blit server --allow-forward-insecure).
        #[arg(long)]
        insecure: bool,
    },

    /// Proxy TCP connections into the server's network over SOCKS5
    ///
    /// `ssh -D`: a local SOCKS5 listener whose target comes from each request,
    /// so one port reaches everything the server reaches and no target has to be
    /// known in advance. The listen address is `[bind_address:]port`, bind
    /// defaults to 127.0.0.1, and a port of 0 picks a free one.
    ///
    /// Names are resolved on the server, so the proxy reaches hosts this machine
    /// cannot look up. CONNECT only — BIND and UDP ASSOCIATE are not supported —
    /// and no authentication method beyond no-auth.
    ///
    /// The proxy ends with the process, like a forward.
    ///
    /// Examples:
    ///   blit socks 1080
    ///   blit socks 127.0.0.1:1080
    ///   blit --on prod socks 1080          # through a named remote
    ///   curl -x socks5h://localhost:1080 http://api.internal/
    Socks {
        /// Where to listen: [bind_address:]port
        #[arg(value_name = "[BIND:]PORT")]
        listen: String,
    },

    /// Print the full CLI reference (usage guide for scripts and LLM agents)
    Learn,
    /// Run the blit terminal multiplexer server
    Server {
        /// Isolate this server's socket, databases, caches, and extension
        /// settings under NAME (or set BLIT_SERVER_NAME)
        #[arg(
            long,
            env = "BLIT_SERVER_NAME",
            value_name = "NAME",
            default_value = "default"
        )]
        name: blit_server::ServerName,

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

        /// Export the server socket path as BLIT_SOCK in spawned terminals
        /// (or set BLIT_EXPORT_SOCK=1)
        #[arg(long)]
        export_sock: bool,

        /// Append the server binary's directory to PATH in spawned terminals,
        /// so `blit` is callable inside them (or set BLIT_INJECT_PATH=1)
        #[arg(long)]
        inject_path: bool,

        /// Maximum number of live terminals. Overrides BLIT_MAX_PTYS;
        /// unlimited by default. Exited terminals are bounded separately by
        /// BLIT_MAX_EXITED. A create refused past the cap answers
        /// CREATE2(WANT_STATUS) with a budget status instead of hanging.
        #[arg(long, value_name = "N")]
        max_ptys: Option<usize>,

        /// Surface video encoders to try, best first: h264-nvenc, av1-nvenc,
        /// h264-vaapi, av1-vaapi, h264-vulkan, av1-vulkan, h264-software,
        /// av1-software (or set BLIT_SURFACE_ENCODERS). The first entry a
        /// viewer can decode and this host can build wins.
        #[arg(long, value_name = "LIST")]
        surface_encoders: Option<String>,

        /// Camera formats viewers may send: mjpeg, h264, av1, h264-444,
        /// av1-444 (or set BLIT_MEDIA_CAMERA_CODECS). Each name selects one
        /// format — h264 does not imply h264-444. mjpeg is always accepted.
        /// Narrows what this host can decode; never widens it.
        #[arg(long, value_name = "LIST")]
        camera_codecs: Option<String>,

        /// Microphone formats viewers may send: pcm, opus (or set
        /// BLIT_MEDIA_MICROPHONE_CODECS). pcm is always accepted — it is the
        /// fallback a browser reaches when it cannot encode Opus.
        #[arg(long, value_name = "LIST")]
        microphone_codecs: Option<String>,

        /// Restrict what the TCP/UDP relay may reach: host[:ports], where
        /// host is a name, a *.suffix glob, an address, a CIDR block, or *,
        /// and ports is a comma-separated list of n or n-m. Repeatable (or
        /// set BLIT_ALLOW_FORWARD to a comma-separated list). Unrestricted
        /// when absent; loopback is always permitted.
        #[arg(long, value_name = "PATTERN")]
        allow_forward: Vec<String>,

        /// Permit relayed TLS streams to skip certificate verification (or set
        /// BLIT_ALLOW_FORWARD_INSECURE=1). Right for a self-signed dev server
        /// on loopback, wrong for anything reached across a network.
        #[arg(long)]
        allow_forward_insecure: bool,

        /// Refuse durable Wasmi extensions and do not restore desired
        /// definitions (or set BLIT_ALLOW_EXT_PERSIST=0). Transient
        /// extensions still run. This is the recovery path for a persistent
        /// definition that breaks the server it starts in.
        #[arg(long)]
        no_persistent_extensions: bool,

        #[command(flatten)]
        deployment: ServerDeploymentOpts,

        /// Enable verbose logging
        #[arg(long, short)]
        verbose: bool,

        /// Disable native non-PTY child processes (or set BLIT_PROCESS=0)
        #[arg(long)]
        no_processes: bool,
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

    /// Hash a gateway passphrase for BLIT_PASSPHRASE
    ///
    /// Prints an argon2id PHC string suitable for BLIT_PASSPHRASE. If VALUE is
    /// omitted or "-", reads from stdin. The stored hash is salted; browser
    /// clients still enter the original plaintext passphrase.
    HashPassphrase {
        /// Plaintext passphrase to hash (or -/omitted to read from stdin)
        value: Option<String>,
    },

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
    ///   BLIT_QUIC_PUBLIC_ADDR  Browser-facing hostname:port or :port to advertise
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

    /// Invoke a live extension-provided command namespace
    ///
    /// Generated shell completions supplement Blit's static grammar through a
    /// hidden, read-only command-directory query. This tested external-
    /// subcommand boundary still captures every token after `@name` verbatim.
    #[command(external_subcommand)]
    External(Vec<String>),
}

// ── Event journal subcommands ────────────────────────────────────────────

#[derive(Subcommand)]
pub enum EventsCommand {
    /// Show config revision, ring use/loss counters, and active event types
    Config,

    /// Replace ring size and/or activation bitset
    Set {
        /// Ring capacity in bytes (4 KiB through just under 64 MiB)
        #[arg(long)]
        size: Option<u64>,

        /// Activation selectors: default, all, none, exact names, category.*,
        /// with optional + or - prefixes
        #[arg(long, value_name = "SPEC")]
        events: Option<String>,

        /// Apply only while the server configuration is still at this revision
        #[arg(long, value_name = "REVISION")]
        if_revision: Option<u64>,
    },

    /// Write one retained binary snapshot to stdout or a local file
    Dump {
        /// Local output path; defaults to stdout. Use - for stdout explicitly.
        #[arg(short, long, value_name = "LOCAL_PATH")]
        output: Option<String>,
    },

    /// Follow events in the foreground, writing stdout or a local file
    ///
    /// The stream ends with this client connection. A connection may own at
    /// most four concurrent event tails.
    Tail {
        /// Local output path; defaults to stdout. Use - for stdout explicitly.
        #[arg(short, long, value_name = "LOCAL_PATH")]
        output: Option<String>,

        /// Append to --output instead of truncating it
        #[arg(long, requires = "output")]
        append: bool,

        /// Follow only events produced after this command starts
        #[arg(long)]
        from_now: bool,
    },

    /// Manage detached recordings written by the server
    Record {
        #[command(subcommand)]
        command: EventsRecordCommand,
    },
}

#[derive(Subcommand)]
pub enum EventsRecordCommand {
    /// Start a detached recording to a path on the server
    ///
    /// Returns an ID only after the initial header/history has been flushed.
    /// The server permits at most eight detached recordings process-wide.
    Start {
        /// Destination path on the server
        #[arg(value_name = "SERVER_PATH")]
        path: String,

        /// Append instead of truncating the server file
        #[arg(long)]
        append: bool,

        /// Record only events produced after this command starts
        #[arg(long)]
        from_now: bool,
    },

    /// List recordings as TSV: ID, STATE, RECORDS, BYTES, LOST, HISTORY, MODE, PATH, ERROR
    List,

    /// Stop and flush a server recording
    ///
    /// Returns an error if any recording write or final flush failed.
    Stop {
        /// Recording ID from `blit events record start` or `list`
        id: u32,
    },
}

// ── Terminal subcommands ─────────────────────────────────────────────────

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum TerminalCommand {
    /// List all terminals (TSV: ID, TAG, TITLE, COMMAND, CWD, STATUS)
    #[command(alias = "ls")]
    List,

    /// Start a new terminal and print its ID
    ///
    /// The command is executed directly, the way a process is started anywhere
    /// else — no login shell, so no rc files and no shell syntax. Pass --shell
    /// to run one string through $SHELL instead. With no command at all, the
    /// terminal gets the default interactive shell.
    ///
    /// Options come before the command; everything after the first bare word
    /// belongs to it. Use -- when the command's own flags would be ambiguous.
    ///
    /// Examples:
    ///   blit terminal start htop
    ///   blit terminal start -- cargo test --release
    ///   blit terminal start --cwd /src --env RUST_LOG=debug -- cargo run
    ///   blit terminal start --shell 'ls | wc -l'
    Start {
        /// Command to run (defaults to $SHELL or /bin/sh)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,

        /// Run the command through the server's login shell ($SHELL -lic)
        /// instead of executing it directly. Needed for pipes, redirections,
        /// globs, and anything else that is shell syntax rather than a program.
        #[arg(long, short = 'c')]
        shell: bool,

        /// Working directory for the new terminal
        #[arg(long, value_name = "DIR")]
        cwd: Option<String>,

        /// Set an environment variable, repeatable (--env KEY=VALUE).
        /// Overrides whatever the server would otherwise pass down.
        #[arg(long, value_name = "KEY=VALUE")]
        env: Vec<String>,

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

        /// Seconds after which the server stops this terminal on its own,
        /// armed at creation. Unlike --timeout, which only bounds how long
        /// this command waits, the deadline outlives the client: the
        /// terminal dies even if this process is killed. Re-send it with
        /// `blit terminal deadline` to use it as a dead-man switch.
        #[arg(long, value_name = "SECONDS")]
        deadline: Option<u64>,
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
    ///
    /// --since CURSOR instead reads only what was appended since a cursor
    /// and prints the next cursor on stderr, so a loop pulls each byte once:
    ///   c=$(blit terminal history 3 --since now 2>&1 >/dev/null | cut -d' ' -f2)
    ///   blit terminal history 3 --since "$c"
    /// CURSOR is SEQ, SEQ:COL, `now` (no text, current cursor) or `start`.
    History {
        /// Terminal ID
        id: u16,

        /// Start N lines from the top (oldest = 0)
        #[arg(long, conflicts_with_all = ["from_end", "since"])]
        from_start: Option<u32>,

        /// Start N lines from the bottom (newest = 0)
        #[arg(long, conflicts_with_all = ["from_start", "since"])]
        from_end: Option<u32>,

        /// Maximum number of lines to return
        #[arg(long, conflicts_with = "since")]
        limit: Option<u32>,

        /// Read only what is new since this cursor (SEQ[:COL], now, start)
        #[arg(long, value_name = "CURSOR")]
        since: Option<String>,

        /// Cap a --since read; the reply says where to continue from
        #[arg(long, value_name = "BYTES", requires = "since")]
        max_bytes: Option<u32>,

        /// JSON output (with --since: text plus the next cursor)
        #[arg(long, requires = "since")]
        json: bool,

        /// Include ANSI color/style escape sequences in output
        #[arg(long, conflicts_with = "since")]
        ansi: bool,

        /// Resize to this many rows before capturing
        #[arg(long)]
        rows: Option<u16>,

        /// Resize to this many columns before capturing
        #[arg(long)]
        cols: Option<u16>,
    },

    /// List the commands a terminal has run (needs OSC 133 shell integration).
    ///
    /// TSV: INDEX, STATUS, EXIT, MS, START_SEQ, END_SEQ, COMMAND. Prints the
    /// most recent commands by default. Indices are stable and never reused,
    /// so one can be fed back to `blit terminal output`.
    ///
    /// Nothing is recorded unless the shell emits OSC 133 semantic prompts —
    /// see docs/shell-integration.md for the one-line hook.
    Journal {
        /// Terminal ID
        id: u16,

        /// Start at this command index instead of the newest ones
        #[arg(long, value_name = "INDEX")]
        from: Option<u64>,

        /// Maximum number of records
        #[arg(long, default_value_t = crate::journal::JOURNAL_LIMIT)]
        limit: u16,

        /// One JSON object per record
        #[arg(long)]
        json: bool,
    },

    /// Print one command's output (needs OSC 133 shell integration).
    ///
    /// Defaults to the newest command. With --wait, blocks server-side until
    /// the command finishes and exits with its status (124 if the wait timed
    /// out), which is how to run something in a live shell and collect the
    /// result:
    ///   blit terminal send 3 'cargo test\n'
    ///   blit terminal output 3 --wait 600
    Output {
        /// Terminal ID
        id: u16,

        /// Command index from `blit terminal journal` (default: newest)
        index: Option<u64>,

        /// Block up to SECONDS for the command to finish first
        #[arg(long, value_name = "SECONDS")]
        wait: Option<u64>,

        /// Cap the output; the reply says where to continue from
        #[arg(long, value_name = "BYTES", default_value_t = crate::journal::OUTPUT_MAX_BYTES)]
        max_bytes: u32,

        /// JSON: the record plus its output text
        #[arg(long)]
        json: bool,
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

    /// Send a mouse event to a terminal.
    ///
    /// Coordinates are zero-based cell positions, matching browser terminal
    /// mouse reporting. The server translates the event using the terminal's
    /// active mouse mode/encoding (X10, normal, button-motion, any-motion, SGR).
    /// Examples:
    ///   blit terminal mouse 3 click 10 5
    ///   blit terminal mouse 3 down 10 5 --button right
    ///   blit terminal mouse 3 move 12 5 --button left
    ///   blit terminal mouse 3 wheel-up 10 5
    Mouse {
        /// Terminal ID
        id: u16,

        /// Mouse event: down, up, move, click, hover, wheel-up, or wheel-down
        event: String,

        /// Zero-based terminal column
        col: u16,

        /// Zero-based terminal row
        row: u16,

        /// Mouse button for down/up/click/move
        #[arg(long, short = 'b', default_value = "left")]
        button: String,
    },

    /// Click at terminal cell coordinates.
    ///
    /// Shorthand for terminal mouse ID click COL ROW. Coordinates are
    /// zero-based cells, not pixels.
    Click {
        /// Terminal ID
        id: u16,

        /// Zero-based terminal column
        col: u16,

        /// Zero-based terminal row
        row: u16,

        /// Mouse button: left, middle, or right
        #[arg(long, short = 'b', default_value = "left")]
        button: String,
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

    /// Arm, refresh, or clear a terminal's server-enforced deadline
    ///
    /// The countdown restarts from now on every call, so repeating it on an
    /// interval makes it a dead-man switch: the terminal outlives the client
    /// by at most one period. 0 clears it.
    Deadline {
        /// Terminal ID
        id: u16,

        /// Seconds from now, or 0 to clear
        seconds: u64,
    },

    /// Send a signal to a terminal's process group
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

    /// Attach this terminal to a remote one (Ctrl-] detaches)
    ///
    /// Puts the local tty in raw mode, forwards keystrokes, and repaints
    /// the remote grid. Needs a real terminal on stdin. Exits with the
    /// remote program's status if it finishes while attached.
    Attach {
        /// Terminal ID
        id: u16,
    },

    /// Set a terminal's viewport size
    ///
    /// The size is a client *request*: the server reconciles the desired
    /// sizes of every attached viewer, so another connected client can
    /// hold the grid larger than this.
    Resize {
        /// Terminal ID
        id: u16,

        /// Columns
        cols: u16,

        /// Rows
        rows: u16,
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

// ── Client subcommands ───────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum ClientCommand {
    /// List other connected clients and subscriptions as TSV
    #[command(alias = "ls")]
    List,

    /// Disconnect another client
    Kick {
        /// Client ID from `blit client list`
        id: u64,

        /// Reason shown to the disconnected client
        #[arg(short, long)]
        reason: Option<String>,
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

        /// Mouse button: left, right, middle, back, or forward [default: left]
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

    /// Scroll a surface
    ///
    /// AMOUNT is in wheel detents — one notch of a physical wheel, which
    /// is what an app treats as a scroll step. Positive scrolls down (or
    /// right with --horizontal). Fractions are allowed.
    Scroll {
        /// Surface ID
        id: u16,

        /// Wheel detents; positive = down/right
        amount: f64,

        /// Scroll horizontally instead of vertically
        #[arg(long)]
        horizontal: bool,

        /// Send it the way a trackpad does: smooth surface pixels with no
        /// detent count, and `finger` as the source.
        ///
        /// The default speaks as a wheel, which carries `axis_value120` and
        /// so takes an entirely different path through every toolkit -- and
        /// through Xwayland, which reads detents outright when they are
        /// present and divides the smooth value when they are not. A wheel
        /// test therefore says nothing about trackpad scrolling.
        #[arg(long)]
        smooth: bool,
    },

    /// Give a surface keyboard and pointer focus
    Focus {
        /// Surface ID
        id: u16,
    },

    /// Commit literal UTF-8 text to a surface
    ///
    /// Unlike `type`, this sends the text itself rather than synthesised
    /// US-QWERTY keystrokes, so non-ASCII characters actually arrive. It
    /// has no {braces} syntax — use `key` for special keys.
    Text {
        /// Surface ID
        id: u16,

        /// Text to commit
        text: String,
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
        /// Accepted values: h264, av1, h264-444, av1-444 — the `-444`
        /// variants also announce 4:4:4 chroma, which is what makes the
        /// server pick a 4:4:4 encoder.
        /// Default: all codecs.
        #[arg(short, long, value_delimiter = ',')]
        codec: Vec<String>,

        /// Ask for the surface at this size, as WIDTHxHEIGHT in physical
        /// pixels (e.g. 5120x2880), optionally at a device pixel ratio:
        /// WIDTHxHEIGHT@DPR (e.g. 1200x900@3).  Default: whatever other
        /// viewers have already negotiated, at 1x.
        ///
        /// The ratio is what makes this viewer a high-DPI one for size
        /// mediation: the surface is composited at the highest ratio any
        /// viewer claims, and lower-ratio viewers are served a downscale of
        /// it.  Claiming one is the only way to exercise that split without
        /// a browser.
        ///
        /// Recording writes the bitstream to a file rather than decoding it,
        /// so this is also announced as the decode ceiling — which is what
        /// lets a recording reach resolutions above the H.264 ceiling that
        /// the server will not send to a client that hasn't asked.
        #[arg(short, long)]
        size: Option<String>,

        /// Encode only this recorder's stream at WIDTHxHEIGHT without
        /// resizing or participating in mediation for the compositor
        /// surface. Useful for exercising a server-side encoder when the
        /// native-size stream would use Vulkan Video.
        #[arg(long, conflicts_with = "size")]
        encode_size: Option<String>,

        /// Display refresh rate to advertise to the server. This drives the
        /// compositor source clock during browser-free timing tests.
        #[arg(long, default_value_t = 60)]
        fps: u16,

        /// Also write per-frame timing to this path as CSV:
        /// `pts_ms,arrival_ms,bytes,key`.
        ///
        /// `pts_ms` is the capture clock stamped at compositor commit;
        /// `arrival_ms` is when the frame reached this process.  Their
        /// difference is the delivery jitter a viewer has to absorb, so this
        /// measures the pipeline's timing without needing a browser.
        #[arg(long)]
        timing: Option<String>,
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

        /// Set the primary selection (what middle click pastes) instead of
        /// the clipboard.  Displaces whichever app currently owns it.
        #[arg(long)]
        primary: bool,

        /// Text to set (if omitted, reads from stdin)
        text: Option<String>,
    },
}

// ── Fs subcommands ───────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum FsCommand {
    /// Mirror a directory tree from the server, streaming changes
    ///
    /// Prints the initial snapshot once it is coherent, then one line per
    /// change (`+` added, `~` modified, `-` deleted, `>` moved). With
    /// --json, emits one NDJSON event per record (`upsert`, `delete`,
    /// `move`, plus `reset`/`sync` staging markers and `synced`/`closed`).
    Sync {
        /// Path on the server (absolute, or relative to the server's cwd)
        path: String,

        /// Sync file contents too (hashes always sync)
        #[arg(long)]
        content: bool,

        /// Watch only the path and its immediate children
        #[arg(long)]
        no_recursive: bool,

        /// Honor .gitignore, $GIT_DIR/info/exclude and core.excludesFile
        #[arg(long)]
        gitignore: bool,

        /// Honor .ignore files (ripgrep's convention)
        #[arg(long)]
        dot_ignore: bool,

        /// Shorthand for --gitignore --dot-ignore --exclude-git
        #[arg(long)]
        ignore: bool,

        /// Skip .git directories and gitfiles
        #[arg(long)]
        exclude_git: bool,

        /// Skip paths matching a gitignore-syntax pattern (repeatable)
        #[arg(long, value_name = "PATTERN")]
        exclude: Vec<String>,

        /// Exit after the initial snapshot instead of streaming
        #[arg(long)]
        once: bool,

        /// NDJSON event output
        #[arg(long)]
        json: bool,
    },

    /// Write a file from stdin, with conflict detection
    ///
    /// Content is read from stdin. By default an unconditional overwrite;
    /// --create fails if the file exists, --if-hash writes only if the
    /// current content matches. Exit 1 on conflict.
    Write {
        /// Path to write, relative to --root
        path: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Write only if the current content hash equals this hex value
        #[arg(long, conflicts_with_all = ["create", "force"])]
        if_hash: Option<String>,

        /// Create only if the path does not already exist
        #[arg(long, conflicts_with_all = ["if_hash", "force"])]
        create: bool,

        /// Overwrite unconditionally (ignore any precondition)
        #[arg(long)]
        force: bool,

        /// Create missing parent directories
        #[arg(long)]
        parents: bool,

        /// fsync the file and its parent before returning
        #[arg(long)]
        durable: bool,

        /// File mode in octal (e.g. 644); default preserves or umask
        #[arg(long)]
        mode: Option<String>,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// Create a directory
    Mkdir {
        /// Path to create, relative to --root
        path: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Create missing parent directories
        #[arg(long)]
        parents: bool,

        /// Directory mode in octal (e.g. 700)
        #[arg(long)]
        mode: Option<String>,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// Remove a file or directory subtree
    Rm {
        /// Path to remove, relative to --root
        path: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Remove only if the current content hash equals this hex value
        #[arg(long)]
        if_hash: Option<String>,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// Rename or move a file or subtree
    Mv {
        /// Source path, relative to --root
        from: String,

        /// Destination path, relative to --root
        to: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Create missing parent directories of the destination
        #[arg(long)]
        parents: bool,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// Create a hard link, or a symlink with -s (like ln(1))
    Ln {
        /// Existing file path relative to --root; with -s, the verbatim
        /// symlink target (relative, absolute, or dangling)
        target: String,

        /// Link path to create, relative to --root
        link: String,

        /// Create a symlink instead of a hard link
        #[arg(short = 's', long)]
        symlink: bool,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Replace only if the current entry's content hash equals this
        /// hex value (a symlink's hash covers its target bytes)
        #[arg(long, conflicts_with = "force")]
        if_hash: Option<String>,

        /// Replace an existing entry unconditionally
        #[arg(long)]
        force: bool,

        /// Create missing parent directories of the link
        #[arg(long)]
        parents: bool,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// Search file contents across a tree (docs/design/fs-grep.md)
    ///
    /// Examples:
    ///   blit fs grep needle                    # literal, case-insensitive
    ///   blit fs grep -e 'fn \\w+' --root crates # regex
    ///   blit fs grep -sw Config                # case-sensitive whole word
    ///   blit fs grep --no-ignore TODO          # include gitignored files
    ///   blit fs grep -l needle                 # matching paths only
    ///
    /// Prints PATH:LINE:TEXT, like grep(1). Exits 1 when nothing matched.
    Grep {
        /// Pattern: a literal string, or a regex with -e
        pattern: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Treat the pattern as a regular expression
        #[arg(short = 'e', long)]
        regex: bool,

        /// Match case exactly (the default is case-insensitive)
        #[arg(short = 's', long)]
        case_sensitive: bool,

        /// Match whole words only
        #[arg(short = 'w', long)]
        word: bool,

        /// Search gitignored files too; they rank after tracked ones.
        /// Much slower on a tree with build output.
        #[arg(long)]
        no_ignore: bool,

        /// Stop after this many matching files (0 = server default)
        #[arg(short = 'm', long, default_value_t = 0)]
        max_matches: u16,

        /// Print only the paths that contain a match
        #[arg(short = 'l', long)]
        files_with_matches: bool,

        /// NDJSON output, one object per match
        #[arg(long)]
        json: bool,
    },

    /// Print a file's contents to stdout (bytes, unmodified)
    Cat {
        /// File path relative to --root
        path: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,
    },

    /// Fuzzy-find files by path (docs/design/fs-search.md)
    ///
    /// Scores a subsequence match over each root-relative path, best
    /// first. Exits 1 when nothing matched.
    Find {
        /// Query to match against paths
        query: String,

        /// Root directory on the server (relative to the client's cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Maximum number of paths to return
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: u16,

        /// NDJSON output, one object per path
        #[arg(long)]
        json: bool,
    },
}

// ── Git subcommands ──────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum GitCommand {
    /// Branch, ahead/behind, stash, and working-tree status
    Status {
        /// Repository location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        repo: String,

        /// Keep watching, reprinting whenever the status changes
        #[arg(long)]
        watch: bool,

        /// NDJSON output (one state snapshot per line)
        #[arg(long)]
        json: bool,
    },

    /// Commit history, newest first
    ///
    /// Examples:
    ///   blit git log                 # HEAD
    ///   blit git log v1.0            # from a tag
    ///   blit git log main..feature   # a range
    ///   blit git log --watch main..HEAD
    ///   blit git log --follow -- src/main.rs
    Log {
        /// Revision or range to log (default: HEAD). A ref, (short) oid,
        /// HEAD~N, or a range A..B / A...B.
        rev: Option<String>,

        /// Restrict to commits touching this path (after `--`)
        #[arg(last = true)]
        pathspec: Vec<String>,

        /// Repository location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        repo: String,

        /// Maximum commits to print
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u16,

        /// Keep the log live, refreshing as its endpoint refs move
        #[arg(long)]
        watch: bool,

        /// Follow a single file across renames (needs a path)
        #[arg(long)]
        follow: bool,

        /// Follow only the first parent of each merge
        #[arg(long)]
        first_parent: bool,

        /// Include the full commit message, not just the subject
        #[arg(long)]
        full_message: bool,

        /// Topological order (parents after children) within the page
        #[arg(long)]
        topo: bool,

        /// NDJSON output (one commit per line)
        #[arg(long)]
        json: bool,
    },

    /// Changed files (unstaged by default), optionally with per-file hunks
    ///
    /// Examples:
    ///   blit git diff                # worktree vs index (unstaged)
    ///   blit git diff --staged       # index vs HEAD (staged)
    ///   blit git diff main           # worktree vs a commit
    ///   blit git diff main dev       # between two commits
    ///   blit git diff main..dev      # same as: main dev
    ///   blit git diff main...dev     # since they diverged (merge base)
    ///   blit git diff --merge-base main
    ///                                # worktree vs where main forked
    ///   blit git diff HEAD~2 -- src  # limited to a path
    Diff {
        /// Revisions to compare: none (worktree vs index), one (that
        /// revision vs the worktree, or the index with --staged), two
        /// (between them), or a single A..B / A...B range. Each is a ref,
        /// (short) oid, or HEAD~N.
        revs: Vec<String>,

        /// Restrict to this path (after `--`)
        #[arg(last = true)]
        pathspec: Vec<String>,

        /// Repository location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        repo: String,

        /// Compare the index to HEAD (staged changes) instead of the worktree
        #[arg(long)]
        staged: bool,

        /// Compare against where the revision forked, not its tip, so the
        /// new side can be the worktree — `A...B` always ends at a commit
        /// (git's --merge-base)
        #[arg(long)]
        merge_base: bool,

        /// Show per-file hunks, not just the changed-file list
        #[arg(short = 'p', long)]
        patch: bool,

        /// With -p, include binary content as a GIT binary patch block, so
        /// `git apply --binary` can replay it (git's --binary)
        #[arg(long)]
        binary: bool,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Print an object's bytes (like `git cat-file` / `git show`)
    ///
    /// Examples:
    ///   blit git show HEAD:src/main.rs   # a file at a revision
    ///   blit git show v1.0:Cargo.toml
    ///   blit git show HEAD               # the commit object itself
    Show {
        /// REV[:PATH]. Omit PATH for the commit object; omit REV
        /// (`:path`) for HEAD.
        spec: String,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// Stop after this many bytes
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_len: u32,
    },

    /// List one tree level (like `git ls-tree`)
    ///
    /// TSV: MODE TYPE OID<TAB>NAME. Not recursive — pass a path to
    /// descend.
    LsTree {
        /// REV[:PATH]; omit PATH for the root tree
        spec: String,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// List the index (like `git ls-files --stage`)
    ///
    /// TSV: MODE STAGE OID<TAB>PATH. Conflicted paths appear once per
    /// stage (1 = base, 2 = ours, 3 = theirs).
    LsFiles {
        /// Restrict to this path prefix
        #[arg(default_value = "")]
        path: String,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Best common ancestors of two or more revisions
    ///
    /// Exits 1 when the histories are unrelated, as git does.
    MergeBase {
        /// Two or more revisions
        #[arg(required = true, num_args = 2..)]
        revs: Vec<String>,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Who last touched each line (like `git blame`)
    ///
    /// Prints `OID LINE) TEXT`-less attribution: one row per contiguous
    /// range, since the ranges are what the server computes. Resolve the
    /// oids with `blit git log` if you want authors.
    Blame {
        /// File to blame
        path: String,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// Revision to blame from (default: HEAD)
        #[arg(long)]
        rev: Option<String>,

        /// First line (1-based); with --lines, the start of the range
        #[arg(long)]
        start: Option<u32>,

        /// How many lines to attribute — a viewport blame is cheap, a
        /// whole large file is not
        #[arg(long)]
        lines: Option<u32>,

        /// Follow renames (git's -M)
        #[arg(short = 'M', long)]
        follow: bool,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// A ref's reflog (like `git reflog`)
    ///
    /// The only way to name an oid no longer reachable from any ref — an
    /// amended or reset-away commit that `log` cannot reach.
    Reflog {
        /// Ref to read (default: HEAD)
        #[arg(default_value = "")]
        ref_name: String,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// Maximum entries to print
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u16,

        /// Oldest first (default is newest first, like git)
        #[arg(long)]
        reverse: bool,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Find repositories under a path
    ///
    /// TSV: WORKDIR<TAB>GITDIR. Deduped by gitdir, so several paths
    /// resolving to one repository report once.
    Discover {
        /// Directory to search (default: server cwd)
        #[arg(default_value = ".")]
        path: String,

        /// How deep to descend
        #[arg(long, default_value_t = 4)]
        depth: u8,

        /// Descend into repositories after finding one
        #[arg(long)]
        nested: bool,

        /// Report bare repositories too
        #[arg(long)]
        bare: bool,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Fetch from a remote, reporting per-ref what happened
    ///
    /// Runs the server's own `git fetch`, so its credential helpers and
    /// config apply. Exits 1 if any ref was refused — unlike `git fetch`,
    /// which can exit 0 having refused one refspec of several.
    Fetch {
        /// Remote name (default: origin)
        #[arg(default_value = "")]
        remote: String,

        /// Refspecs to fetch (default: the remote's configured ones)
        refspecs: Vec<String>,

        /// Repository path on the server
        #[arg(long, default_value = ".")]
        repo: String,

        /// Delete remote-tracking refs the remote no longer has
        #[arg(long)]
        prune: bool,

        /// Anchor fetched tips under refs/blit/fetch/ so a concurrent gc
        /// cannot prune them
        #[arg(long)]
        anchor: bool,

        /// Give up after this many seconds
        #[arg(long, default_value_t = 120)]
        timeout: u32,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },
}

// ── Kv subcommands ───────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum KvCommand {
    /// Print one value's bytes to stdout
    ///
    /// Exits 1 when the key is absent — that is an empty answer, not an
    /// error, so `if blit kv get k >/dev/null; then` reads naturally.
    Get {
        /// Key
        key: String,
    },

    /// Set one value, from an argument or stdin
    ///
    /// Compare-and-swap by default: pass --if-hash to require the current
    /// value, or --force to overwrite unconditionally. Exit 1 on conflict.
    Put {
        /// Key
        key: String,

        /// Value; omit to read it from stdin
        value: Option<String>,

        /// Write only if the current value's hash equals this hex value
        #[arg(long, conflicts_with = "force")]
        if_hash: Option<String>,

        /// Overwrite unconditionally
        #[arg(long)]
        force: bool,

        /// Wait for the write to reach disk before answering
        #[arg(long)]
        durable: bool,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// Delete one key
    Rm {
        /// Key
        key: String,

        /// Delete only if the current value's hash equals this hex value
        #[arg(long, conflicts_with = "force")]
        if_hash: Option<String>,

        /// Delete unconditionally
        #[arg(long)]
        force: bool,

        /// Wait for the delete to reach disk before answering
        #[arg(long)]
        durable: bool,

        /// JSON result output
        #[arg(long)]
        json: bool,
    },

    /// List the keys under a prefix (TSV: KEY, SIZE)
    Ls {
        /// Key prefix; omit for the whole store
        #[arg(default_value = "")]
        prefix: String,

        /// Keep streaming changes after the first snapshot
        #[arg(long)]
        watch: bool,

        /// Include values (TSV gains a third column)
        #[arg(long)]
        values: bool,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },
}

// ── Lsp subcommands ──────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum LspCommand {
    /// Definition of the symbol at PATH:LINE:COL
    Def {
        /// Position, 1-based (e.g. src/main.rs:10:4)
        spec: String,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output (one location per line)
        #[arg(long)]
        json: bool,
    },

    /// References to the symbol at PATH:LINE:COL
    Refs {
        /// Position, 1-based (e.g. src/main.rs:10:4)
        spec: String,

        /// Include the declaration itself
        #[arg(long)]
        declaration: bool,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output (one location per line)
        #[arg(long)]
        json: bool,
    },

    /// Type and docs of the symbol at PATH:LINE:COL
    Hover {
        /// Position, 1-based (e.g. src/main.rs:10:4)
        spec: String,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Completions at PATH:LINE:COL (TSV: LABEL, KIND, DETAIL)
    ///
    /// What a language server offers for the identifier being typed
    /// there. `cut -f1` gives a plain label list.
    Complete {
        /// Position, 1-based (e.g. src/main.rs:10:4)
        spec: String,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Signature help at PATH:LINE:COL
    ///
    /// Prints the signature, underlines the active parameter, then any
    /// documentation.
    Signature {
        /// Position, 1-based (e.g. src/main.rs:10:4)
        spec: String,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output
        #[arg(long)]
        json: bool,
    },

    /// Search workspace symbols, or outline one file with --file
    Symbols {
        /// Fuzzy symbol query (workspace-wide; empty lists everything
        /// the server returns)
        query: Option<String>,

        /// Outline this file instead of searching the workspace
        #[arg(long)]
        file: Option<String>,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output (one symbol per line)
        #[arg(long)]
        json: bool,
    },

    /// Current diagnostics for the workspace or one path
    ///
    /// Exit code 1 when diagnostics exist, 0 when clean.
    #[command(alias = "diag")]
    Diagnostics {
        /// Only diagnostics for this file or directory
        path: Option<String>,

        /// Keep watching, reprinting as diagnostics change
        #[arg(long)]
        watch: bool,

        /// Wait for language servers to finish indexing first
        #[arg(long)]
        wait: bool,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output (one diagnostic per line)
        #[arg(long)]
        json: bool,
    },

    /// Rename plan for the symbol at PATH:LINE:COL (prints the edits,
    /// never applies them)
    Rename {
        /// Position, 1-based (e.g. src/main.rs:10:4)
        spec: String,

        /// The new name
        new_name: String,

        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// NDJSON output (one edit per line)
        #[arg(long)]
        json: bool,
    },

    /// Block until the workspace's language servers are ready
    Wait {
        /// Workspace location on the server (default: server cwd)
        #[arg(long, default_value = ".")]
        root: String,

        /// Give up after this many seconds
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },

    /// List running language servers
    #[command(alias = "ls")]
    List {
        /// NDJSON output (one server per line)
        #[arg(long)]
        json: bool,
    },

    /// Stop a language server by ref (see `blit lsp list`)
    Stop {
        /// Server ref from `blit lsp list`
        server_ref: u16,
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

    /// Disable or enable a named remote without removing it.
    /// Disabled remotes are kept in blit.remotes (commented out) and excluded
    /// from connection resolution until re-enabled.
    Toggle {
        /// Name of the remote to toggle
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn server_help_advertises_every_extension_and_channel_setting() {
        let mut server = Cli::command()
            .find_subcommand("server")
            .expect("server subcommand")
            .clone();
        let help = server.render_long_help().to_string();
        for flag in [
            "--no-extensions",
            "--no-channels",
            "--ext-max-running",
            "--ext-max-persistent",
            "--ext-max-transient",
            "--ext-follow-max-per-client",
            "--ext-follow-max",
            "--ext-argument-store-max",
            "--ext-module-max",
            "--ext-object-cache-max",
            "--ext-object-cache-max-entries",
            "--ext-upload-max-per-client",
            "--ext-upload-max-active",
            "--ext-upload-timeout",
            "--ext-pending-timeout",
            "--ext-max-validating",
            "--ext-memory-max",
            "--ext-outbox-max",
            "--ext-outbox-messages-max",
            "--ext-outbox-timeout",
            "--ext-job-max-per-client",
            "--ext-job-max",
            "--ext-job-pending-max-per-client",
            "--ext-job-pending-max",
            "--ext-job-bytes-max-per-client",
            "--ext-job-bytes-max",
            "--ext-output-retain-max",
            "--ext-terminal-retain",
            "--ext-command-store-max",
            "--ext-command-snapshot-max",
            "--ext-table-elements-max",
            "--ext-value-stack-max",
            "--ext-call-depth-max",
            "--ext-stack-size",
            "--ext-fuel-slice",
            "--channel-max-listen-per-client",
            "--channel-max-listeners",
            "--channel-max-per-client",
            "--channel-max-connected",
            "--channel-buffer-max",
        ] {
            assert!(help.contains(flag), "server help is missing {flag}");
        }
    }

    #[test]
    fn server_accepts_both_family_flags_and_capacity_flags() {
        let cli = Cli::try_parse_from([
            "blit",
            "server",
            "--no-extensions",
            "--no-channels",
            "--ext-max-running",
            "4",
            "--ext-object-cache-max",
            "2147483648",
            "--channel-max-connected",
            "64",
            "--channel-buffer-max",
            "134217728",
        ])
        .unwrap();
        let Command::Server { deployment, .. } = cli.command else {
            panic!("expected server command");
        };
        deployment.into_overrides().unwrap();
    }

    #[test]
    fn server_name_defaults_and_is_validated() {
        let cli = Cli::try_parse_from(["blit", "server", "--name", "work-tree.2"]).unwrap();
        let Command::Server { name, .. } = cli.command else {
            panic!("expected server command");
        };
        assert_eq!(name.as_str(), "work-tree.2");

        let cli = Cli::try_parse_from(["blit", "server"]).unwrap();
        let Command::Server { name, .. } = cli.command else {
            panic!("expected server command");
        };
        assert_eq!(name.as_str(), "default");

        let error = Cli::try_parse_from(["blit", "server", "--name", "../other"])
            .err()
            .expect("path-like names must be rejected");
        assert!(error.to_string().contains("server name"));
    }
}
