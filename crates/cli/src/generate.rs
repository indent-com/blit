use crate::cli;
use clap::{Arg, Command, CommandFactory};
use clap_complete::Shell;
use std::fs;
use std::path::Path;

/// Build a clap Command for blit-server (mirrors its manual arg parser).
fn blit_server_cmd() -> Command {
    Command::new("blit-server")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Terminal streaming server")
        .long_about(
            "blit-server multiplexes PTYs on a Unix socket. It tracks terminal state, \
             diffs cell grids into LZ4-compressed binary frames, and publishes updates to \
             connected clients.\n\n\
             Supports systemd socket activation via LISTEN_FDS=1.",
        )
        .arg(
            Arg::new("socket")
                .long("socket")
                .value_name("PATH")
                .help("IPC socket/pipe path (or set BLIT_SOCK)"),
        )
        .arg(
            Arg::new("fd-channel")
                .long("fd-channel")
                .value_name("FD")
                .help("Accept clients via fd-passing on FD (Unix only, or set BLIT_FD_CHANNEL)"),
        )
        .arg(
            Arg::new("shell-flags")
                .long("shell-flags")
                .value_name("FLAGS")
                .help("Shell flags (default: li, or set BLIT_SHELL_FLAGS)"),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .short('v')
                .action(clap::ArgAction::SetTrue)
                .help("Enable verbose logging (or set BLIT_VERBOSE=1)"),
        )
        .arg(
            Arg::new("path")
                .value_name("PATH")
                .help("Socket path (positional alternative to --socket)"),
        )
        .after_help(
            "ENVIRONMENT:\n    \
             BLIT_SOCK               Unix socket path\n    \
             SHELL                   Shell to spawn for new PTYs (default: /bin/sh)\n    \
             BLIT_SHELL_FLAGS        Shell flags (default: li)\n    \
             BLIT_SCROLLBACK         Scrollback buffer rows per PTY (default: 1000000)\n    \
             BLIT_FD_CHANNEL         File descriptor for fd-passing channel\n    \
             BLIT_SURFACE_ENCODERS   Comma-separated encoder priority list\n    \
             BLIT_SURFACE_QUALITY    Surface quality: low, medium, high, ultra\n    \
             BLIT_CHROMA             Chroma subsampling: 420 or 444 (default: 4:4:4)\n    \
             BLIT_VAAPI_DEVICE       VA-API render node (default: /dev/dri/renderD128)\n    \
             BLIT_CUDA_DEVICE        CUDA device ordinal for NVENC (default: 0)\n    \
             BLIT_MAX_CONNECTIONS    Max simultaneous connections (0 = unlimited)\n    \
             BLIT_MAX_PTYS           Max simultaneous PTYs (0 = unlimited)",
        )
}

/// Build a clap Command for blit-gateway (mirrors its env-var config).
fn blit_gateway_cmd() -> Command {
    Command::new("blit-gateway")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Terminal streaming WebSocket gateway")
        .long_about(
            "blit-gateway serves the browser UI and proxies WebSocket traffic to one or \
             more blit-server(1) Unix sockets. It handles passphrase authentication and \
             serves static web assets.\n\n\
             Use it for always-on deployments behind a reverse proxy or as a systemd \
             service. For local and SSH use, the blit(1) CLI embeds equivalent gateway \
             functionality and is simpler to run.\n\n\
             When BLIT_QUIC=1, the gateway also listens for WebTransport (HTTP/3) \
             connections on the same address, requiring TLS certificates.\n\n\
             All configuration is via environment variables.",
        )
        .after_help(
            "ENVIRONMENT:\n    \
             BLIT_PASSPHRASE    Browser passphrase (required)\n    \
             BLIT_ADDR          Listen address (default: 0.0.0.0:3264)\n    \
             BLIT_REMOTES       Path to remotes file (default: ~/.config/blit/blit.remotes)\n    \
             BLIT_FONT_DIRS     Colon-separated extra font directories\n    \
             BLIT_CORS          CORS origin for font routes (* or specific origin)\n    \
             BLIT_QUIC          Set to 1 to enable WebTransport (QUIC/HTTP3)\n    \
             BLIT_TLS_CERT      PEM certificate file (for WebTransport)\n    \
             BLIT_TLS_KEY       PEM private key file (for WebTransport)\n    \
             BLIT_STORE_CONFIG  Set to 1 to sync browser settings to ~/.config/blit/blit.conf",
        )
}

/// Build a clap Command for blit-webrtc-forwarder.
fn blit_webrtc_forwarder_cmd() -> Command {
    Command::new("blit-webrtc-forwarder")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Forward a blit-server terminal over WebRTC")
        .long_about(
            "blit-webrtc-forwarder connects to a blit-server(1) Unix socket and \
             bridges it to browsers over WebRTC data channels. It handles signaling, \
             STUN/TURN NAT traversal, and peer-to-peer connections.\n\n\
             For most use cases, blit share is simpler -- it runs the forwarder \
             in-process and auto-starts a server if needed. The standalone binary is \
             for custom deployments where the server is managed separately.",
        )
        .arg(
            Arg::new("socket")
                .long("socket")
                .value_name("PATH")
                .env("BLIT_SOCK")
                .required(true)
                .help("Path to the blit-server Unix socket"),
        )
        .arg(
            Arg::new("passphrase")
                .long("passphrase")
                .value_name("PASSPHRASE")
                .env("BLIT_PASSPHRASE")
                .required(true)
                .help("Share passphrase"),
        )
        .arg(
            Arg::new("hub")
                .long("hub")
                .value_name("URL")
                .env("BLIT_HUB")
                .default_value("https://hub.blit.sh")
                .help("Signaling hub URL"),
        )
        .arg(
            Arg::new("message")
                .long("message")
                .value_name("TEMPLATE")
                .help("Override the message template (use {secret} as placeholder)"),
        )
        .arg(
            Arg::new("quiet")
                .long("quiet")
                .action(clap::ArgAction::SetTrue)
                .help("Don't print the sharing URL"),
        )
        .arg(
            Arg::new("verbose")
                .long("verbose")
                .action(clap::ArgAction::SetTrue)
                .help("Print detailed connection diagnostics to stderr"),
        )
}

fn generate_man_page(cmd: Command, out_dir: &Path) {
    let name = cmd.get_name().to_string();
    let man = clap_mangen::Man::new(cmd);
    let mut buf = Vec::new();
    man.render(&mut buf).expect("failed to render man page");
    let path = out_dir.join(format!("{name}.1"));
    fs::write(&path, buf).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

fn generate_completions(mut cmd: Command, out_dir: &Path, name: &str) {
    for shell in [Shell::Fish, Shell::Bash, Shell::Zsh] {
        let dir = match shell {
            Shell::Fish => out_dir.join("fish/vendor_completions.d"),
            Shell::Bash => out_dir.join("bash-completion/completions"),
            Shell::Zsh => out_dir.join("zsh/site-functions"),
            _ => unreachable!(),
        };
        fs::create_dir_all(&dir).unwrap();
        clap_complete::generate_to(shell, &mut cmd, name, &dir).unwrap();
    }
}

pub fn run(output: &str) {
    let base = Path::new(output);

    // Man pages
    let man_dir = base.join("man/man1");
    fs::create_dir_all(&man_dir).unwrap();

    clap_mangen::generate_to(cli::Cli::command(), &man_dir).expect("failed to generate man pages");
    generate_man_page(blit_server_cmd(), &man_dir);
    generate_man_page(blit_gateway_cmd(), &man_dir);
    generate_man_page(blit_webrtc_forwarder_cmd(), &man_dir);

    // Shell completions (for the main blit CLI only)
    generate_completions(cli::Cli::command(), base, "blit");
}
