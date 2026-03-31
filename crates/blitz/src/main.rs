mod peer;
mod server;
mod signaling;

use clap::Parser;
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::sync::mpsc;

const DEFAULT_SIGNAL_URL: &str = "wss://cloud.blit.sh";
const DEFAULT_URL_TEMPLATE: &str = "https://blit.sh/#{secret}";

#[derive(Parser)]
#[command(name = "blitz", version, about = "Share a terminal session via WebRTC")]
struct Cli {
    /// Passphrase for the session (default: random UUID)
    #[arg(long)]
    passphrase: Option<String>,

    /// Signaling service URL
    #[arg(long, default_value = DEFAULT_SIGNAL_URL)]
    signal_url: String,

    /// URL template to display (use {secret} as placeholder)
    #[arg(long, default_value = DEFAULT_URL_TEMPLATE)]
    url: String,

    /// Connect to an existing blit-server socket instead of starting an embedded one
    #[arg(long)]
    socket: Option<String>,

    /// Don't print the sharing URL
    #[arg(long)]
    quiet: bool,
}

fn derive_signing_key(passphrase: &str) -> SigningKey {
    let mut hasher = Sha256::new();
    hasher.update(passphrase.as_bytes());
    let seed: [u8; 32] = hasher.finalize().into();
    SigningKey::from_bytes(&seed)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct PeerState {
    handle: tokio::task::JoinHandle<()>,
    signal_tx: mpsc::UnboundedSender<serde_json::Value>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let passphrase = cli
        .passphrase
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let signing_key = derive_signing_key(&passphrase);
    let public_key_hex = hex_encode(signing_key.verifying_key().as_bytes());

    let sock_path = match &cli.socket {
        Some(path) => path.clone(),
        None => {
            let path = server::start_embedded(&passphrase).await;
            eprintln!("embedded server listening on {path}");
            path
        }
    };

    if !cli.quiet {
        let url = cli.url.replace("{secret}", &passphrase);
        println!("{url}");
    }

    let (sig_event_tx, mut sig_event_rx) = mpsc::unbounded_channel::<signaling::Event>();
    let (sig_send_tx, sig_send_rx) = mpsc::unbounded_channel::<String>();
    let signal_url = format!(
        "{}/channel/{}/producer",
        cli.signal_url.trim_end_matches('/'),
        public_key_hex,
    );

    tokio::spawn(signaling::connect(
        signal_url,
        signing_key.clone(),
        sig_event_tx,
        sig_send_rx,
    ));

    let mut peers: HashMap<String, PeerState> = HashMap::new();

    while let Some(event) = sig_event_rx.recv().await {
        match event {
            signaling::Event::Registered { session_id } => {
                eprintln!("registered with signaling server (session {session_id})");
                for (id, state) in peers.drain() {
                    eprintln!("aborting stale peer task: {id}");
                    state.handle.abort();
                }
            }
            signaling::Event::PeerJoined { session_id } => {
                eprintln!("consumer joined: {session_id}");
                let (peer_sig_tx, peer_sig_rx) = mpsc::unbounded_channel();
                let peer_id = session_id.clone();
                let sock = sock_path.clone();
                let out_tx = sig_send_tx.clone();
                let key = signing_key.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) =
                        peer::handle_peer(peer_id.clone(), sock, peer_sig_rx, out_tx, key).await
                    {
                        eprintln!("peer {peer_id} error: {e}");
                    }
                });
                peers.insert(
                    session_id,
                    PeerState {
                        handle,
                        signal_tx: peer_sig_tx,
                    },
                );
            }
            signaling::Event::PeerLeft { session_id } => {
                eprintln!("consumer left: {session_id}");
                if let Some(state) = peers.remove(&session_id) {
                    state.handle.abort();
                }
            }
            signaling::Event::Signal { from, data } => {
                if let Some(state) = peers.get(&from) {
                    let _ = state.signal_tx.send(data);
                } else {
                    eprintln!("signal from unknown peer {from}, ignoring");
                }
            }
            signaling::Event::Error { message } => {
                eprintln!("signaling error: {message}");
            }
        }
    }
}
