use blit_webrtc_forwarder::{Config, DEFAULT_SIGNAL_URL, DEFAULT_URL_TEMPLATE};
use clap::Parser;

#[derive(Parser)]
#[command(name = "blit-webrtc-forwarder", version, about = "Forward a blit-server session over WebRTC")]
struct Cli {
    /// Path to the blit-server Unix socket
    #[arg(long)]
    socket: String,

    /// Passphrase for the session (or set BLITZ_PASSPHRASE env var)
    #[arg(long, env = "BLITZ_PASSPHRASE")]
    passphrase: String,

    /// Signaling service URL
    #[arg(long, default_value = DEFAULT_SIGNAL_URL)]
    signal_url: String,

    /// URL template to display (use {secret} as placeholder)
    #[arg(long, default_value = DEFAULT_URL_TEMPLATE)]
    url: String,

    /// Don't print the sharing URL
    #[arg(long)]
    quiet: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    blit_webrtc_forwarder::run(Config {
        sock_path: cli.socket,
        signal_url: cli.signal_url,
        passphrase: cli.passphrase,
        url_template: Some(cli.url),
        quiet: cli.quiet,
    })
    .await;
}
