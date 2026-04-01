pub mod client;
pub mod ice;
mod peer;
pub mod signaling;
pub mod turn;

use ed25519_dalek::SigningKey;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

pub const DEFAULT_HUB_URL: &str = "hub.blit.sh";

/// Resolve the machine's default local IP (the one the OS would route outbound traffic from).
/// Returns `None` if no route exists. No packets are sent.
pub fn default_local_ip() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:80").ok()?;
    Some(probe.local_addr().ok()?.ip())
}
pub const DEFAULT_URL_TEMPLATE: &str = "https://blit.sh/#{secret}";

pub fn normalize_hub(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        return trimmed.to_string();
    }
    if trimmed.starts_with("https://") {
        return trimmed.replacen("https://", "wss://", 1);
    }
    if trimmed.starts_with("http://") {
        return trimmed.replacen("http://", "ws://", 1);
    }
    if trimmed.contains("localhost") || trimmed.contains("127.0.0.1") || trimmed.contains("[::1]") {
        return format!("ws://{trimmed}");
    }
    format!("wss://{trimmed}")
}

pub struct Config {
    pub sock_path: String,
    pub signal_url: String,
    pub passphrase: String,
    pub url_template: Option<String>,
    pub quiet: bool,
}

const PBKDF2_SALT: &[u8] = b"https://blit.sh";
const PBKDF2_ROUNDS: u32 = 100_000;

pub fn derive_signing_key(passphrase: &str) -> SigningKey {
    let mut seed = [0u8; 32];
    pbkdf2::<Hmac<Sha256>>(passphrase.as_bytes(), PBKDF2_SALT, PBKDF2_ROUNDS, &mut seed)
        .expect("HMAC can be initialized with any key length");
    SigningKey::from_bytes(&seed)
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct PeerState {
    handle: tokio::task::JoinHandle<()>,
    signal_tx: mpsc::UnboundedSender<serde_json::Value>,
    established: Arc<AtomicBool>,
}

pub async fn run(config: Config) {
    let signing_key = derive_signing_key(&config.passphrase);
    let public_key_hex = hex_encode(signing_key.verifying_key().as_bytes());

    if !config.quiet {
        if let Some(template) = &config.url_template {
            let url = template.replace("{secret}", &config.passphrase);
            println!("{url}");
        }
    }

    let ice_config = match ice::fetch_ice_config(&config.signal_url).await {
        Ok(cfg) => {
            eprintln!("fetched ICE config ({} servers)", cfg.ice_servers.len());
            Some(cfg)
        }
        Err(e) => {
            eprintln!("failed to fetch ICE config: {e}");
            None
        }
    };

    let (sig_event_tx, mut sig_event_rx) = mpsc::unbounded_channel::<signaling::Event>();
    let (sig_send_tx, sig_send_rx) = mpsc::unbounded_channel::<String>();
    let signal_url = format!(
        "{}/channel/{}/producer",
        config.signal_url.trim_end_matches('/'),
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
                let stale: Vec<String> = peers
                    .iter()
                    .filter(|(_, s)| !s.established.load(Ordering::Relaxed))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in stale {
                    if let Some(state) = peers.remove(&id) {
                        eprintln!("aborting peer still in signaling phase: {id}");
                        state.handle.abort();
                    }
                }
            }
            signaling::Event::PeerJoined { session_id } => {
                if let Some(existing) = peers.get(&session_id) {
                    if existing.established.load(Ordering::Relaxed) {
                        eprintln!("ignoring duplicate peer_joined for established peer: {session_id}");
                        continue;
                    }
                    if let Some(old) = peers.remove(&session_id) {
                        old.handle.abort();
                    }
                }
                eprintln!("consumer joined: {session_id}");
                let (peer_sig_tx, peer_sig_rx) = mpsc::unbounded_channel();
                let established = Arc::new(AtomicBool::new(false));
                let peer_id = session_id.clone();
                let sock = config.sock_path.clone();
                let out_tx = sig_send_tx.clone();
                let key = signing_key.clone();
                let est = established.clone();
                let ice = ice_config.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) =
                        peer::handle_peer(peer_id.clone(), sock, peer_sig_rx, out_tx, key, est, ice).await
                    {
                        eprintln!("peer {peer_id} error: {e}");
                    }
                });
                peers.insert(
                    session_id,
                    PeerState {
                        handle,
                        signal_tx: peer_sig_tx,
                        established,
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
