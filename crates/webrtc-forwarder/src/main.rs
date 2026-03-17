use blit_webrtc_forwarder::{Config, DEFAULT_HUB_URL};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "blit-webrtc-forwarder",
    version,
    about = "Forward a blit-server session over WebRTC"
)]
struct Cli {
    /// Path to the blit-server Unix socket
    #[arg(long, env = "BLIT_SOCK")]
    socket: String,

    /// Passphrase for the session (or set BLIT_PASSPHRASE env var)
    #[arg(long, env = "BLIT_PASSPHRASE")]
    passphrase: String,

    /// Signaling hub URL
    #[arg(long, default_value = DEFAULT_HUB_URL, env = "BLIT_HUB")]
    hub: String,

    /// Override the message template (use {secret} as placeholder); skips hub fetch
    #[arg(long)]
    message: Option<String>,

    /// Don't print the sharing URL
    #[arg(long)]
    quiet: bool,

    /// Print detailed connection diagnostics to stderr
    #[arg(long)]
    verbose: bool,
}

#[tokio::main]
async fn main() {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let cli = Cli::parse();
    blit_webrtc_forwarder::run(Config {
        sock_path: cli.socket,
        signal_url: blit_webrtc_forwarder::normalize_hub(&cli.hub),
        passphrase: cli.passphrase,
        message_override: cli.message,
        quiet: cli.quiet,
        verbose: cli.verbose,
    })
    .await;
}
