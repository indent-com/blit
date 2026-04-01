macro_rules! verbose {
    ($($arg:tt)*) => {
        if $crate::VERBOSE.load(::std::sync::atomic::Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}

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

pub static VERBOSE: AtomicBool = AtomicBool::new(false);

pub const DEFAULT_HUB_URL: &str = "hub.blit.sh";

/// Resolve the machine's default local IP (the one the OS would route outbound traffic from).
/// Returns `None` if no route exists. No packets are sent.
pub fn default_local_ip() -> Option<std::net::IpAddr> {
    let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect("192.0.2.1:80").ok()?;
    Some(probe.local_addr().ok()?.ip())
}
const DEFAULT_MESSAGE_TEMPLATE: &str = "https://blit.sh/#{secret}";

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
    pub message_override: Option<String>,
    pub quiet: bool,
    pub verbose: bool,
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

struct Message {
    template: String,
    fatal: bool,
}

async fn fetch_message(signal_url_base: &str) -> Option<Message> {
    let base = signal_url_base
        .trim_end_matches('/')
        .replace("wss://", "https://")
        .replace("ws://", "http://");
    let url = format!("{base}/message");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", format!("blit/{}", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let template = body.get("template")?.as_str()?.to_string();
    let fatal = body.get("fatal").and_then(|v| v.as_bool()).unwrap_or(false);
    Some(Message { template, fatal })
}

pub async fn run(config: Config) {
    VERBOSE.store(config.verbose, Ordering::Relaxed);
    let signing_key = derive_signing_key(&config.passphrase);
    let public_key_hex = hex_encode(signing_key.verifying_key().as_bytes());

    if !config.quiet {
        let (template, fatal) = match &config.message_override {
            Some(t) => (t.clone(), false),
            None => match fetch_message(&config.signal_url).await {
                Some(msg) => (msg.template, msg.fatal),
                None => (DEFAULT_MESSAGE_TEMPLATE.to_string(), false),
            },
        };
        let rendered = template.replace("{secret}", &config.passphrase);
        println!("{rendered}");
        if fatal {
            std::process::exit(1);
        }
    }

    let ice_config = match ice::fetch_ice_config(&config.signal_url).await {
        Ok(cfg) => {
            verbose!("fetched ICE config ({} servers)", cfg.ice_servers.len());
            Some(cfg)
        }
        Err(e) => {
            verbose!("failed to fetch ICE config: {e}");
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
                verbose!("registered with signaling server (session {session_id})");
                let stale: Vec<String> = peers
                    .iter()
                    .filter(|(_, s)| !s.established.load(Ordering::Relaxed))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in stale {
                    if let Some(state) = peers.remove(&id) {
                        verbose!("aborting peer still in signaling phase: {id}");
                        state.handle.abort();
                    }
                }
            }
            signaling::Event::PeerJoined { session_id } => {
                if let Some(existing) = peers.get(&session_id) {
                    if existing.established.load(Ordering::Relaxed) {
                        verbose!("ignoring duplicate peer_joined for established peer: {session_id}");
                        continue;
                    }
                    if let Some(old) = peers.remove(&session_id) {
                        old.handle.abort();
                    }
                }
                verbose!("consumer joined: {session_id}");
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
                        verbose!("peer {peer_id} error: {e}");
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
                verbose!("consumer left: {session_id}");
                if let Some(state) = peers.remove(&session_id) {
                    state.handle.abort();
                }
            }
            signaling::Event::Signal { from, data } => {
                if let Some(state) = peers.get(&from) {
                    let _ = state.signal_tx.send(data);
                } else {
                    verbose!("signal from unknown peer {from}, ignoring");
                }
            }
            signaling::Event::Error { message } => {
                verbose!("signaling error: {message}");
            }
        }
    }
}
