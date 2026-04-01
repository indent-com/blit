use crate::ice::{self, IceConfig, Transport};
use crate::signaling;
use crate::turn::{self, TurnRelay};
use ed25519_dalek::SigningKey;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::Receive;
use str0m::{Candidate, Event, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

pub async fn handle_peer(
    peer_session_id: String,
    sock_path: String,
    mut signal_rx: mpsc::UnboundedReceiver<serde_json::Value>,
    signal_tx: mpsc::UnboundedSender<String>,
    signing_key: SigningKey,
    established: Arc<AtomicBool>,
    ice_config: Option<IceConfig>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            match turn::stun_binding(*stun_addr, &tokio_udp).await {
                Ok(srflx) => {
                    rtc.add_local_candidate(
                        Candidate::server_reflexive(srflx, local_addr, "udp")
                            .expect("valid candidate"),
                    );
                    break;
                }
                Err(e) => eprintln!("STUN binding failed: {e}"),
            }
        }

        for ts in &turn_servers {
            let result = match ts.transport {
                Transport::Udp => {
                    TurnRelay::allocate_udp(ts.addr, &ts.username, &ts.credential).await
                }
                Transport::Tcp => {
                    TurnRelay::allocate_tcp(
                        ts.addr, ts.tls, &ts.hostname, &ts.username, &ts.credential,
                    )
                    .await
                }
            };
            match result {
                Ok(r) => {
                    rtc.add_local_candidate(
                        Candidate::relayed(r.relay_addr, local_addr, "udp")
                            .expect("valid candidate"),
                    );
                    relay = Some(r);
                    break;
                }
                Err(e) => eprintln!("TURN allocate ({:?}) failed: {e}", ts.transport),
            }
        }
    }

    let offer: SdpOffer = loop {
        match signal_rx.recv().await {
            Some(data) => {
                if let Some(sdp) = data.get("sdp") {
                    let offer: SdpOffer = serde_json::from_value(sdp.clone())?;
                    break offer;
                }
            }
            None => return Ok(()),
        }
    };

    let answer = rtc.sdp_api().accept_offer(offer)?;
    let answer_json = serde_json::to_value(&answer)?;
    let signal_data = serde_json::json!({ "sdp": answer_json });
    let msg = signaling::build_signed_message(&signing_key, &peer_session_id, &signal_data);
    signal_tx.send(msg).map_err(|e| format!("send failed: {e}"))?;

    let mut blit_conn: Option<UnixStream> = None;
    let mut blit_channel: Option<ChannelId> = None;
    let mut buf = vec![0u8; 65535];
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
                            eprintln!("data channel opened: {label}");
                            if label == "blit" {
                                blit_channel = Some(cid);
                                let stream = UnixStream::connect(&sock_path).await?;
                                blit_conn = Some(stream);
                                established.store(true, Ordering::Relaxed);
                            }
                        }
                        Event::ChannelData(cd) => {
                            if Some(cd.id) == blit_channel {
                                if let Some(conn) = &mut blit_conn {
                                    let data = &cd.data;
                                    if data.len() < 4 {
                                        continue;
                                    }
                                    let len = u32::from_le_bytes([
                                        data[0], data[1], data[2], data[3],
                                    ]) as usize;
                                    if data.len() < 4 + len {
                                        continue;
                                    }
                                    let payload = &data[4..4 + len];
                                    let frame_len = (payload.len() as u32).to_le_bytes();
                                    if conn.write_all(&frame_len).await.is_err() {
                                        eprintln!("blit-server socket write failed");
                                        return Ok(());
                                    }
                                    if !payload.is_empty()
                                        && conn.write_all(payload).await.is_err()
                                    {
                                        eprintln!("blit-server socket write failed");
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        Event::ChannelClose(cid) => {
                            if Some(cid) == blit_channel {
                                eprintln!("blit data channel closed");
                                return Ok(());
                            }
                        }
                        Event::IceConnectionStateChange(state) => {
                            eprintln!("ICE state: {state:?}");
                            if matches!(
                                state,
                                str0m::IceConnectionState::Disconnected
                            ) {
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
                if let Some(conn) = &mut blit_conn {
                    let mut len_buf = [0u8; 4];
                    conn.read_exact(&mut len_buf).await.map(|_| {
                        u32::from_le_bytes(len_buf) as usize
                    })
                } else {
                    std::future::pending::<Result<usize, std::io::Error>>().await
                }
            } => {
                match result {
                    Ok(len) if len <= MAX_FRAME_SIZE => {
                        if let Some(conn) = &mut blit_conn {
                            let mut payload = vec![0u8; len];
                            if len > 0 {
                                conn.read_exact(&mut payload).await?;
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
                    }
                    _ => {
                        eprintln!("blit-server connection closed");
                        return Ok(());
                    }
                }
            }
            sig = async {
                if signaling_alive {
                    signal_rx.recv().await
                } else {
                    std::future::pending::<Option<serde_json::Value>>().await
                }
            } => {
                match sig {
                    Some(data) => {
                        if let Some(candidate) = data.get("candidate") {
                            if let Ok(c) = serde_json::from_value::<Candidate>(candidate.clone()) {
                                rtc.add_remote_candidate(c);
                            }
                        }
                    }
                    None => {
                        signaling_alive = false;
                        if !established.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        eprintln!("signaling channel closed, WebRTC connection continues");
                    }
                }
            }
        }
    }
}
