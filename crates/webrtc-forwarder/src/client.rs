use crate::ice;
use crate::signaling;
use crate::turn::{self, TurnRelay};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::net::UdpSocket;
use std::time::Instant;
use str0m::channel::ChannelId;
use str0m::net::Receive;
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[allow(dead_code)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    from: Option<String>,
    data: Option<serde_json::Value>,
    message: Option<String>,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub async fn connect(
    passphrase: &str,
    signal_url: &str,
) -> Result<tokio::io::DuplexStream, BoxError> {
    let signing_key = crate::derive_signing_key(passphrase);
    let public_key_hex = crate::hex_encode(signing_key.verifying_key().as_bytes());

    let ice_config = ice::fetch_ice_config(signal_url).await.ok();

    let ws_url = format!(
        "{}/channel/{}/consumer",
        signal_url.trim_end_matches('/'),
        public_key_hex,
    );
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url).await?;
    let (mut ws_write, mut ws_read) = ws.split();

    let _my_session_id = loop {
        let msg = ws_read
            .next()
            .await
            .ok_or("signaling closed before registration")??;
        if let Message::Text(t) = msg {
            if let Ok(m) = serde_json::from_str::<ServerMessage>(&t) {
                if m.msg_type == "registered" {
                    break m.session_id.unwrap_or_default();
                }
                if m.msg_type == "error" {
                    return Err(format!("signaling: {}", m.message.unwrap_or_default()).into());
                }
            }
        }
    };

    let forwarder_session_id = loop {
        let msg = ws_read
            .next()
            .await
            .ok_or("signaling closed before peer joined")??;
        if let Message::Text(t) = msg {
            if let Ok(m) = serde_json::from_str::<ServerMessage>(&t) {
                if m.msg_type == "peer_joined" {
                    break m.session_id.unwrap_or_default();
                }
                if m.msg_type == "error" {
                    return Err(format!("signaling: {}", m.message.unwrap_or_default()).into());
                }
            }
        }
    };

    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.set_nonblocking(true)?;
    let local_addr = udp.local_addr()?;
    let tokio_udp = tokio::net::UdpSocket::from_std(udp)?;

    let mut rtc = Rtc::new(Instant::now());

    if let Some(ip) = crate::default_local_ip() {
        let host_addr = std::net::SocketAddr::new(ip, local_addr.port());
        if let Ok(c) = Candidate::host(host_addr, "udp") {
            rtc.add_local_candidate(c);
        }
    }

    let mut relay: Option<TurnRelay> = None;

    if let Some(config) = &ice_config {
        let (stun_servers, turn_servers) = ice::collect_servers(config);

        for stun_addr in stun_servers.iter().take(1) {
            if let Ok(srflx) = turn::stun_binding(*stun_addr, &tokio_udp).await {
                rtc.add_local_candidate(
                    Candidate::server_reflexive(srflx, local_addr, "udp")
                        .expect("valid candidate"),
                );
                break;
            }
        }

        for ts in &turn_servers {
            let result = match ts.transport {
                ice::Transport::Udp => {
                    TurnRelay::allocate_udp(ts.addr, &ts.username, &ts.credential).await
                }
                ice::Transport::Tcp => {
                    TurnRelay::allocate_tcp(
                        ts.addr, ts.tls, &ts.hostname, &ts.username, &ts.credential,
                    )
                    .await
                }
            };
            if let Ok(r) = result {
                rtc.add_local_candidate(
                    Candidate::relayed(r.relay_addr, local_addr, "udp")
                        .expect("valid candidate"),
                );
                relay = Some(r);
                break;
            }
        }
    }

    let mut changes = rtc.sdp_api();
    let _channel_id = changes.add_channel("blit".to_string());
    let (offer, pending) = changes.apply().unwrap();

    let offer_json = serde_json::to_value(&offer)?;
    let signal_data = serde_json::json!({ "sdp": offer_json });
    let msg = signaling::build_signed_message(&signing_key, &forwarder_session_id, &signal_data);
    ws_write.send(Message::Text(msg.into())).await?;

    let mut answer_pending = Some(pending);
    let mut signal_rx_buf: Vec<serde_json::Value> = Vec::new();

    loop {
        let msg = ws_read
            .next()
            .await
            .ok_or("signaling closed before answer")??;
        if let Message::Text(t) = msg {
            if let Ok(m) = serde_json::from_str::<ServerMessage>(&t) {
                if m.msg_type == "signal" {
                    if let Some(data) = m.data {
                        if let Some(sdp) = data.get("sdp") {
                            let answer = serde_json::from_value(sdp.clone())?;
                            if let Some(p) = answer_pending.take() {
                                rtc.sdp_api().accept_answer(p, answer)?;
                            }
                        } else {
                            signal_rx_buf.push(data);
                        }
                        if answer_pending.is_none() {
                            break;
                        }
                    }
                }
            }
        }
    }

    for data in signal_rx_buf.drain(..) {
        if let Some(candidate) = data.get("candidate") {
            if let Ok(c) = serde_json::from_value::<Candidate>(candidate.clone()) {
                rtc.add_remote_candidate(c);
            }
        }
    }

    let (app_half, driver_half) = tokio::io::duplex(256 * 1024);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();

    tokio::spawn(async move {
        if let Err(e) = drive(
            rtc,
            tokio_udp,
            local_addr,
            relay,
            driver_half,
            ws_read,
            ws_write,
            ready_tx,
        )
        .await
        {
            eprintln!("webrtc client error: {e}");
        }
    });

    ready_rx.await.map_err(|_| "driver task died")??;

    Ok(app_half)
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    mut rtc: Rtc,
    tokio_udp: tokio::net::UdpSocket,
    local_addr: std::net::SocketAddr,
    mut relay: Option<TurnRelay>,
    driver_half: tokio::io::DuplexStream,
    mut ws_read: futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    >,
    mut _ws_write: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
) -> Result<(), BoxError> {
    let (mut drv_read, mut drv_write) = tokio::io::split(driver_half);
    let mut buf = vec![0u8; 65535];
    let mut blit_channel: Option<ChannelId> = None;
    let mut ready = Some(ready_tx);
    let mut signaling_alive = true;
    let relay_addr = relay.as_ref().map(|r| r.relay_addr);

    loop {
        let timeout = loop {
            match rtc.poll_output()? {
                Output::Timeout(v) => break v,
                Output::Transmit(t) => {
                    if relay_addr == Some(t.source) {
                        if let Some(r) = &relay {
                            let _ = r.send_tx.send((t.destination, t.contents.to_vec()));
                        }
                    } else {
                        tokio_udp.send_to(&t.contents, t.destination).await?;
                    }
                    continue;
                }
                Output::Event(ev) => {
                    match ev {
                        Event::ChannelOpen(cid, label) => {
                            if label == "blit" {
                                blit_channel = Some(cid);
                                if let Some(tx) = ready.take() {
                                    let _ = tx.send(Ok(()));
                                }
                            }
                        }
                        Event::ChannelData(cd) => {
                            if Some(cd.id) == blit_channel {
                                drv_write.write_all(&cd.data).await?;
                            }
                        }
                        Event::ChannelClose(cid) => {
                            if Some(cid) == blit_channel {
                                return Ok(());
                            }
                        }
                        Event::IceConnectionStateChange(state) => {
                            if matches!(state, str0m::IceConnectionState::Disconnected) {
                                if let Some(tx) = ready.take() {
                                    let _ = tx.send(Err("ICE disconnected".into()));
                                }
                                return Ok(());
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
            }
        };

        let sleep_dur = timeout.saturating_duration_since(Instant::now());

        tokio::select! {
            result = tokio_udp.recv_from(&mut buf) => {
                let (n, source) = result?;
                if let Ok(receive) = Receive::new(
                    str0m::net::Protocol::Udp,
                    source,
                    local_addr,
                    &buf[..n],
                ) {
                    rtc.handle_input(Input::Receive(Instant::now(), receive))?;
                }
            }
            _ = tokio::time::sleep(sleep_dur) => {
                rtc.handle_input(Input::Timeout(Instant::now()))?;
            }
            turn_data = async {
                if let Some(r) = &mut relay {
                    r.recv_rx.recv().await
                } else {
                    std::future::pending::<Option<(std::net::SocketAddr, Vec<u8>)>>().await
                }
            } => {
                if let Some((peer_addr, data)) = turn_data {
                    if let Some(ra) = relay_addr {
                        if let Ok(receive) = Receive::new(
                            str0m::net::Protocol::Udp,
                            peer_addr,
                            ra,
                            &data,
                        ) {
                            rtc.handle_input(Input::Receive(Instant::now(), receive))?;
                        }
                    }
                }
            }
            result = async {
                if blit_channel.is_some() {
                    let mut len_buf = [0u8; 4];
                    drv_read.read_exact(&mut len_buf).await.map(|_| {
                        u32::from_le_bytes(len_buf) as usize
                    })
                } else {
                    std::future::pending::<Result<usize, std::io::Error>>().await
                }
            } => {
                match result {
                    Ok(len) if len <= MAX_FRAME_SIZE => {
                        let mut payload = vec![0u8; len];
                        if len > 0 {
                            drv_read.read_exact(&mut payload).await?;
                        }
                        if let Some(cid) = blit_channel {
                            let frame_len = (len as u32).to_le_bytes();
                            let mut frame = Vec::with_capacity(4 + len);
                            frame.extend_from_slice(&frame_len);
                            frame.extend_from_slice(&payload);
                            if let Some(mut ch) = rtc.channel(cid) {
                                let _ = ch.write(true, &frame);
                            }
                        }
                    }
                    _ => return Ok(()),
                }
            }
            sig = async {
                if signaling_alive {
                    ws_read.next().await
                } else {
                    std::future::pending().await
                }
            } => {
                match sig {
                    Some(Ok(Message::Text(t))) => {
                        if let Ok(m) = serde_json::from_str::<ServerMessage>(&t) {
                            if m.msg_type == "signal" {
                                if let Some(data) = m.data {
                                    if let Some(candidate) = data.get("candidate") {
                                        if let Ok(c) = serde_json::from_value::<Candidate>(candidate.clone()) {
                                            rtc.add_remote_candidate(c);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    None | Some(Err(_)) => {
                        signaling_alive = false;
                        if ready.is_some() {
                            if let Some(tx) = ready.take() {
                                let _ = tx.send(Err("signaling lost before channel open".into()));
                            }
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}
