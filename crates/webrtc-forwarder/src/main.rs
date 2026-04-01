use blit_webrtc_forwarder::{Config, DEFAULT_HUB_URL, DEFAULT_URL_TEMPLATE};
use clap::Parser;

#[derive(Parser)]
#[command(name = "blit-webrtc-forwarder", version, about = "Forward a blit-server session over WebRTC")]
struct Cli {
    /// Path to the blit-server Unix socket
    #[arg(long)]
    socket: String,

    /// Passphrase for the session (or set BLIT_PASSPHRASE env var)
    #[arg(long, env = "BLIT_PASSPHRASE")]
    passphrase: String,

    /// Signaling hub URL
    #[arg(long, default_value = DEFAULT_HUB_URL, env = "BLIT_HUB")]
    hub: String,

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
        signal_url: blit_webrtc_forwarder::normalize_hub(&cli.hub),
        passphrase: cli.passphrase,
        url_template: Some(cli.url),
        quiet: cli.quiet,
    })
    .await;
}
