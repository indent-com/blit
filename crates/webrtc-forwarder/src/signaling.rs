use base64::Engine;
use crypto_box::SalsaBox;
use crypto_box::aead::{Aead, AeadCore, OsRng};
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::BoxKeys;

#[derive(Debug)]
pub enum Event {
    Registered {
        session_id: String,
    },
    PeerJoined {
        session_id: String,
    },
    PeerLeft {
        session_id: String,
    },
    Signal {
        from: String,
        data: serde_json::Value,
    },
    Error {
        message: String,
    },
}

#[derive(Deserialize)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    from: Option<String>,
    data: Option<serde_json::Value>,
    message: Option<String>,
}

#[derive(Serialize)]
struct ClientMessage {
    signed: String,
    target: String,
}

pub fn sign_payload(key: &SigningKey, payload: &[u8]) -> String {
    let sig = key.sign(payload);
    let mut envelope = Vec::with_capacity(64 + payload.len());
    envelope.extend_from_slice(&sig.to_bytes());
    envelope.extend_from_slice(payload);
    base64::engine::general_purpose::STANDARD.encode(&envelope)
}

/// Build a signed message with the `data` encrypted inside a NaCl crypto_box.
/// The signed inner payload is `{"box":"<base64(nonce||ciphertext)>"}` so the
/// hub can still parse it as JSON while the actual SDP/ICE data is opaque.
pub fn build_sealed_message(
    signing_key: &SigningKey,
    target: &str,
    data: &serde_json::Value,
    box_keys: &BoxKeys,
) -> String {
    let plaintext = serde_json::to_vec(data).unwrap();
    let salsa = SalsaBox::new(&box_keys.their_public, &box_keys.our_secret);
    let nonce = SalsaBox::generate_nonce(&mut OsRng);
    let ciphertext = salsa.encrypt(&nonce, plaintext.as_ref()).expect("encrypt");
    let mut sealed = Vec::with_capacity(24 + ciphertext.len());
    sealed.extend_from_slice(&nonce);
    sealed.extend_from_slice(&ciphertext);
    let sealed_b64 = base64::engine::general_purpose::STANDARD.encode(&sealed);
    // The inner payload the hub sees after signature verification:
    let inner = serde_json::json!({ "box": sealed_b64 });
    let inner_bytes = serde_json::to_vec(&inner).unwrap();
    let signed = sign_payload(signing_key, &inner_bytes);
    serde_json::to_string(&ClientMessage {
        signed,
        target: target.to_owned(),
    })
    .unwrap()
}

/// Legacy unencrypted signed message (for peers without crypto_box keys).
pub fn build_signed_message(key: &SigningKey, target: &str, data: &serde_json::Value) -> String {
    let payload = serde_json::to_vec(data).unwrap();
    let signed = sign_payload(key, &payload);
    serde_json::to_string(&ClientMessage {
        signed,
        target: target.to_owned(),
    })
    .unwrap()
}

/// Try to open a NaCl crypto_box sealed payload. Returns the decrypted JSON
/// value, or `None` if the data doesn't contain a `"box"` field or decryption
/// fails.
pub fn open_sealed_data(data: &serde_json::Value, box_keys: &BoxKeys) -> Option<serde_json::Value> {
    let sealed_b64 = data.get("box")?.as_str()?;
    let sealed = base64::engine::general_purpose::STANDARD
        .decode(sealed_b64)
        .ok()?;
    if sealed.len() < 24 {
        return None;
    }
    let nonce: &crypto_box::Nonce = (&sealed[..24]).into();
    let ciphertext = &sealed[24..];
    let salsa = SalsaBox::new(&box_keys.their_public, &box_keys.our_secret);
    let plaintext = salsa.decrypt(nonce, ciphertext).ok()?;
    serde_json::from_slice(&plaintext).ok()
}

pub async fn connect(
    url: String,
    key: SigningKey,
    box_keys: Option<BoxKeys>,
    event_tx: mpsc::UnboundedSender<Event>,
    mut outgoing_rx: mpsc::UnboundedReceiver<String>,
) {
    loop {
        match try_connect(&url, &key, box_keys.as_ref(), &event_tx, &mut outgoing_rx).await {
            Ok(()) => {
                verbose!("signaling connection closed, reconnecting...");
            }
            Err(e) => {
                verbose!("signaling connection error: {e}, reconnecting...");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn try_connect(
    url: &str,
    _key: &SigningKey,
    box_keys: Option<&BoxKeys>,
    tx: &mpsc::UnboundedSender<Event>,
    outgoing_rx: &mut mpsc::UnboundedReceiver<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    let (mut write, mut read) = ws.split();

    loop {
        tokio::select! {
            msg = read.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                };
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Ping(_) | Message::Pong(_) => continue,
                    Message::Close(_) => break,
                    _ => continue,
                };

                let parsed: ServerMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                let event = match parsed.msg_type.as_str() {
                    "registered" => Event::Registered {
                        session_id: parsed.session_id.unwrap_or_default(),
                    },
                    "peer_joined" => Event::PeerJoined {
                        session_id: parsed.session_id.unwrap_or_default(),
                    },
                    "peer_left" => Event::PeerLeft {
                        session_id: parsed.session_id.unwrap_or_default(),
                    },
                    "signal" => {
                        let raw = parsed.data.unwrap_or(serde_json::Value::Null);
                        // Try to open a crypto_box sealed payload; fall back
                        // to plaintext for legacy peers.
                        let data = box_keys
                            .and_then(|bk| open_sealed_data(&raw, bk))
                            .unwrap_or(raw);
                        Event::Signal {
                            from: parsed.from.unwrap_or_default(),
                            data,
                        }
                    },
                    "error" => Event::Error {
                        message: parsed.message.unwrap_or_default(),
                    },
                    _ => continue,
                };

                if tx.send(event).is_err() {
                    break;
                }
            }
            msg = outgoing_rx.recv() => {
                match msg {
                    Some(text) => {
                        write.send(Message::Text(text.into())).await?;
                    }
                    None => break,
                }
            }
        }
    }

    Ok(())
}
