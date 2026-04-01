use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

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

pub fn build_signed_message(key: &SigningKey, target: &str, data: &serde_json::Value) -> String {
    let payload = serde_json::to_vec(data).unwrap();
    let signed = sign_payload(key, &payload);
    serde_json::to_string(&ClientMessage {
        signed,
        target: target.to_owned(),
    })
    .unwrap()
}

pub async fn connect(
    url: String,
    key: SigningKey,
    event_tx: mpsc::UnboundedSender<Event>,
    mut outgoing_rx: mpsc::UnboundedReceiver<String>,
) {
    loop {
        match try_connect(&url, &key, &event_tx, &mut outgoing_rx).await {
            Ok(()) => {
                eprintln!("signaling connection closed, reconnecting...");
            }
            Err(e) => {
                eprintln!("signaling connection error: {e}, reconnecting...");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

async fn try_connect(
    url: &str,
    _key: &SigningKey,
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
                    "signal" => Event::Signal {
                        from: parsed.from.unwrap_or_default(),
                        data: parsed.data.unwrap_or(serde_json::Value::Null),
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
